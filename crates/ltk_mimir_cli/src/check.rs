//! `mimir check`: say what an update would do, without doing any of it.
//!
//! Fetches the published manifest and diffs it against the shared cache -
//! [`HashStore::check`] in `ltk_mimir_cache`. No download, no install, and no
//! update lock, so this is safe to run while another process is midway through
//! `mimir update`.

use std::path::PathBuf;

use anyhow::Result;
use indicatif::HumanBytes;
use ltk_mimir_cache::{HashStore, ReleaseSource, TableStatus, UreqFetch};

pub struct Options {
    /// GitHub `owner/repo` whose latest release ships the tables.
    pub repo: String,

    /// Explicit base URL serving `manifest.json` + the `.lhdb` assets (a
    /// mirror); overrides `repo`.
    pub url: Option<String>,

    /// Explicit cache directory; `None` resolves the shared cache.
    pub dir: Option<PathBuf>,
}

pub fn run(opts: &Options) -> Result<()> {
    let source = match &opts.url {
        Some(url) => ReleaseSource::base_url(url.clone()),
        None => ReleaseSource::github(&opts.repo),
    };
    let store = match &opts.dir {
        Some(dir) => HashStore::at(dir),
        None => HashStore::discover()?,
    };

    let report = store.check(&UreqFetch::new(source))?;

    println!("{}", store.dir().display());
    let width = report
        .tables
        .iter()
        .map(|diff| diff.table.id().len())
        .max()
        .unwrap_or(0)
        .max("table".len());
    println!("  {:width$}  {:12}{:12}status", "table", "have", "release");

    for diff in &report.tables {
        // Version labels rather than the filenames they are embedded in. Both
        // sides are shown because they can differ while the bytes do not: a
        // release relabels every table it rebuilds, and only the ones whose
        // content actually changed are worth downloading.
        let have = match &diff.local {
            Some(local) if !local.version.is_empty() => local.version.as_str(),
            Some(_) => "?",
            None => "-",
        };
        let detail = match diff.status {
            TableStatus::Unsupported => {
                format!(" (.hashdb format {})", diff.remote.format_version)
            }
            _ => String::new(),
        };

        println!(
            "  {:width$}  {:12}{:12}{}{detail}",
            diff.table.id(),
            have,
            diff.remote.version,
            diff.status,
        );
    }

    for id in &report.unknown_tables {
        println!("  {id}: published, but unknown to this build (newer release?)");
    }

    if report.is_up_to_date() {
        println!("nothing to download");
    } else {
        // A release published before the manifest recorded sizes leaves the
        // total unknown; the table count is still exact.
        let size = match report.download_bytes() {
            Some(bytes) => format!(", {}", HumanBytes(bytes)),
            None => String::new(),
        };
        println!(
            "{} table(s) behind{size} - run `mimir update`",
            report.behind()
        );
    }

    Ok(())
}
