//! `mimir update`: bring the shared cache up to date with the latest published
//! tables.
//!
//! Tables are built by CI from the canonical txt lists and shipped as GitHub
//! release assets, so updating a machine is a download, not a rebuild. The
//! whole compare → download → verify → install loop is
//! [`HashStore::update`] in `ltk_mimir_cache`; this module just supplies the
//! reqwest-backed [`Fetch`] and prints what happened.

use std::path::PathBuf;

use anyhow::Result;
use ltk_mimir_cache::{Fetch, HashStore, UpdateOptions, UpdateOutcome};

pub struct Options {
    /// GitHub `owner/repo` whose latest release ships the tables.
    pub repo: String,

    /// Explicit base URL serving `manifest.json` + the `.lhdb` assets (a
    /// mirror); overrides `repo`.
    pub url: Option<String>,

    /// Explicit cache directory; `None` resolves the shared cache.
    pub dir: Option<PathBuf>,

    /// Reinstall every table even when the local copy already matches.
    pub force: bool,
}

pub fn run(opts: &Options) -> Result<()> {
    let base = match &opts.url {
        Some(url) => url.trim_end_matches('/').to_owned(),
        None => format!("https://github.com/{}/releases/latest/download", opts.repo),
    };
    let store = match &opts.dir {
        Some(dir) => HashStore::at(dir),
        None => HashStore::discover()?,
    };
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("mimir/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let outcome = store.update(
        &HttpFetch { base, client },
        UpdateOptions { force: opts.force },
    )?;
    let report = match outcome {
        UpdateOutcome::Locked => {
            println!(
                "another process is already updating {} - nothing to do",
                store.dir().display()
            );
            return Ok(());
        }
        UpdateOutcome::Completed(report) => report,
    };

    for id in &report.unknown_tables {
        eprintln!("{id}: unknown table - skipped (newer mimir release?)");
    }
    if report.is_up_to_date() {
        println!("up to date");
    } else {
        println!(
            "updated {} table(s) in {}",
            report.installed.len(),
            store.dir().display()
        );
    }
    if !report.gc.deleted.is_empty() {
        println!("gc: removed {} superseded file(s)", report.gc.deleted.len());
    }
    if !report.gc.retained.is_empty() {
        println!(
            "gc: {} superseded file(s) still in use - will retry next update",
            report.gc.retained.len()
        );
    }

    Ok(())
}

/// Release assets over HTTP, with a progress line per table download.
struct HttpFetch {
    base: String,
    client: reqwest::blocking::Client,
}

impl Fetch for HttpFetch {
    // reqwest's error already names the URL and HTTP status, so it flows
    // through `UpdateError::Fetch` untouched instead of being boxed.
    type Error = reqwest::Error;

    fn fetch(&self, filename: &str) -> std::result::Result<Vec<u8>, reqwest::Error> {
        if filename.ends_with(".lhdb") {
            println!("downloading {filename}");
        }

        let url = format!("{}/{filename}", self.base);
        let response = self.client.get(&url).send()?.error_for_status()?;

        Ok(response.bytes()?.to_vec())
    }
}
