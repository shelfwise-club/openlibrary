use anyhow::Result;
use postgres::Client;

// Tables are created in FK-dependency order (authors/works before
// authorships/editions). The sequences are created up front because the
// column defaults reference them by name; in the Rails database they already
// exist (bigserial), so everything here is a no-op there. The OWNED BY links
// let TRUNCATE ... RESTART IDENTITY reset the sequences on fresh databases.
const DDL: &str = "
CREATE SEQUENCE IF NOT EXISTS authors_id_seq;
CREATE SEQUENCE IF NOT EXISTS works_id_seq;
CREATE SEQUENCE IF NOT EXISTS authorships_id_seq;
CREATE SEQUENCE IF NOT EXISTS editions_id_seq;

CREATE TABLE IF NOT EXISTS authors (
    id int8 NOT NULL DEFAULT nextval('authors_id_seq'::regclass),
    bio TEXT,
    created_at timestamp NOT NULL,
    name TEXT NOT NULL,
    open_library_id TEXT,
    photo_id TEXT,
    sort_name TEXT,
    updated_at timestamp NOT NULL,
    works_count int4 NOT NULL DEFAULT 0,
    birth_date TEXT,
    death_date TEXT,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS works (
    id int8 NOT NULL DEFAULT nextval('works_id_seq'::regclass),
    cover_id TEXT,
    created_at timestamp NOT NULL,
    description TEXT,
    edition_count int4 NOT NULL DEFAULT 0,
    first_publish_year int4,
    open_library_id TEXT,
    subjects TEXT[] NOT NULL DEFAULT '{}',
    subtitle TEXT,
    title TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL,
    first_publish_date TEXT,
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS authorships (
    id int8 NOT NULL DEFAULT nextval('authorships_id_seq'::regclass),
    author_id int8 NOT NULL,
    created_at timestamp NOT NULL,
    position int4 NOT NULL DEFAULT 0,
    updated_at timestamp NOT NULL,
    work_id int8 NOT NULL,
    CONSTRAINT works_fk FOREIGN KEY (work_id) REFERENCES works(id),
    CONSTRAINT authors_fk FOREIGN KEY (author_id) REFERENCES authors(id),
    PRIMARY KEY (id)
);

CREATE TABLE IF NOT EXISTS editions (
    id int8 NOT NULL DEFAULT nextval('editions_id_seq'::regclass),
    asin TEXT,
    cover_id TEXT,
    created_at timestamp NOT NULL,
    format TEXT,
    goodreads_id TEXT,
    google_books_id TEXT,
    isbn10 TEXT,
    isbn13 TEXT,
    language TEXT,
    open_library_id TEXT,
    page_count int4,
    published_year int4,
    publisher TEXT,
    title TEXT,
    updated_at timestamp NOT NULL,
    work_id int8 NOT NULL,
    subtitle TEXT,
    series TEXT,
    edition_name TEXT,
    internet_archive_id TEXT,
    CONSTRAINT works_fk FOREIGN KEY (work_id) REFERENCES works(id),
    PRIMARY KEY (id)
);

ALTER SEQUENCE authors_id_seq OWNED BY authors.id;
ALTER SEQUENCE works_id_seq OWNED BY works.id;
ALTER SEQUENCE authorships_id_seq OWNED BY authorships.id;
ALTER SEQUENCE editions_id_seq OWNED BY editions.id;
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
