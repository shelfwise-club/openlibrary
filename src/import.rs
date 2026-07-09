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
const DEV_LIMIT: u64 = 1_000_000;
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
    base_slug TEXT NOT NULL,
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
    base_slug TEXT NOT NULL,
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
    base_slug TEXT NOT NULL,
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

// The dumps carry far more than the app wants (works whose editions were all
// dropped, authors with no surviving books). Full/dev imports admit new rows
// via the staging-derived filters below; --only refreshes run the UPDATE half
// only, so a partial reimport can never re-bloat the database.
//
// Merging is split into UPDATE-existing + INSERT-new (instead of one ON
// CONFLICT upsert) because slugs demand it: an existing row's slug must never
// change (URLs), while a new row's slug needs deduplication — batch
// duplicates are ranked with ROW_NUMBER and offset by suffixes already taken
// in the table, then written as `base--N`. Slugified text can never contain
// `--`, so generated slugs can't collide with natural ones.

/// Full/dev: authors credited on at least one work that has a staged edition.
const AUTHORS_ON_KEPT_WORKS: &str = "AND open_library_id IN (
    SELECT r.author_olid
    FROM _import_work_author_refs r
    JOIN _import_editions_staging e ON e.work_olid = r.work_olid)";

/// Full/dev: works with at least one staged edition.
const WORKS_WITH_STAGED_EDITIONS: &str =
    "AND open_library_id IN (SELECT work_olid FROM _import_editions_staging)";

/// The slug-dedup scaffolding shared by every insert: rank new rows within
/// their base_slug, look up the highest suffix that base already has in the
/// live table, and emit `base` or `base--N`.
fn slugged_insert_sql(table: &str, filter: &str, columns: &str, values: &str, joins: &str) -> String {
    format!(
        "
INSERT INTO {table} ({columns}, slug, created_at, updated_at)
SELECT {values},
       CASE WHEN n.rn + COALESCE(o.taken, 0) = 1 THEN n.base_slug
            ELSE n.base_slug || '--' || (n.rn + COALESCE(o.taken, 0)) END,
       $1::timestamp, $1::timestamp
FROM (
    SELECT s.*, ROW_NUMBER() OVER (PARTITION BY s.base_slug ORDER BY s.open_library_id) AS rn
    FROM (
        SELECT DISTINCT ON (open_library_id) *
        FROM _import_{table}_staging
        WHERE open_library_id NOT IN (SELECT open_library_id FROM {table})
        {filter}
    ) s
) n
{joins}
LEFT JOIN LATERAL (
    SELECT max(CASE WHEN t.slug = n.base_slug THEN 1
                    ELSE (substring(t.slug FROM '([0-9]+)$'))::int END) AS taken
    FROM {table} t
    WHERE t.slug = n.base_slug
       OR (t.slug >= n.base_slug || '--' AND t.slug < n.base_slug || '-.'
           AND t.slug ~ ('^' || n.base_slug || '--[0-9]+$'))
) o ON true"
    )
}

fn insert_authors_sql(filter: &str) -> String {
    slugged_insert_sql(
        "authors",
        filter,
        "open_library_id, name, bio, birth_date, death_date, photo_id",
        "n.open_library_id, n.name, n.bio, n.birth_date, n.death_date, n.photo_id",
        "",
    )
}

const UPDATE_AUTHORS: &str = "
UPDATE authors a SET
    name = s.name,
    bio = s.bio,
    birth_date = s.birth_date,
    death_date = s.death_date,
    photo_id = s.photo_id,
    updated_at = $1::timestamp
FROM (SELECT DISTINCT ON (open_library_id) * FROM _import_authors_staging) s
WHERE a.open_library_id = s.open_library_id";

fn insert_works_sql(filter: &str) -> String {
    slugged_insert_sql(
        "works",
        filter,
        "open_library_id, title, subtitle, description, first_publish_date, first_publish_year, cover_id, subjects",
        "n.open_library_id, n.title, n.subtitle, n.description, n.first_publish_date, n.first_publish_year, n.cover_id, n.subjects",
        "",
    )
}

const UPDATE_WORKS: &str = "
UPDATE works w SET
    title = s.title,
    subtitle = s.subtitle,
    description = s.description,
    first_publish_date = s.first_publish_date,
    first_publish_year = s.first_publish_year,
    cover_id = s.cover_id,
    subjects = s.subjects,
    updated_at = $1::timestamp
FROM (SELECT DISTINCT ON (open_library_id) * FROM _import_works_staging) s
WHERE w.open_library_id = s.open_library_id";

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

fn insert_editions_sql() -> String {
    slugged_insert_sql(
        "editions",
        "",
        "open_library_id, title, subtitle, asin, cover_id, format, goodreads_id, google_books_id, isbn10, isbn13, language, page_count, published_year, publisher, series, edition_name, internet_archive_id, work_id",
        "n.open_library_id, n.title, n.subtitle, n.asin, n.cover_id, n.format, n.goodreads_id, n.google_books_id, n.isbn10, n.isbn13, n.language, n.page_count, n.published_year, n.publisher, n.series, n.edition_name, n.internet_archive_id, w.id",
        "JOIN works w ON w.open_library_id = n.work_olid",
    )
}

const UPDATE_EDITIONS: &str = "
UPDATE editions e SET
    title = s.title,
    subtitle = s.subtitle,
    asin = s.asin,
    cover_id = s.cover_id,
    format = s.format,
    goodreads_id = s.goodreads_id,
    google_books_id = s.google_books_id,
    isbn10 = s.isbn10,
    isbn13 = s.isbn13,
    language = s.language,
    page_count = s.page_count,
    published_year = s.published_year,
    publisher = s.publisher,
    series = s.series,
    edition_name = s.edition_name,
    internet_archive_id = s.internet_archive_id,
    work_id = w.id,
    updated_at = $1::timestamp
FROM (SELECT DISTINCT ON (open_library_id) * FROM _import_editions_staging) s
JOIN works w ON w.open_library_id = s.work_olid
WHERE e.open_library_id = s.open_library_id";

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

    // Editions are staged first: the surviving editions decide which works
    // get imported, and those works decide which authors — everything else
    // in the dumps is dead weight for the app.
    stage_editions(client, dir, dev)?;

    // The refs COPY runs on a second connection because the works COPY holds
    // the first one for the duration of the scan.
    let mut refs_client = connect(database_url)?;
    scan_works(client, &mut refs_client, dir)?;
    merge(client, "works", Some(UPDATE_WORKS), Some(&insert_works_sql(WORKS_WITH_STAGED_EDITIONS)), now)?;

    scan_authors(client, dir)?;
    merge(client, "authors", Some(UPDATE_AUTHORS), Some(&insert_authors_sql(AUTHORS_ON_KEPT_WORKS)), now)?;
    merge(client, "authorships", None, Some(UPSERT_AUTHORSHIPS), now)?;

    merge(client, "editions", Some(UPDATE_EDITIONS), Some(&insert_editions_sql()), now)?;

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
    stage_editions(client, dir, dev)?;
    merge(client, "editions", Some(UPDATE_EDITIONS), Some(&insert_editions_sql()), now)?;
    recount_editions(client)?;
    drop_staging(client);
    println!("Import complete.");
    Ok(())
}

/// Reimport authors. Ids are stable, so authorships and counts are untouched.
fn only_authors(client: &mut Client, dir: &Path, now: NaiveDateTime) -> Result<()> {
    client.batch_execute(STAGING_AUTHORS)?;
    scan_authors(client, dir)?;
    merge(client, "authors", Some(UPDATE_AUTHORS), None, now)?;
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
    scan_works(client, &mut refs_client, dir)?;
    merge(client, "works", Some(UPDATE_WORKS), None, now)?;
    merge(client, "authorships", None, Some(UPSERT_AUTHORSHIPS), now)?;
    recount_works(client)?;
    drop_staging(client);
    println!("Import complete.");
    Ok(())
}

/// Merge staged rows into a table: refresh existing rows (never touching
/// slug or created_at), insert new ones with freshly deduplicated slugs,
/// then ensure indexes. On a first load into an empty table the secondary
/// indexes are dropped for speed; the unique open_library_id indexes stay.
fn merge(
    client: &mut Client,
    table: &str,
    update: Option<&str>,
    insert: Option<&str>,
    now: NaiveDateTime,
) -> Result<()> {
    let empty: bool = client
        .query_one(&format!("SELECT NOT EXISTS (SELECT 1 FROM {table})"), &[])?
        .get(0);
    if empty {
        schema::drop_secondary_indexes(client, table)?;
    }
    let mut updated: u64 = 0;
    if let Some(sql) = update
        && !empty
    {
        updated = with_step(&format!("updating existing {table}"), |_| {
            Ok(client.execute(sql, &[&now])?)
        })?;
    }
    let mut inserted: u64 = 0;
    if let Some(sql) = insert {
        inserted = with_step(&format!("inserting new {table}"), |_| {
            Ok(client.execute(sql, &[&now])?)
        })?;
    }
    with_step(
        &format!("indexing {table} ({inserted} inserted, {updated} updated)"),
        |_| schema::create_indexes(client, table),
    )?;
    Ok(())
}

fn scan_authors(client: &mut Client, dir: &Path) -> Result<u64> {
    let (mut reader, bar) = open_dump(dir, DumpType::Authors)?;
    let sink = client.copy_in(
        "COPY _import_authors_staging (open_library_id, base_slug, name, bio, birth_date, death_date, photo_id) FROM STDIN BINARY",
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
        let base_slug = base_slug(&name, olid);
        writer.write(&[
            &olid,
            &base_slug,
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

fn scan_works(client: &mut Client, refs_client: &mut Client, dir: &Path) -> Result<u64> {
    let (mut reader, bar) = open_dump(dir, DumpType::Works)?;
    let sink = client.copy_in(
        "COPY _import_works_staging (open_library_id, base_slug, title, subtitle, description, first_publish_date, first_publish_year, cover_id, subjects) FROM STDIN BINARY",
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
        let base_slug = base_slug(&title, olid);
        writer.write(&[
            &olid,
            &base_slug,
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
/// meeting the quality bar are kept, capped at DEV_LIMIT. The staged rows
/// also decide which works and authors are worth importing.
fn stage_editions(client: &mut Client, dir: &Path, dev: bool) -> Result<u64> {
    let (mut reader, bar) = open_dump(dir, DumpType::Editions)?;
    let max_year = Utc::now().year() + 1;
    let sink = client.copy_in(
        "COPY _import_editions_staging (work_olid, open_library_id, base_slug, title, subtitle, asin, cover_id, format, goodreads_id, google_books_id, isbn10, isbn13, language, page_count, published_year, publisher, series, edition_name, internet_archive_id) FROM STDIN BINARY",
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
        let base_slug = edition_base_slug(&edition, olid);
        writer.write(&[
            &work_olid,
            &olid,
            &base_slug,
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

/// Slug base for authors (from the name) and works (from the title); the
/// record's open_library_id is the fallback when nothing slugifiable remains
/// (e.g. punctuation-only or fully non-transliterable input).
fn base_slug(text: &str, olid: &str) -> String {
    let slug = crate::model::slugify(text);
    if slug.is_empty() { olid.to_lowercase() } else { slug }
}

/// Edition slugs are `{isbn13|isbn10}-{slugged title}`, degrading to
/// whichever half exists, then to the open_library_id.
fn edition_base_slug(edition: &Edition, olid: &str) -> String {
    let isbn = edition.isbn_13.first().or(edition.isbn_10.first());
    let text = match (isbn, &edition.title) {
        (Some(isbn), Some(title)) => format!("{isbn} {title}"),
        (Some(isbn), None) => isbn.clone(),
        (None, Some(title)) => title.clone(),
        (None, None) => String::new(),
    };
    base_slug(&text, olid)
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
