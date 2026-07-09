use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, NaiveDateTime, Utc};
use flate2::read::MultiGzDecoder;
use indicatif::{ProgressBar, ProgressStyle};
use postgres::binary_copy::BinaryCopyInWriter;
use postgres::types::Type;
use postgres::{Client, NoTls};

use crate::dump::DumpType;
use crate::model::{Author, Edition, Work, short_key};
use crate::schema;

/// How many editions `--dev` imports.
const DEV_LIMIT: u64 = 200_000;
/// `--dev` keeps editions published in this year or later.
const DEV_MIN_YEAR: i32 = 1950;

/// `works.title` has a btree index; clamp pathological titles so the index
/// build can't blow the btree entry size limit (~2704 bytes).
const MAX_TITLE_CHARS: usize = 1000;

/// Only editions in these languages are imported (MARC codes, matched against
/// the edition's first language — the one stored in `editions.language`).
/// Editions with no language set are dropped too.
const IMPORT_LANGUAGES: &[&str] = &["eng", "fre", "spa", "ita"];

// Everything is imported by upserting on open_library_id so that row ids stay
// stable across imports — the Rails app has tables (activities, …) holding
// foreign keys into these tables, which truncate-and-reload would break or
// cascade-wipe. Dumps are first COPY'd into unlogged staging tables (unlogged
// rather than temp so a second connection can see them), then merged with
// INSERT … ON CONFLICT. Rows that disappear from a newer dump are kept.

const STAGING_AUTHORS: &str = "
DROP TABLE IF EXISTS _import_authors_staging;
CREATE UNLOGGED TABLE _import_authors_staging (
    open_library_id TEXT NOT NULL,
    name TEXT NOT NULL,
    bio TEXT,
    birth_date TEXT,
    death_date TEXT,
    photo_id TEXT
)";

const STAGING_WORKS: &str = "
DROP TABLE IF EXISTS _import_works_staging;
CREATE UNLOGGED TABLE _import_works_staging (
    open_library_id TEXT NOT NULL,
    title TEXT NOT NULL,
    subtitle TEXT,
    description TEXT,
    first_publish_date TEXT,
    first_publish_year int4,
    cover_id TEXT,
    subjects TEXT[] NOT NULL
)";

const STAGING_EDITIONS: &str = "
DROP TABLE IF EXISTS _import_editions_staging;
CREATE UNLOGGED TABLE _import_editions_staging (
    work_olid TEXT NOT NULL,
    open_library_id TEXT NOT NULL,
    title TEXT,
    subtitle TEXT,
    asin TEXT,
    cover_id TEXT,
    format TEXT,
    goodreads_id TEXT,
    google_books_id TEXT,
    isbn10 TEXT,
    isbn13 TEXT,
    language TEXT,
    page_count int4,
    published_year int4,
    publisher TEXT,
    series TEXT,
    edition_name TEXT,
    internet_archive_id TEXT
)";

/// Work→author links captured while scanning the works dump.
const STAGING_REFS: &str = "
DROP TABLE IF EXISTS _import_work_author_refs;
CREATE UNLOGGED TABLE _import_work_author_refs (
    work_olid TEXT NOT NULL,
    author_olid TEXT NOT NULL,
    position int4 NOT NULL
)";

const UPSERT_AUTHORS: &str = "
INSERT INTO authors (open_library_id, name, bio, birth_date, death_date, photo_id, created_at, updated_at)
SELECT DISTINCT ON (open_library_id)
    open_library_id, name, bio, birth_date, death_date, photo_id, $1::timestamp, $1::timestamp
FROM _import_authors_staging
ON CONFLICT (open_library_id) DO UPDATE SET
    name = EXCLUDED.name,
    bio = EXCLUDED.bio,
    birth_date = EXCLUDED.birth_date,
    death_date = EXCLUDED.death_date,
    photo_id = EXCLUDED.photo_id,
    updated_at = EXCLUDED.updated_at";

const UPSERT_WORKS: &str = "
INSERT INTO works (open_library_id, title, subtitle, description, first_publish_date, first_publish_year, cover_id, subjects, created_at, updated_at)
SELECT DISTINCT ON (open_library_id)
    open_library_id, title, subtitle, description, first_publish_date, first_publish_year, cover_id, subjects, $1::timestamp, $1::timestamp
FROM _import_works_staging
ON CONFLICT (open_library_id) DO UPDATE SET
    title = EXCLUDED.title,
    subtitle = EXCLUDED.subtitle,
    description = EXCLUDED.description,
    first_publish_date = EXCLUDED.first_publish_date,
    first_publish_year = EXCLUDED.first_publish_year,
    cover_id = EXCLUDED.cover_id,
    subjects = EXCLUDED.subjects,
    updated_at = EXCLUDED.updated_at";

const UPSERT_AUTHORSHIPS: &str = "
INSERT INTO authorships (work_id, author_id, position, created_at, updated_at)
SELECT w.id, a.id, MIN(r.position), $1::timestamp, $1::timestamp
FROM _import_work_author_refs r
JOIN works w ON w.open_library_id = r.work_olid
JOIN authors a ON a.open_library_id = r.author_olid
GROUP BY w.id, a.id
ON CONFLICT (work_id, author_id) DO UPDATE SET
    position = EXCLUDED.position,
    updated_at = EXCLUDED.updated_at";

const UPSERT_EDITIONS: &str = "
INSERT INTO editions (open_library_id, title, subtitle, asin, cover_id, format,
    goodreads_id, google_books_id, isbn10, isbn13, language, page_count,
    published_year, publisher, series, edition_name, internet_archive_id,
    work_id, created_at, updated_at)
SELECT DISTINCT ON (s.open_library_id)
    s.open_library_id, s.title, s.subtitle, s.asin, s.cover_id, s.format,
    s.goodreads_id, s.google_books_id, s.isbn10, s.isbn13, s.language, s.page_count,
    s.published_year, s.publisher, s.series, s.edition_name, s.internet_archive_id,
    w.id, $1::timestamp, $1::timestamp
FROM _import_editions_staging s
JOIN works w ON w.open_library_id = s.work_olid
ON CONFLICT (open_library_id) DO UPDATE SET
    title = EXCLUDED.title,
    subtitle = EXCLUDED.subtitle,
    asin = EXCLUDED.asin,
    cover_id = EXCLUDED.cover_id,
    format = EXCLUDED.format,
    goodreads_id = EXCLUDED.goodreads_id,
    google_books_id = EXCLUDED.google_books_id,
    isbn10 = EXCLUDED.isbn10,
    isbn13 = EXCLUDED.isbn13,
    language = EXCLUDED.language,
    page_count = EXCLUDED.page_count,
    published_year = EXCLUDED.published_year,
    publisher = EXCLUDED.publisher,
    series = EXCLUDED.series,
    edition_name = EXCLUDED.edition_name,
    internet_archive_id = EXCLUDED.internet_archive_id,
    work_id = EXCLUDED.work_id,
    updated_at = EXCLUDED.updated_at";

pub fn run(dir: &Path, database_url: &str, only: Option<DumpType>, dev: bool) -> Result<()> {
    let mut client = connect(database_url)?;
    schema::create_tables(&mut client)?;
    let now = Utc::now().naive_utc();

    match only {
        None => import_all(&mut client, dir, database_url, now, dev),
        Some(DumpType::Editions) => only_editions(&mut client, dir, now, dev),
        Some(t) if dev => bail!(
            "--dev selects books by filtering editions, so it cannot be combined with `--only {}`; use `--only editions` or drop --only",
            t.name()
        ),
        Some(DumpType::Authors) => only_authors(&mut client, dir, now),
        Some(DumpType::Works) => only_works(&mut client, dir, database_url, now),
    }
}

fn connect(database_url: &str) -> Result<Client> {
    let mut client = Client::connect(database_url, NoTls).with_context(|| {
        format!(
            "failed to connect to {database_url} — is PostgreSQL running and the database created?"
        )
    })?;
    // Bulk-load tuning: crash-safety of individual commits doesn't matter here
    // (the import is restartable), and index builds want more memory.
    client.batch_execute("SET synchronous_commit = off; SET maintenance_work_mem = '1GB'")?;
    Ok(client)
}

/// Full (or --dev) import: authors, works, authorships, editions.
fn import_all(
    client: &mut Client,
    dir: &Path,
    database_url: &str,
    now: NaiveDateTime,
    dev: bool,
) -> Result<()> {
    if dev {
        println!(
            "Dev import: up to {DEV_LIMIT} editions published {DEV_MIN_YEAR}+ with covers, plus their works and authors"
        );
    }
    client.batch_execute(STAGING_AUTHORS)?;
    client.batch_execute(STAGING_WORKS)?;
    client.batch_execute(STAGING_EDITIONS)?;
    client.batch_execute(STAGING_REFS)?;

    // Dev picks its editions first; the set of works they reference then
    // drives which works (and transitively authors) get imported.
    let keep_works = if dev {
        let mut works = HashSet::new();
        stage_editions(client, dir, true, Some(&mut works))?;
        Some(works)
    } else {
        None
    };

    // The refs COPY runs on a second connection because the works COPY holds
    // the first one for the duration of the scan.
    let mut refs_client = connect(database_url)?;
    scan_works(client, &mut refs_client, dir, keep_works.as_ref())?;
    merge(client, "works", UPSERT_WORKS, now)?;

    let keep_authors = if dev {
        let rows = client.query(
            "SELECT DISTINCT author_olid FROM _import_work_author_refs",
            &[],
        )?;
        Some(rows.iter().map(|r| r.get(0)).collect::<HashSet<String>>())
    } else {
        None
    };
    scan_authors(client, dir, keep_authors.as_ref())?;
    merge(client, "authors", UPSERT_AUTHORS, now)?;
    merge(client, "authorships", UPSERT_AUTHORSHIPS, now)?;

    if !dev {
        stage_editions(client, dir, false, None)?;
    }
    merge(client, "editions", UPSERT_EDITIONS, now)?;

    recount_editions(client)?;
    recount_works(client)?;
    drop_staging(client);
    println!("Import complete.");
    Ok(())
}

/// Reimport editions against the works already in the database.
fn only_editions(client: &mut Client, dir: &Path, now: NaiveDateTime, dev: bool) -> Result<()> {
    let works: i64 = client.query_one("SELECT count(*) FROM works", &[])?.get(0);
    if works == 0 {
        bail!(
            "the works table is empty and editions link to works by id — run a full import first"
        );
    }
    client.batch_execute(STAGING_EDITIONS)?;
    stage_editions(client, dir, dev, None)?;
    merge(client, "editions", UPSERT_EDITIONS, now)?;
    recount_editions(client)?;
    drop_staging(client);
    println!("Import complete.");
    Ok(())
}

/// Reimport authors. Ids are stable, so authorships and counts are untouched.
fn only_authors(client: &mut Client, dir: &Path, now: NaiveDateTime) -> Result<()> {
    client.batch_execute(STAGING_AUTHORS)?;
    scan_authors(client, dir, None)?;
    merge(client, "authors", UPSERT_AUTHORS, now)?;
    drop_staging(client);
    println!("Import complete.");
    Ok(())
}

/// Reimport works and refresh authorships against the existing authors.
/// Editions keep pointing at the same work ids, so they are untouched.
fn only_works(
    client: &mut Client,
    dir: &Path,
    database_url: &str,
    now: NaiveDateTime,
) -> Result<()> {
    client.batch_execute(STAGING_WORKS)?;
    client.batch_execute(STAGING_REFS)?;
    let mut refs_client = connect(database_url)?;
    scan_works(client, &mut refs_client, dir, None)?;
    merge(client, "works", UPSERT_WORKS, now)?;
    merge(client, "authorships", UPSERT_AUTHORSHIPS, now)?;
    recount_works(client)?;
    drop_staging(client);
    println!("Import complete.");
    Ok(())
}

/// Run a table's upsert. If the table is empty (first import), its secondary
/// indexes are dropped for the load and rebuilt after; the unique indexes
/// stay — they're the ON CONFLICT targets.
fn merge(client: &mut Client, table: &str, upsert: &str, now: NaiveDateTime) -> Result<u64> {
    let empty: bool = client
        .query_one(&format!("SELECT NOT EXISTS (SELECT 1 FROM {table})"), &[])?
        .get(0);
    if empty {
        schema::drop_secondary_indexes(client, table)?;
    }
    let n = with_step(&format!("upserting {table}"), |_| {
        Ok(client.execute(upsert, &[&now])?)
    })?;
    with_step(&format!("indexing {table} ({n} rows upserted)"), |_| {
        schema::create_indexes(client, table)
    })?;
    Ok(n)
}

fn scan_authors(client: &mut Client, dir: &Path, keep: Option<&HashSet<String>>) -> Result<u64> {
    let (mut reader, bar) = open_dump(dir, DumpType::Authors)?;
    let sink = client.copy_in(
        "COPY _import_authors_staging (open_library_id, name, bio, birth_date, death_date, photo_id) FROM STDIN BINARY",
    )?;
    let mut writer = BinaryCopyInWriter::new(
        sink,
        &[
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
        ],
    );

    let mut line = String::new();
    let mut written: u64 = 0;
    let mut skipped: u64 = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Some((key, json)) = split_row(&line) else {
            skipped += 1;
            continue;
        };
        let olid = short_key(key);
        if let Some(keep) = keep
            && !keep.contains(olid)
        {
            continue;
        }
        let Ok(author) = serde_json::from_str::<Author>(json) else {
            skipped += 1;
            continue;
        };
        // name is NOT NULL in the schema; nameless author records are junk.
        let Some(name) = author.name.or(author.personal_name) else {
            skipped += 1;
            continue;
        };
        let photo_id = author.photos.first().map(i32::to_string);
        writer.write(&[
            &olid,
            &name,
            &author.bio,
            &author.birth_date,
            &author.death_date,
            &photo_id,
        ])?;
        written += 1;
        if written % 8192 == 0 {
            bar.set_message(format!("{written} rows"));
        }
    }
    writer.finish()?;
    finish_scan(&bar, written, skipped);
    Ok(written)
}

fn scan_works(
    client: &mut Client,
    refs_client: &mut Client,
    dir: &Path,
    keep: Option<&HashSet<String>>,
) -> Result<u64> {
    let (mut reader, bar) = open_dump(dir, DumpType::Works)?;
    let sink = client.copy_in(
        "COPY _import_works_staging (open_library_id, title, subtitle, description, first_publish_date, first_publish_year, cover_id, subjects) FROM STDIN BINARY",
    )?;
    let mut writer = BinaryCopyInWriter::new(
        sink,
        &[
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::INT4,
            Type::TEXT,
            Type::TEXT_ARRAY,
        ],
    );
    let refs_sink = refs_client.copy_in(
        "COPY _import_work_author_refs (work_olid, author_olid, position) FROM STDIN BINARY",
    )?;
    let mut refs_writer = BinaryCopyInWriter::new(refs_sink, &[Type::TEXT, Type::TEXT, Type::INT4]);

    let mut line = String::new();
    let mut written: u64 = 0;
    let mut skipped: u64 = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Some((key, json)) = split_row(&line) else {
            skipped += 1;
            continue;
        };
        let olid = short_key(key);
        if let Some(keep) = keep
            && !keep.contains(olid)
        {
            continue;
        }
        let Ok(work) = serde_json::from_str::<Work>(json) else {
            skipped += 1;
            continue;
        };
        // title is NOT NULL in the schema; untitled works are unusable anyway.
        let Some(title) = work.title.map(|t| clamp_chars(t, MAX_TITLE_CHARS)) else {
            skipped += 1;
            continue;
        };
        let first_publish_year = work
            .first_publish_date
            .as_deref()
            .and_then(crate::model::extract_year);
        let cover_id = work.covers.first().map(i32::to_string);
        writer.write(&[
            &olid,
            &title,
            &work.subtitle,
            &work.description,
            &work.first_publish_date,
            &first_publish_year,
            &cover_id,
            &work.subjects,
        ])?;
        for (position, author_olid) in work.author_keys.iter().enumerate() {
            refs_writer.write(&[&olid, author_olid, &(position as i32)])?;
        }
        written += 1;
        if written % 8192 == 0 {
            bar.set_message(format!("{written} rows"));
        }
    }
    writer.finish()?;
    refs_writer.finish()?;
    finish_scan(&bar, written, skipped);
    Ok(written)
}

/// Scan the editions dump into the staging table. With `dev`, only editions
/// meeting the quality bar are kept, capped at DEV_LIMIT; `collect_works`
/// receives the open_library_ids of the works they reference.
fn stage_editions(
    client: &mut Client,
    dir: &Path,
    dev: bool,
    mut collect_works: Option<&mut HashSet<String>>,
) -> Result<u64> {
    let (mut reader, bar) = open_dump(dir, DumpType::Editions)?;
    let max_year = Utc::now().year() + 1;
    let sink = client.copy_in(
        "COPY _import_editions_staging (work_olid, open_library_id, title, subtitle, asin, cover_id, format, goodreads_id, google_books_id, isbn10, isbn13, language, page_count, published_year, publisher, series, edition_name, internet_archive_id) FROM STDIN BINARY",
    )?;
    let mut writer = BinaryCopyInWriter::new(
        sink,
        &[
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::INT4,
            Type::INT4,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
            Type::TEXT,
        ],
    );

    let mut line = String::new();
    let mut staged: u64 = 0;
    let mut no_work: u64 = 0;
    let mut wrong_language: u64 = 0;
    let mut skipped: u64 = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let Some((key, json)) = split_row(&line) else {
            skipped += 1;
            continue;
        };
        let Ok(edition) = serde_json::from_str::<Edition>(json) else {
            skipped += 1;
            continue;
        };
        // work_id is NOT NULL in the schema — orphan editions can't be kept.
        let Some(work_olid) = edition.work_keys.first() else {
            no_work += 1;
            continue;
        };
        let language = edition.languages.first();
        if !matches!(language, Some(l) if IMPORT_LANGUAGES.contains(&l.as_str())) {
            wrong_language += 1;
            continue;
        }
        let year = edition.publish_year();
        if dev && !dev_quality(&edition, year, max_year) {
            continue;
        }
        let olid = short_key(key);
        let cover_id = edition.covers.first().map(i32::to_string);
        writer.write(&[
            &work_olid,
            &olid,
            &edition.title,
            &edition.subtitle,
            &edition.identifiers.amazon,
            &cover_id,
            &edition.physical_format,
            &edition.identifiers.goodreads,
            &edition.identifiers.google,
            &edition.isbn_10.first(),
            &edition.isbn_13.first(),
            &language,
            &edition.number_of_pages,
            &year,
            &edition.publishers.first(),
            &edition.series.first(),
            &edition.edition_name,
            &edition.ocaid,
        ])?;
        staged += 1;
        if staged % 8192 == 0 {
            bar.set_message(format!("{staged} staged"));
        }
        if let Some(works) = collect_works.as_deref_mut() {
            works.insert(work_olid.clone());
        }
        if dev && staged >= DEV_LIMIT {
            break;
        }
    }
    writer.finish()?;
    let mut extra = format!(
        "({no_work} without a work dropped, {wrong_language} not in {})",
        IMPORT_LANGUAGES.join("/")
    );
    if skipped > 0 {
        extra.push_str(&format!(" ({skipped} malformed lines skipped)"));
    }
    bar.set_message(format!("{staged} staged {extra}"));
    bar.finish();
    Ok(staged)
}

/// Refresh the `works.edition_count` counter cache, touching only rows whose
/// count is actually wrong.
fn recount_editions(client: &mut Client) -> Result<()> {
    with_step("updating edition counts", |_| {
        client.execute(
            "UPDATE works SET edition_count = c.n
             FROM (SELECT work_id, count(*)::int4 AS n FROM editions GROUP BY work_id) c
             WHERE works.id = c.work_id AND works.edition_count IS DISTINCT FROM c.n",
            &[],
        )?;
        client.execute(
            "UPDATE works SET edition_count = 0
             WHERE edition_count <> 0
               AND NOT EXISTS (SELECT 1 FROM editions e WHERE e.work_id = works.id)",
            &[],
        )?;
        Ok(())
    })
}

/// Refresh the `authors.works_count` counter cache.
fn recount_works(client: &mut Client) -> Result<()> {
    with_step("updating works counts", |_| {
        client.execute(
            "UPDATE authors SET works_count = c.n
             FROM (SELECT author_id, count(*)::int4 AS n FROM authorships GROUP BY author_id) c
             WHERE authors.id = c.author_id AND authors.works_count IS DISTINCT FROM c.n",
            &[],
        )?;
        client.execute(
            "UPDATE authors SET works_count = 0
             WHERE works_count <> 0
               AND NOT EXISTS (SELECT 1 FROM authorships s WHERE s.author_id = authors.id)",
            &[],
        )?;
        Ok(())
    })
}

fn drop_staging(client: &mut Client) {
    let _ = client.batch_execute(
        "DROP TABLE IF EXISTS _import_authors_staging;
         DROP TABLE IF EXISTS _import_works_staging;
         DROP TABLE IF EXISTS _import_editions_staging;
         DROP TABLE IF EXISTS _import_work_author_refs",
    );
}

/// The `--dev` quality bar: a titled book from {DEV_MIN_YEAR} or later with a
/// cover image. (A work reference is already required for all editions.)
fn dev_quality(e: &Edition, year: Option<i32>, max_year: i32) -> bool {
    e.title.is_some()
        && !e.covers.is_empty()
        && matches!(year, Some(y) if (DEV_MIN_YEAR..=max_year).contains(&y))
}

/// Dump lines are `type \t key \t revision \t last_modified \t json`. Tabs
/// inside the JSON are always escaped as `\t`, so a raw tab only ever
/// separates columns. Returns (key, json).
fn split_row(line: &str) -> Option<(&str, &str)> {
    let mut cols = line.trim_end_matches(['\n', '\r']).splitn(5, '\t');
    let _type = cols.next()?;
    let key = cols.next()?;
    let _revision = cols.next()?;
    let _last_modified = cols.next()?;
    let json = cols.next()?;
    Some((key, json))
}

fn clamp_chars(s: String, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
        None => s,
    }
}

/// Find the dump file for this type: `ol_dump_<type>_latest.txt.gz` or a
/// dated `ol_dump_<type>_YYYY-MM-DD.txt.gz` from `download --archive`. If
/// several are present, the most recently modified wins.
fn find_dump(dir: &Path, dump: DumpType) -> Result<PathBuf> {
    let prefix = format!("ol_dump_{}_", dump.name());
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(&prefix) || !name.ends_with(".txt.gz") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            if newest.as_ref().is_none_or(|(t, _)| modified > *t) {
                newest = Some((modified, entry.path()));
            }
        }
    }
    newest.map(|(_, path)| path).with_context(|| {
        format!(
            "no {prefix}*.txt.gz in {} — run `openlibrary download` first",
            dir.display()
        )
    })
}

fn open_dump(dir: &Path, dump: DumpType) -> Result<(impl BufRead, ProgressBar)> {
    let path = find_dump(dir, dump)?;
    let size = fs::metadata(&path)?.len();

    let bar = ProgressBar::new(size);
    bar.set_prefix(dump.name().to_string());
    // Show which file was picked until row counts take over the message.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        bar.set_message(name.to_string());
    }
    bar.set_style(
        ProgressStyle::with_template(
            "{prefix:>8} [{bar:40.cyan/blue}] {bytes}/{total_bytes} (eta {eta}) {msg}",
        )
        .unwrap()
        .progress_chars("=> "),
    );

    let file = File::open(&path)?;
    // Progress tracks compressed bytes consumed, so the bar reflects position
    // in the file on disk even though we parse the decompressed stream.
    let reader = BufReader::with_capacity(
        1 << 20,
        MultiGzDecoder::new(BufReader::with_capacity(1 << 20, bar.wrap_read(file))),
    );
    Ok((reader, bar))
}

fn finish_scan(bar: &ProgressBar, written: u64, skipped: u64) {
    let mut msg = format!("{written} rows");
    if skipped > 0 {
        msg.push_str(&format!(" ({skipped} skipped)"));
    }
    bar.set_message(msg);
    bar.finish();
}

/// Run a slow SQL step behind a spinner so the user sees what's happening.
fn with_step<T>(msg: &str, f: impl FnOnce(&ProgressBar) -> Result<T>) -> Result<T> {
    let bar = ProgressBar::new_spinner();
    bar.set_style(ProgressStyle::with_template("{spinner} {msg} [{elapsed}]").unwrap());
    bar.set_message(msg.to_string());
    bar.enable_steady_tick(Duration::from_millis(120));
    let result = f(&bar);
    bar.finish();
    result
}
