use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};

use crate::dump::DumpType;

/// Where to download from: the openlibrary.org "latest" redirect, or a
/// specific dated dump on archive.org.
pub enum Source {
    Latest,
    Archive { date: String },
}

impl Source {
    fn url(&self, dump: DumpType) -> String {
        match self {
            Source::Latest => dump.url(),
            Source::Archive { date } => dump.archive_url(date),
        }
    }

    fn filename(&self, dump: DumpType) -> String {
        match self {
            Source::Latest => dump.filename(),
            Source::Archive { date } => dump.dated_filename(date),
        }
    }
}

pub fn run(dir: &Path, force: bool, source: &Source, only: Option<DumpType>) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create {}", dir.display()))?;

    let client = Client::builder()
        .timeout(None) // the default 30s timeout applies to the whole body — fatal for multi-GB files
        .connect_timeout(Duration::from_secs(30))
        .build()?;

    match source {
        Source::Latest => println!("Downloading latest Open Library dumps to {}", dir.display()),
        Source::Archive { date } => println!(
            "Downloading Open Library dumps for {date} from archive.org to {}",
            dir.display()
        ),
    }
    let multi = MultiProgress::new();

    let types: &[DumpType] = match only {
        Some(ref t) => std::slice::from_ref(t),
        None => &DumpType::ALL,
    };
    let results: Vec<(DumpType, Result<()>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = types
            .iter()
            .map(|&t| {
                let bar = multi.add(ProgressBar::new_spinner());
                bar.set_prefix(t.name());
                bar.enable_steady_tick(Duration::from_millis(250));
                bar.set_style(spinner_style());
                bar.set_message("connecting…");
                let client = &client;
                scope.spawn(move || (t, download_one(client, t, dir, force, source, bar)))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut failed = false;
    for (t, result) in results {
        if let Err(e) = result {
            failed = true;
            eprintln!("error downloading {}: {e:#}", t.name());
        }
    }
    if failed {
        bail!("some downloads failed — re-run `openlibrary download` to resume");
    }
    println!("All dumps downloaded.");
    Ok(())
}

fn download_one(
    client: &Client,
    dump: DumpType,
    dir: &Path,
    force: bool,
    source: &Source,
    bar: ProgressBar,
) -> Result<()> {
    let path: PathBuf = dir.join(source.filename(dump));
    let mut existing = if force {
        let _ = fs::remove_file(&path);
        0
    } else {
        fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    };

    let mut request = client.get(source.url(dump));
    if existing > 0 {
        request = request.header(RANGE, format!("bytes={existing}-"));
    }
    let response = request.send().context("request failed")?;

    let total: u64;
    match response.status() {
        // Server honored the range request: resume where the file left off.
        StatusCode::PARTIAL_CONTENT => {
            total = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('/').next())
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| anyhow!("missing Content-Range on 206 response"))?;
        }
        // Requested range starts at (or past) the end of the file: already complete.
        StatusCode::RANGE_NOT_SATISFIABLE => {
            bar.set_style(bytes_style());
            bar.set_length(existing);
            bar.set_position(existing);
            bar.finish_with_message("already downloaded");
            return Ok(());
        }
        StatusCode::OK => {
            // Full response — either a fresh download or the server ignored our
            // range request; either way start over.
            existing = 0;
            total = response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
        status => bail!("unexpected HTTP status {status}"),
    }

    bar.set_style(bytes_style());
    bar.set_length(total);
    bar.set_position(existing);
    if existing > 0 {
        bar.set_message("resuming");
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(existing == 0)
        .append(existing > 0)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut writer = BufWriter::with_capacity(1 << 20, file);
    let mut reader = bar.wrap_read(response);

    io::copy(&mut reader, &mut writer).context("download interrupted")?;
    bar.finish_with_message("done");
    Ok(())
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:>8} {spinner} {msg}").unwrap()
}

fn bytes_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:>8} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta}) {msg}",
    )
    .unwrap()
    .progress_chars("=> ")
}
