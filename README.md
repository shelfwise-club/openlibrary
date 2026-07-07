# openlibrary

CLI that downloads the [Open Library data dumps](https://openlibrary.org/developers/dumps) and imports them into PostgreSQL.

## Usage

```sh
# Download the latest authors/works/editions dumps into ./data (~20 GB total).
# Downloads run concurrently with progress bars, resume if interrupted, and
# skip files that are already complete. Use --force to re-download.
openlibrary download

# Download a single dump type (authors, editions, or works).
openlibrary download --only editions

# Download a specific dump date directly from archive.org instead — useful
# when openlibrary.org is down or you want a pinned dump. Files are saved
# under their dated names; import picks up whichever dump files are present
# (most recently modified per type wins).
openlibrary download --archive --date 2026-01-31

# Import everything into postgres (creates the tables; the database must exist).
createdb openlibrary
openlibrary import

# Reimport a single dump type: authors, editions, or works (see below for
# how relationships are handled).
openlibrary import --only authors

# Dev dataset: up to 200k good-quality editions (published 1950+, with cover
# images and a work reference), plus only the works and authors they reference.
openlibrary import --dev
```

The connection string comes from `--database-url` or `DATABASE_URL`, defaulting to `postgres://localhost:5432/shelfwise_development`.

## Schema

Rails-style tables: `authors`, `works`, `editions`, and an `authorships` join table, linked by bigint `id` foreign keys. Open Library keys are kept in `open_library_id` without their path prefix (`OL45883W`, not `/works/OL45883W`). Counter caches (`authors.works_count`, `works.edition_count`) are recomputed at the end of each import. Missing tables/sequences/indexes are created, so the importer works both against the Rails database and standalone.

Imports are **upserts keyed on `open_library_id`**: dumps are streamed via binary `COPY` into unlogged `_import_*` staging tables, then merged with `INSERT … ON CONFLICT DO UPDATE` (foreign keys resolved by joining on `open_library_id`). Row ids are therefore stable across imports — app tables referencing works/editions/authors (activities, shelvings, …) are never touched and never break. `created_at` is preserved on existing rows; `updated_at` is bumped. Two consequences of the merge model: records that disappear from a newer dump (including author↔work links) are kept, not deleted, and the first import into empty tables is the fast path (secondary indexes are dropped and rebuilt around it) while subsequent reimports maintain indexes in place and run slower.

Editions without a work reference (or whose work isn't in the database) are dropped, as are nameless authors and untitled works.

`--only` semantics:

- `--only editions` — rescans the editions dump against the works already in the database. Requires works to be imported first.
- `--only authors` — upserts authors; authorships and counts are untouched.
- `--only works` — upserts works and refreshes authorships against the existing authors. Editions keep their (stable) `work_id`s.

Records with malformed JSON are skipped and counted; inconsistent fields (string-or-object descriptions, string page counts, `-1` cover ids) are normalized during parsing. `published_year` is extracted from the free-form publish date, and external ids (`asin`, `goodreads_id`, `google_books_id`, `internet_archive_id`) come from the edition's `identifiers`/`ocaid` fields.
