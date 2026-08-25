//! `mimir update`: bring the shared cache up to date with the latest published
//! tables.
//!
//! Tables are built by CI from the canonical txt lists and shipped as GitHub
//! release assets, so updating a machine is a download, not a rebuild. The
//! whole compare → download → verify → install loop is
//! [`HashStore::update`] in `ltk_mimir_cache`; this module just points its
//! bundled [`UreqFetch`] at the right release and prints what happened.

use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::Result;
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use ltk_hashdb::FORMAT_VERSION;
use ltk_mimir_cache::{
    HashStore, PlannedTable, ReleaseSource, Table, UpdateObserver, UpdateOptions, UpdateOutcome,
    UreqFetch,
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

    let progress = Progress::default();
    let mut options = UpdateOptions::default().observed_by(&progress);
    if opts.force {
        options = options.forced();
    }

    let outcome = store.update(&UreqFetch::new(source), options);
    progress.finish();

    let report = match outcome? {
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

/// One bar for the whole run, driven by [`UpdateObserver`].
///
/// The update tells us the plan before it downloads anything, so the bar has a
/// length in tables and - as long as the release recorded sizes - a total in
/// bytes, rather than a spinner per file that only knows how far it has got.
#[derive(Default)]
struct Progress {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Drawn only once there is something to download.
    bar: Option<ProgressBar>,

    tables: usize,

    installed: usize,

    /// Bytes finished before the table now streaming, so the bar's position is
    /// run-wide while `progressed` reports per table.
    base: u64,

    /// Bytes of the current table, kept to fold into `base` when it lands.
    done: u64,
}

impl Progress {
    /// Clear the bar, leaving the lines it printed above it.
    fn finish(&self) {
        if let Some(bar) = &self.lock().bar {
            bar.finish_and_clear();
        }
    }

    /// A poisoned progress bar is not worth aborting an update over.
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl UpdateObserver for Progress {
    fn planned(&self, tables: &[PlannedTable]) {
        if tables.is_empty() {
            return;
        }

        // `Option` sums to `None` if any table is missing a size - an older
        // release - which is the cue to count bytes without a total.
        let total: Option<u64> = tables.iter().map(|table| table.size_bytes).sum();
        let bar = match total {
            Some(total) => ProgressBar::new(total).with_style(style(TOTALLED)),
            None => ProgressBar::new_spinner().with_style(style(COUNTING)),
        };
        bar.enable_steady_tick(Duration::from_millis(120));

        println!(
            "downloading {} table(s){}",
            tables.len(),
            match total {
                Some(total) => format!(", {}", HumanBytes(total)),
                None => String::new(),
            }
        );

        let mut state = self.lock();
        state.tables = tables.len();
        state.bar = Some(bar);
    }

    fn progressed(&self, table: Table, done: u64, _total: Option<u64>) {
        let mut state = self.lock();
        state.done = done;

        let position = state.base + done;
        let message = format!("{table} ({}/{})", state.installed + 1, state.tables);
        if let Some(bar) = &state.bar {
            bar.set_position(position);
            bar.set_message(message);
        }
    }

    fn downloaded(&self, table: Table) {
        let mut state = self.lock();
        let done = state.done;
        state.base += done;
        state.done = 0;
        state.installed += 1;

        let line = format!("downloaded {table} ({})", HumanBytes(done));
        match &state.bar {
            // Through the bar so the line lands above it rather than over it -
            // except when there is no bar to disturb, which is every redirected
            // run, where `println` on a hidden bar would print nothing at all.
            Some(bar) if !bar.is_hidden() => bar.println(line),
            _ => println!("{line}"),
        }
    }
}

/// The bar when the release recorded every table's size.
const TOTALLED: &str = "{bar:24} {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta})  {msg}";

/// The fallback when it did not: bytes so far, with nothing to fill.
const COUNTING: &str = "{spinner} {bytes} ({bytes_per_sec})  {msg}";

/// A bar style, falling back to the default rather than failing an update over
/// a template typo.
fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_bar())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `style` swallows a broken template, so nothing else would report one.
    #[test]
    fn the_bar_templates_parse() {
        for template in [TOTALLED, COUNTING] {
            assert!(
                ProgressStyle::with_template(template).is_ok(),
                "{template:?}"
            );
        }
    }
}
