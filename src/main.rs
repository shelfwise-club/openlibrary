mod download;
mod dump;
mod import;
mod model;
mod schema;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use dump::DumpType;

#[derive(Parser)]
#[command(
    name = "openlibrary",
    version,
    about = "Download and import Open Library data dumps"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download the latest Open Library data dumps
    Download {
        /// Download only one dump type
        #[arg(long, value_enum)]
        only: Option<DumpType>,
        /// Directory to store the dumps
        #[arg(long, default_value = "./data")]
        dir: PathBuf,
        /// Re-download files even if they already exist
        #[arg(long)]
        force: bool,
        /// Download a dated dump directly from archive.org (requires --date)
        #[arg(long, requires = "date")]
        archive: bool,
        /// Which dump date to download from archive.org
        #[arg(long, value_name = "YYYY-MM-DD", requires = "archive")]
        date: Option<String>,
    },
    /// Parse the dumps and import them into PostgreSQL
    Import {
        /// Import only one dump type
        #[arg(long, value_enum)]
        only: Option<DumpType>,
        /// Import a small, good-quality subset (~200k editions from 1950+ with covers)
        #[arg(long)]
        dev: bool,
        /// PostgreSQL connection string
        #[arg(
            long,
            env = "DATABASE_URL",
            default_value = "postgres://localhost:5432/shelfwise_development"
        )]
        database_url: String,
        /// Directory the dumps were downloaded to
        #[arg(long, default_value = "./data")]
        dir: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Download {
            only,
            dir,
            force,
            archive,
            date,
        } => {
            let source = match (archive, date) {
                (true, Some(date)) => {
                    if chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
                        anyhow::bail!("--date must be YYYY-MM-DD (got {date:?})");
                    }
                    download::Source::Archive { date }
                }
                _ => download::Source::Latest,
            };
            download::run(&dir, force, &source, only)
        }
        Command::Import {
            only,
            dev,
            database_url,
            dir,
        } => import::run(&dir, &database_url, only, dev),
    }
}
