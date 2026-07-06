use clap::ValueEnum;

#[derive(Copy, Clone, PartialEq, Eq, Debug, ValueEnum)]
pub enum DumpType {
    Authors,
    Editions,
    Works,
}

impl DumpType {
    pub const ALL: [DumpType; 3] = [DumpType::Authors, DumpType::Works, DumpType::Editions];

    pub fn name(self) -> &'static str {
        match self {
            DumpType::Authors => "authors",
            DumpType::Editions => "editions",
            DumpType::Works => "works",
        }
    }

    pub fn filename(self) -> String {
        format!("ol_dump_{}_latest.txt.gz", self.name())
    }

    pub fn dated_filename(self, date: &str) -> String {
        format!("ol_dump_{}_{}.txt.gz", self.name(), date)
    }

    pub fn url(self) -> String {
        let base = std::env::var("OPENLIBRARY_DUMP_BASE_URL")
            .unwrap_or_else(|_| "https://openlibrary.org/data".to_string());
        format!("{}/{}", base.trim_end_matches('/'), self.filename())
    }

    /// Direct archive.org URL for a specific dump date, e.g.
    /// `https://archive.org/download/ol_dump_2026-01-31/ol_dump_authors_2026-01-31.txt.gz`.
    pub fn archive_url(self, date: &str) -> String {
        let base = std::env::var("OPENLIBRARY_ARCHIVE_BASE_URL")
            .unwrap_or_else(|_| "https://archive.org/download".to_string());
        format!(
            "{}/ol_dump_{date}/{}",
            base.trim_end_matches('/'),
            self.dated_filename(date)
        )
    }
}
