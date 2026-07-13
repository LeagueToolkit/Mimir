//! In-process cache updates: the compare → download → verify → install loop
//! behind `mimir update`, exposed as [`HashStore::update`].
//!
//! The crate ships no HTTP client; callers supply a [`Fetch`] that maps a
//! release asset filename to its bytes (reqwest, a mirror, a directory in
//! tests). Everything else - comparison, verification, atomic install, GC -
//! lives here.

use std::fs;
use std::path::PathBuf;

use crate::store::{is_valid_version, MANIFEST_FILE};
use crate::{fsutil, CommitItem, GcReport, HashStore, Manifest, ManifestError, Table, UpdateError};

/// The boxed error a [`Fetch`] may return; wrapped into [`UpdateError::Fetch`]
/// with the filename that failed.
pub type FetchError = Box<dyn std::error::Error + Send + Sync>;

/// Fetch one release asset by filename (`manifest.json`,
/// `game-<version>.lhdb`, ...).
///
/// For a GitHub release the asset URL is
/// `https://github.com/<owner>/<repo>/releases/latest/download/<filename>`.
/// Any `Fn(&str) -> Result<Vec<u8>, FetchError>` closure is a `Fetch`.
pub trait Fetch {
    fn fetch(&self, filename: &str) -> std::result::Result<Vec<u8>, FetchError>;
}

impl<F> Fetch for F
where
    F: Fn(&str) -> std::result::Result<Vec<u8>, FetchError>,
{
    fn fetch(&self, filename: &str) -> std::result::Result<Vec<u8>, FetchError> {
        self(filename)
    }
}

/// Knobs for [`HashStore::update`].
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateOptions {
    /// Reinstall every table even when the local copy already matches.
    pub force: bool,
}

/// What an update run did.
#[derive(Debug)]
pub enum UpdateOutcome {
    /// Another process holds the update lock; nothing was done.
    Locked,

    /// The run completed; the report says what changed.
    Completed(UpdateReport),
}

/// What a completed update run installed and cleaned up.
#[derive(Debug, Default)]
pub struct UpdateReport {
    /// Tables that were downloaded, verified, and installed.
    pub installed: Vec<Table>,

    /// Remote manifest ids this build has no [`Table`] for (a newer release).
    /// Skipped, never fatal.
    pub unknown_tables: Vec<String>,

    /// What GC swept. GC runs even on up-to-date runs, so files a prior run
    /// had to retain (e.g. still mmap'd on Windows) get another chance.
    pub gc: GcReport,
}

impl UpdateReport {
    /// True when the run installed nothing because everything already matched.
    pub fn is_up_to_date(&self) -> bool {
        self.installed.is_empty()
    }
}

/// Downloaded-but-not-yet-installed files, removed on drop so neither success
/// nor failure litters the cache dir.
struct Staged(Vec<PathBuf>);

impl Drop for Staged {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

impl HashStore {
    /// Bring the cache up to date with a published release, in-process.
    ///
    /// Fetches the remote `manifest.json`, downloads every table whose sha256
    /// differs from the local one (all of them under
    /// [`force`](UpdateOptions::force)), verifies each checksum, installs
    /// atomically via [`commit`](HashStore::commit), and [`gc`](HashStore::gc)s
    /// superseded versions - all under the single-updater lock. Readers see
    /// either the whole old version or the whole new one.
    ///
    /// Returns [`UpdateOutcome::Locked`] when another process is already
    /// updating. Any failure errors out before anything is installed. A release
    /// published mid-run can fail a fetch or a checksum; that state is transient
    /// and re-running converges on the new release.
    pub fn update(
        &self,
        remote: &(impl Fetch + ?Sized),
        options: UpdateOptions,
    ) -> Result<UpdateOutcome, UpdateError> {
        let Some(_lock) = self.try_lock_update()? else {
            return Ok(UpdateOutcome::Locked);
        };

        let local = match self.manifest() {
            Ok(manifest) => Some(manifest),
            Err(ManifestError::Missing(_)) => None,
            Err(e) => return Err(e.into()),
        };

        // Stage a verified download for every table that differs from the local
        // manifest (or whose file went missing); `staged` cleans up on any error.
        let remote_manifest = Manifest::from_slice(&fetch(remote, MANIFEST_FILE)?)?;
        let mut report = UpdateReport::default();
        let mut items = Vec::new();
        let mut staged = Staged(Vec::new());
        for (id, entry) in &remote_manifest.tables {
            let Some(table) = Table::from_id(id) else {
                report.unknown_tables.push(id.clone());
                continue;
            };
            let version =
                version_of(table, &entry.file).ok_or_else(|| UpdateError::BadRemoteFilename {
                    id: id.clone(),
                    file: entry.file.clone(),
                })?;

            let current = local.as_ref().and_then(|m| m.entry(table));
            let fresh = current
                .is_some_and(|c| c.sha256 == entry.sha256 && self.dir().join(&c.file).is_file());
            if fresh && !options.force {
                continue;
            }

            let bytes = fetch(remote, &entry.file)?;
            let sha256 = fsutil::sha256_bytes(&bytes);
            if sha256 != entry.sha256 {
                return Err(UpdateError::ChecksumMismatch {
                    file: entry.file.clone(),
                    expected: entry.sha256.clone(),
                    actual: sha256,
                });
            }

            let tmp = self.dir().join(format!("{}.download.tmp", entry.file));
            fs::write(&tmp, &bytes)?;
            items.push(CommitItem::new(table, version, &tmp));
            staged.0.push(tmp);
        }

        // Install atomically - table files first, manifest pointer last.
        if !items.is_empty() {
            self.commit(&items, remote_manifest.source.clone())?;
            report.installed = items.iter().map(|item| item.table).collect();
        }

        // Drop the staged downloads before GC so its report never counts our own
        // in-flight `.tmp` files.
        drop(staged);

        // Best-effort: the install is already durable, so a GC hiccup must not
        // fail the update.
        report.gc = self.gc().unwrap_or_default();

        Ok(UpdateOutcome::Completed(report))
    }
}

/// Run one fetch, wrapping the fetcher's error with the filename.
fn fetch(remote: &(impl Fetch + ?Sized), filename: &str) -> Result<Vec<u8>, UpdateError> {
    remote.fetch(filename).map_err(|source| UpdateError::Fetch {
        file: filename.to_string(),
        source,
    })
}

/// The version label embedded in a release filename (`<id>-<version>.lhdb`).
/// The manifest is remote input and the filename is reused locally, so anything
/// that is not a clean path component is rejected via [`is_valid_version`].
fn version_of(table: Table, file: &str) -> Option<&str> {
    let version = file
        .strip_prefix(table.id())?
        .strip_prefix('-')?
        .strip_suffix(".lhdb")?;

    is_valid_version(version).then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_of_rejects_escapes() {
        assert_eq!(
            version_of(Table::Game, "game-2026-07-10.lhdb"),
            Some("2026-07-10")
        );
        assert_eq!(
            version_of(Table::RstXxh3, "rst-xxh3-2026-07-10.lhdb"),
            Some("2026-07-10")
        );
        assert_eq!(version_of(Table::Game, "lcu-1.lhdb"), None);
        assert_eq!(version_of(Table::Game, "game-.lhdb"), None);
        assert_eq!(version_of(Table::Game, "game-..\\evil.lhdb"), None);
        assert_eq!(version_of(Table::Game, "game-a/b.lhdb"), None);
    }
}
