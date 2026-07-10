//! In-process cache updates: the compare → download → verify → install loop
//! behind `mimir update`, exposed as [`HashStore::update`].
//!
//! The crate deliberately ships no HTTP client. Callers supply a [`Fetch`]
//! that maps a release asset filename to its bytes - backed by reqwest, ureq,
//! a mirror, or a directory in tests - and everything else lives here:
//! manifest comparison, checksum verification, atomic install under the
//! single-updater lock, and GC of superseded versions. A GUI app or library
//! consumer keeps its cache fresh without shelling out to the CLI.

use std::fs;
use std::path::PathBuf;

use crate::store::MANIFEST_FILE;
use crate::{fsutil, Error, GcReport, HashStore, Manifest, PublishItem, Result, Table};

/// The boxed error a [`Fetch`] implementation may return. It is wrapped into
/// [`Error::Fetch`] together with the filename that failed.
pub type FetchError = Box<dyn std::error::Error + Send + Sync>;

/// Fetch one release asset by filename (`manifest.json`,
/// `game-<version>.lhdb`, ...) - the single indirection between
/// [`HashStore::update`] and the network.
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
    /// Reinstall every table even when the local copy already matches the
    /// remote manifest.
    pub force: bool,
}

/// What an update run did.
#[derive(Debug)]
pub enum UpdateOutcome {
    /// Another process holds the update lock; nothing was done (not even the
    /// manifest fetch) - leave the work to it.
    Locked,

    /// The run completed; the report says what changed.
    Completed(UpdateReport),
}

/// What a completed update run installed and cleaned up.
#[derive(Debug, Default)]
pub struct UpdateReport {
    /// Tables that were downloaded, verified, and installed. Empty when every
    /// remote table already matched the local cache.
    pub installed: Vec<Table>,

    /// Remote manifest ids this build has no [`Table`] for - a newer release
    /// publishing tables this version doesn't know. Skipped, never fatal.
    pub unknown_tables: Vec<String>,

    /// What the post-install GC swept. GC only runs after an install, so this
    /// stays empty on an up-to-date run.
    pub gc: GcReport,
}

impl UpdateReport {
    /// True when the run installed nothing because everything already matched.
    pub fn is_up_to_date(&self) -> bool {
        self.installed.is_empty()
    }
}

/// Downloaded-but-not-yet-installed files, removed on drop so neither success
/// nor failure litters the cache dir (`gc` would sweep the `.tmp` suffix
/// eventually anyway).
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
    /// Fetches the remote `manifest.json`, keeps every table whose sha256
    /// already matches the local manifest (unless
    /// [`force`](UpdateOptions::force)), downloads the rest through `remote`,
    /// verifies each checksum, installs atomically via
    /// [`publish`](HashStore::publish), and [`gc`](HashStore::gc)s superseded
    /// versions - all under the single-updater lock. Readers keep resolving
    /// throughout: they see either the whole old version or the whole new one.
    ///
    /// Returns [`UpdateOutcome::Locked`] without touching the network when
    /// another process is already updating. A failed download or checksum
    /// mismatch errors out before anything is installed.
    pub fn update(
        &self,
        remote: &(impl Fetch + ?Sized),
        options: UpdateOptions,
    ) -> Result<UpdateOutcome> {
        let Some(_lock) = self.try_lock_update()? else {
            return Ok(UpdateOutcome::Locked);
        };

        let remote_manifest = Manifest::from_slice(&fetch(remote, MANIFEST_FILE)?)?;
        let local = match self.manifest() {
            Ok(manifest) => Some(manifest),
            Err(Error::MissingManifest(_)) => None,
            Err(e) => return Err(e),
        };

        // Stage a verified download for every table whose content differs from
        // what the local manifest points at (or whose file has gone missing on
        // disk).
        let mut report = UpdateReport::default();
        let mut items = Vec::new();
        let mut staged = Staged(Vec::new());
        for (id, entry) in &remote_manifest.tables {
            let Some(table) = Table::from_id(id) else {
                report.unknown_tables.push(id.clone());
                continue;
            };
            let version =
                version_of(table, &entry.file).ok_or_else(|| Error::BadRemoteFilename {
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
                return Err(Error::ChecksumMismatch {
                    file: entry.file.clone(),
                    expected: entry.sha256.clone(),
                    actual: sha256,
                });
            }

            let tmp = self.dir().join(format!("{}.download.tmp", entry.file));
            fs::write(&tmp, &bytes)?;
            items.push(PublishItem::new(table, version, &tmp));
            staged.0.push(tmp);
        }
        if items.is_empty() {
            return Ok(UpdateOutcome::Completed(report));
        }

        // Install atomically - table files first, manifest pointer last - then
        // sweep the versions nothing references anymore.
        self.publish(&items, remote_manifest.source.clone())?;
        report.installed = items.iter().map(|item| item.table).collect();
        report.gc = self.gc()?;

        Ok(UpdateOutcome::Completed(report))
    }
}

/// Run one fetch, wrapping the fetcher's error with the filename.
fn fetch(remote: &(impl Fetch + ?Sized), filename: &str) -> Result<Vec<u8>> {
    remote.fetch(filename).map_err(|source| Error::Fetch {
        file: filename.to_string(),
        source,
    })
}

/// The version label embedded in a release filename (`<id>-<version>.lhdb`).
/// Rejects anything that is not a single clean path component: the manifest is
/// remote input, and the filename is reused locally.
fn version_of(table: Table, file: &str) -> Option<&str> {
    let version = file
        .strip_prefix(table.id())?
        .strip_prefix('-')?
        .strip_suffix(".lhdb")?;

    (!version.is_empty() && !version.contains(['/', '\\'])).then_some(version)
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
