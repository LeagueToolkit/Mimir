//! `mimir update`: bring the shared cache up to date with the latest published
//! tables.
//!
//! Tables are built by CI from the canonical txt lists and shipped as GitHub
//! release assets, so updating a machine is a download, not a rebuild. The
//! whole compare → download → verify → install loop is
//! [`HashStore::update`] in `ltk_mimir_cache`; this module just points its
//! bundled [`UreqFetch`] at the right release and prints what happened.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use ltk_hashdb::FORMAT_VERSION;
use ltk_mimir_cache::{
    Fetch, FetchError, HashStore, ReleaseSource, UpdateOptions, UpdateOutcome, UreqFetch,
};

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
    let source = match &opts.url {
        Some(url) => ReleaseSource::base_url(url.clone()),
        None => ReleaseSource::github(&opts.repo),
    };
    let store = match &opts.dir {
        Some(dir) => HashStore::at(dir),
        None => HashStore::discover()?,
    };

    let fetch = Reporting(UreqFetch::new(source));
    let outcome = store.update(&fetch, UpdateOptions { force: opts.force })?;
    let report = match outcome {
        UpdateOutcome::Locked => {
            let who = match store.lock_holder()? {
                Some(holder) => format!("pid {} since {}", holder.pid, holder.since),
                // It finished between the failed lock and this read, or never
                // got as far as writing its name.
                None => "another process".to_owned(),
            };
            println!(
                "{who} is already updating {} - nothing to do",
                store.dir().display()
            );
            return Ok(());
        }
        UpdateOutcome::Completed(report) => report,
    };

    for id in &report.unknown_tables {
        eprintln!("{id}: unknown table - skipped (newer mimir release?)");
    }
    for skipped in &report.unsupported_tables {
        eprintln!(
            "{}: published in .hashdb format {} - skipped (this build reads {})",
            skipped.table, skipped.format_version, FORMAT_VERSION
        );
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

/// A fetcher that draws a bar for every table it streams through.
///
/// Progress lives here rather than in the library: `fetch_to` hands the download
/// to a sink of our choosing, so counting bytes is a wrapper around the sink and
/// costs the update path nothing.
struct Reporting<F>(F);

impl<F: Fetch> Fetch for Reporting<F> {
    type Error = F::Error;

    fn fetch_to(
        &self,
        filename: &str,
        sink: &mut (dyn Write + Send),
    ) -> Result<u64, FetchError<Self::Error>> {
        // The manifest is a few KB and arrives before anything is worth drawing.
        if !filename.ends_with(".lhdb") {
            return self.0.fetch_to(filename, sink);
        }

        // A spinner rather than a bar: the manifest records each table's sha256
        // and entry count, but not its size, so there is no total to fill.
        let bar = ProgressBar::new_spinner().with_message(filename.to_owned());
        bar.set_style(
            ProgressStyle::with_template("{spinner} {msg}  {bytes} ({bytes_per_sec})")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        bar.enable_steady_tick(Duration::from_millis(120));

        let mut counting = Counting { sink, bar: &bar };
        let result = self.0.fetch_to(filename, &mut counting);
        bar.finish_and_clear();

        if let Ok(bytes) = result {
            println!("downloaded {filename} ({})", indicatif::HumanBytes(bytes));
        }

        result
    }
}

/// A sink that forwards to another and tells the bar how far it has got.
struct Counting<'a> {
    sink: &'a mut (dyn Write + Send),

    bar: &'a ProgressBar,
}

impl Write for Counting<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.sink.write(buf)?;
        self.bar.inc(n as u64);

        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.sink.flush()
    }
}
