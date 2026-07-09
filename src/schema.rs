use anyhow::Result;
use postgres::Client;

// Tables are created in FK-dependency order (authors/works before
// authorships/editions). The sequences are created up front because the
// column defaults reference them by name. In the Rails database the tables
// already exist (as identity columns, whose backing sequences share these
// names), so everything here is a no-op there. Don't add ALTER SEQUENCE
// ... OWNED BY: it errors on identity sequences, and nothing needs the
// ownership link since imports never truncate.
const DDL: &str = "
CREATE SEQUENCE IF NOT EXISTS authors_id_seq;
CREATE SEQUENCE IF NOT EXISTS works_id_seq;
CREATE SEQUENCE IF NOT EXISTS authorships_id_seq;
CREATE SEQUENCE IF NOT EXISTS editions_id_seq;

CREATE TABLE IF NOT EXISTS authors (
    created_at timestamp NOT NULL,
    updated_at timestamp NOT NULL,
    id int8 NOT NULL DEFAULT nextval('authors_id_seq'::regclass),
    open_library_id TEXT,
    works_count int4 NOT NULL DEFAULT 0,
    name TEXT NOT NULL,
    bio TEXT,
    sort_name TEXT,
    birth_date TEXT,
    death_date TEXT,
    photo_id TEXT,
    slug TEXT NOT NULL,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS works (
    created_at timestamp NOT NULL,
    updated_at timestamp NOT NULL,
    id int8 NOT NULL DEFAULT nextval('works_id_seq'::regclass),
    open_library_id TEXT,
    edition_count int4 NOT NULL DEFAULT 0,
    title TEXT NOT NULL,
    subtitle TEXT,
    description TEXT,
    cover_id TEXT,
    first_publish_date TEXT,
    first_publish_year int4,
    subjects TEXT[] NOT NULL DEFAULT '{}',
    slug TEXT NOT NULL,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS authorships (
    created_at timestamp NOT NULL,
    updated_at timestamp NOT NULL,
    id int8 NOT NULL DEFAULT nextval('authorships_id_seq'::regclass),
    author_id int8 NOT NULL,
    work_id int8 NOT NULL,
    position int4 NOT NULL DEFAULT 0,
    CONSTRAINT works_fk FOREIGN KEY (work_id) REFERENCES works(id),
    CONSTRAINT authors_fk FOREIGN KEY (author_id) REFERENCES authors(id),
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS editions (
    created_at timestamp NOT NULL,
    updated_at timestamp NOT NULL,
    id int8 NOT NULL DEFAULT nextval('editions_id_seq'::regclass),
    open_library_id TEXT,
    work_id int8 NOT NULL,
    google_books_id TEXT,
    goodreads_id TEXT,
    asin TEXT,
    title TEXT,
    cover_id TEXT,
    format TEXT,
    isbn10 TEXT,
    isbn13 TEXT,
    language TEXT,
    page_count int4,
    published_year int4,
    publisher TEXT,
    subtitle TEXT,
    series TEXT,
    edition_name TEXT,
    internet_archive_id TEXT,
    slug TEXT NOT NULL,
    CONSTRAINT works_fk FOREIGN KEY (work_id) REFERENCES works(id),
    PRIMARY KEY (id)
);
";

struct Index {
    table: &'static str,
    name: &'static str,
    ddl: &'static str,
    /// Required indexes are the upserts' ON CONFLICT targets — they must
    /// always exist and are never dropped for bulk loads.
    required: bool,
}

/// The `name` must match the name in the DDL — it is what existence checks
/// and drops go by.
const INDEXES: &[Index] = &[
    Index {
        table: "authors",
        name: "index_authors_on_open_library_id",
        ddl: "CREATE UNIQUE INDEX index_authors_on_open_library_id ON public.authors USING btree (open_library_id)",
        required: true,
    },
    Index {
        table: "authors",
        name: "index_authors_on_slug",
        ddl: "CREATE UNIQUE INDEX index_authors_on_slug ON authors USING btree (slug)",
        required: false,
    },
    Index {
        table: "authorships",
        name: "index_authorships_on_author_id",
        ddl: "CREATE INDEX index_authorships_on_author_id ON authorships USING btree (author_id)",
        required: false,
    },
    Index {
        table: "authorships",
        name: "index_authorships_on_work_id_and_author_id",
        ddl: "CREATE UNIQUE INDEX index_authorships_on_work_id_and_author_id ON authorships USING btree (work_id, author_id)",
        required: true,
    },
    Index {
        table: "authorships",
        name: "index_authorships_on_work_id",
        ddl: "CREATE INDEX index_authorships_on_work_id ON authorships USING btree (work_id)",
        required: false,
    },
    Index {
        table: "editions",
        name: "index_editions_on_goodreads_id",
        ddl: "CREATE INDEX index_editions_on_goodreads_id ON editions USING btree (goodreads_id)",
        required: false,
    },
    Index {
        table: "editions",
        name: "index_editions_on_google_books_id",
        ddl: "CREATE INDEX index_editions_on_google_books_id ON editions USING btree (google_books_id)",
        required: false,
    },
    Index {
        table: "editions",
        name: "index_editions_on_isbn10",
        ddl: "CREATE INDEX index_editions_on_isbn10 ON editions USING btree (isbn10)",
        required: false,
    },
    Index {
        table: "editions",
        name: "index_editions_on_isbn13",
        ddl: "CREATE INDEX index_editions_on_isbn13 ON editions USING btree (isbn13)",
        required: false,
    },
    Index {
        table: "editions",
        name: "index_editions_on_open_library_id",
        ddl: "CREATE UNIQUE INDEX index_editions_on_open_library_id ON editions USING btree (open_library_id)",
        required: true,
    },
    Index {
        table: "editions",
        name: "index_editions_on_slug",
        ddl: "CREATE UNIQUE INDEX index_editions_on_slug ON editions USING btree (slug)",
        required: false,
    },
    Index {
        table: "editions",
        name: "index_editions_on_work_id",
        ddl: "CREATE INDEX index_editions_on_work_id ON editions USING btree (work_id)",
        required: false,
    },
    Index {
        table: "works",
        name: "index_works_on_first_publish_year",
        ddl: "CREATE INDEX index_works_on_first_publish_year ON works USING btree (first_publish_year)",
        required: false,
    },
    Index {
        table: "works",
        name: "index_works_on_open_library_id",
        ddl: "CREATE UNIQUE INDEX index_works_on_open_library_id ON works USING btree (open_library_id)",
        required: true,
    },
    Index {
        table: "works",
        name: "index_works_on_slug",
        ddl: "CREATE UNIQUE INDEX index_works_on_slug ON works USING btree (slug)",
        required: false,
    },
    Index {
        table: "works",
        name: "index_works_on_title",
        ddl: "CREATE INDEX index_works_on_title ON works USING btree (title)",
        required: false,
    },
];

pub fn create_tables(client: &mut Client) -> Result<()> {
    client.batch_execute(DDL)?;
    // The ON CONFLICT targets must exist before the first upsert.
    for index in INDEXES.iter().filter(|i| i.required) {
        create_if_missing(client, index)?;
    }
    Ok(())
}

fn index_exists(client: &mut Client, name: &str) -> Result<bool> {
    let row = client.query_one("SELECT to_regclass($1) IS NOT NULL", &[&name])?;
    Ok(row.get(0))
}

fn create_if_missing(client: &mut Client, index: &Index) -> Result<()> {
    if !index_exists(client, index.name)? {
        client.batch_execute(index.ddl)?;
    }
    Ok(())
}

/// Drop a table's secondary indexes so a first bulk load runs at full speed.
/// The unique ON CONFLICT indexes stay.
pub fn drop_secondary_indexes(client: &mut Client, table: &str) -> Result<()> {
    for index in INDEXES.iter().filter(|i| i.table == table && !i.required) {
        client.batch_execute(&format!("DROP INDEX IF EXISTS {}", index.name))?;
    }
    Ok(())
}

/// Create any of the table's indexes that don't exist yet.
pub fn create_indexes(client: &mut Client, table: &str) -> Result<()> {
    for index in INDEXES.iter().filter(|i| i.table == table) {
        create_if_missing(client, index)?;
    }
    Ok(())
}
