//! In-process cache updates: the compare → download → verify → install loop
//! behind `mimir update`, exposed as [`HashStore::update`] (blocking) and
//! [`HashStore::update_async`].
//!
//! The crate ships no HTTP client; callers supply a [`Fetch`] (or an
//! [`AsyncFetch`]) that maps a release asset filename to its bytes (reqwest, a
//! mirror, a directory in tests). Everything else - comparison, verification,
//! atomic install, GC - lives here.

use std::fs;
use std::future::Future;
use std::path::PathBuf;

use crate::store::{is_valid_version, MANIFEST_FILE};
use crate::{
    fsutil, CommitItem, GcReport, HashStore, Manifest, ManifestError, Source, Table, TableEntry,
    UpdateError,
};

/// Fetch one release asset by filename (`manifest.json`,
/// `game-<version>.lhdb`, ...).
///
/// The error is an associated type, so [`HashStore::update`] fails with
/// [`UpdateError<Self::Error>`](UpdateError) and callers see the transport's
/// concrete error instead of a boxed one. For a GitHub release the asset URL is
/// `https://github.com/<owner>/<repo>/releases/latest/download/<filename>`.
/// Any `Fn(&str) -> Result<Vec<u8>, E>` closure whose error type meets the
/// bounds is a `Fetch`.
pub trait Fetch {
    /// The error this fetcher fails with, surfaced in
    /// [`UpdateError::Fetch`] alongside the filename that failed.
    type Error: std::error::Error + Send + Sync + 'static;

    fn fetch(&self, filename: &str) -> Result<Vec<u8>, Self::Error>;
}

impl<F, E> Fetch for F
where
    F: Fn(&str) -> Result<Vec<u8>, E>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Error = E;

    fn fetch(&self, filename: &str) -> Result<Vec<u8>, E> {
        self(filename)
    }
}

/// Fetch one release asset by filename, asynchronously - the [`Fetch`]
/// counterpart driven by [`HashStore::update_async`].
///
/// The returned future must be `Send` so the update can run on multi-threaded
/// executors. Any `Fn(&str) -> Fut` closure returning such a future is an
/// `AsyncFetch`; the future cannot borrow the filename, so build owned state
/// (e.g. the URL) before the `async move` block:
///
/// ```ignore
/// let fetch = |filename: &str| {
///     let url = format!("{base}/{filename}");
///     async move {
///         let response = client.get(&url).send().await?.error_for_status()?;
///         Ok::<_, reqwest::Error>(response.bytes().await?.to_vec())
///     }
/// };
/// ```
pub trait AsyncFetch {
    /// The error this fetcher fails with, surfaced in
    /// [`UpdateError::Fetch`] alongside the filename that failed.
    type Error: std::error::Error + Send + Sync + 'static;

    fn fetch(&self, filename: &str) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send;
}

impl<F, Fut, E> AsyncFetch for F
where
    F: Fn(&str) -> Fut,
    Fut: Future<Output = Result<Vec<u8>, E>> + Send,
    E: std::error::Error + Send + Sync + 'static,
{
    type Error = E;

    fn fetch(&self, filename: &str) -> impl Future<Output = Result<Vec<u8>, E>> + Send {
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
/// nor failure (nor a cancelled [`HashStore::update_async`]) litters the
/// cache dir.
struct Staged(Vec<PathBuf>);

impl Drop for Staged {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// One table `plan` decided to download: the remote entry plus the version
/// label parsed out of its filename.
struct PlannedDownload<'a> {
    table: Table,
    version: &'a str,
    entry: &'a TableEntry,
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
    pub fn update<F: Fetch + ?Sized>(
        &self,
        remote: &F,
        options: UpdateOptions,
    ) -> Result<UpdateOutcome, UpdateError<F::Error>> {
        let Some(_lock) = self.try_lock_update()? else {
            return Ok(UpdateOutcome::Locked);
        };

        let remote_manifest = Manifest::from_slice(&fetch(remote, MANIFEST_FILE)?)?;
        let mut report = UpdateReport::default();
        let planned = self.plan(&remote_manifest, options, &mut report)?;

        // Stage a verified download for every planned table; `staged` cleans up
        // on any error.
        let mut items = Vec::new();
        let mut staged = Staged(Vec::new());
        for download in &planned {
            let bytes = fetch(remote, &download.entry.file)?;
            items.push(self.verify_and_stage(download, &bytes, &mut staged)?);
        }

        self.finish(items, staged, remote_manifest.source.clone(), report)
    }

    /// Async twin of [`update`](HashStore::update): the same compare →
    /// download → verify → install → GC loop with the same guarantees,
    /// awaiting an [`AsyncFetch`] instead of blocking on a [`Fetch`].
    ///
    /// Local work between fetches (checksum verification, staging, the final
    /// [`commit`](HashStore::commit)) runs inline on the calling task - up to
    /// a few hundred milliseconds per table. If that stalls your executor, run
    /// the blocking [`update`](HashStore::update) on a dedicated thread
    /// instead.
    ///
    /// The future is cancel-safe: dropping it at any point releases the update
    /// lock and removes staged `.tmp` downloads, and the manifest only flips
    /// after every file is durable, so a cancelled run leaves the cache
    /// exactly as it was.
    pub async fn update_async<F: AsyncFetch + ?Sized>(
        &self,
        remote: &F,
        options: UpdateOptions,
    ) -> Result<UpdateOutcome, UpdateError<F::Error>> {
        let Some(_lock) = self.try_lock_update()? else {
            return Ok(UpdateOutcome::Locked);
        };

        let remote_manifest = Manifest::from_slice(&fetch_async(remote, MANIFEST_FILE).await?)?;
        let mut report = UpdateReport::default();

        let mut items = Vec::new();
        let mut staged = Staged(Vec::new());
        for download in &self.plan(&remote_manifest, options, &mut report)? {
            let bytes = fetch_async(remote, &download.entry.file).await?;
            items.push(self.verify_and_stage(download, &bytes, &mut staged)?);
        }

        self.finish(items, staged, remote_manifest.source.clone(), report)
    }

    /// Decide what to download: every remote table whose sha256 differs from
    /// the local manifest or whose file went missing (all of them under
    /// [`force`](UpdateOptions::force)). Unknown remote ids are recorded in
    /// `report` and skipped; a malformed remote filename is fatal.
    fn plan<'a, E>(
        &self,
        remote: &'a Manifest,
        options: UpdateOptions,
        report: &mut UpdateReport,
    ) -> Result<Vec<PlannedDownload<'a>>, UpdateError<E>> {
        let local = match self.manifest() {
            Ok(manifest) => Some(manifest),
            Err(ManifestError::Missing(_)) => None,
            Err(e) => return Err(e.into()),
        };

        let mut planned = Vec::new();
        for (id, entry) in &remote.tables {
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

            planned.push(PlannedDownload {
                table,
                version,
                entry,
            });
        }

        Ok(planned)
    }

    /// Verify downloaded bytes against the planned entry's sha256 and stage
    /// them as a `.download.tmp` next to their final name.
    fn verify_and_stage<E>(
        &self,
        download: &PlannedDownload<'_>,
        bytes: &[u8],
        staged: &mut Staged,
    ) -> Result<CommitItem, UpdateError<E>> {
        let sha256 = fsutil::sha256_bytes(bytes);
        if sha256 != download.entry.sha256 {
            return Err(UpdateError::ChecksumMismatch {
                file: download.entry.file.clone(),
                expected: download.entry.sha256.clone(),
                actual: sha256,
            });
        }

        let tmp = self
            .dir()
            .join(format!("{}.download.tmp", download.entry.file));
        fs::write(&tmp, bytes)?;
        let item = CommitItem::new(download.table, download.version, &tmp);
        staged.0.push(tmp);

        Ok(item)
    }

    /// Install the staged items and sweep superseded versions - the shared
    /// tail of [`update`](HashStore::update) and
    /// [`update_async`](HashStore::update_async).
    fn finish<E>(
        &self,
        items: Vec<CommitItem>,
        staged: Staged,
        source: Option<Source>,
        mut report: UpdateReport,
    ) -> Result<UpdateOutcome, UpdateError<E>> {
        // Install atomically - table files first, manifest pointer last.
        if !items.is_empty() {
            self.commit(&items, source)?;
            report.installed = items.iter().map(|item| item.table).collect();
        }

        // Drop the staged downloads before GC so its report never counts our own
        // in-flight `.tmp` files.
        drop(staged);

        report.gc = self.gc().unwrap_or_default();

        Ok(UpdateOutcome::Completed(report))
    }
}

/// Run one fetch, wrapping the fetcher's error with the filename.
fn fetch<F: Fetch + ?Sized>(remote: &F, filename: &str) -> Result<Vec<u8>, UpdateError<F::Error>> {
    remote.fetch(filename).map_err(|source| UpdateError::Fetch {
        file: filename.to_string(),
        source,
    })
}

/// Run one async fetch, wrapping the fetcher's error with the filename.
async fn fetch_async<F: AsyncFetch + ?Sized>(
    remote: &F,
    filename: &str,
) -> Result<Vec<u8>, UpdateError<F::Error>> {
    remote
        .fetch(filename)
        .await
        .map_err(|source| UpdateError::Fetch {
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
