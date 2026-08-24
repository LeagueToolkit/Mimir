//! In-process cache updates: the compare → download → verify → install loop
//! behind `mimir update`, exposed as [`HashStore::update`] (blocking) and
//! [`HashStore::update_async`].
//!
//! The crate ships no HTTP client; callers supply a [`Fetch`] (or an
//! [`AsyncFetch`]) that maps a release asset filename to its bytes (reqwest, a
//! mirror, a directory in tests). Everything else - comparison, verification,
//! atomic install, GC - lives here.

use std::fs::{self, File};
use std::future::Future;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::manifest::version_of;
use crate::store::MANIFEST_FILE;
use crate::{
    fsutil, CheckError, CommitItem, FetchError, GcReport, HashStore, Manifest, ManifestError,
    Source, Table, TableEntry, UpdateError,
};

/// Fetch one release asset by filename (`manifest.json`,
/// `game-<version>.lhdb`, ...).
///
/// [`fetch_to`](Fetch::fetch_to) is the primitive: it streams an asset into a
/// sink, so a 38 MiB table never has to exist in memory and the caller decides
/// where the bytes land. [`fetch`](Fetch::fetch) is the convenience form over
/// it, for the small assets (the manifest) where a buffer is simpler.
///
/// The error is an associated type, so callers see the transport's concrete
/// error instead of a boxed one. For a GitHub release the asset URL is
/// `https://github.com/<owner>/<repo>/releases/latest/download/<filename>`.
/// Any `Fn(&str) -> Result<Vec<u8>, E>` closure whose error type meets the
/// bounds is a `Fetch` - it just buffers rather than streams.
///
/// Wrapping a fetcher is how progress reporting and cancellation are done: pass
/// the inner fetcher a sink of your own that counts bytes, and return an error
/// from it to stop the download where it stands.
///
/// ```
/// # use std::io::Write;
/// # use ltk_mimir_cache::{Fetch, FetchError};
/// struct Reporting<F>(F);
///
/// impl<F: Fetch> Fetch for Reporting<F> {
///     type Error = F::Error;
///
///     fn fetch_to(
///         &self,
///         filename: &str,
///         sink: &mut (dyn Write + Send),
///     ) -> Result<u64, FetchError<Self::Error>> {
///         let mut counting = Counting { sink, done: 0 };
///         self.0.fetch_to(filename, &mut counting)
///     }
/// }
///
/// struct Counting<'a> {
///     sink: &'a mut (dyn Write + Send),
///     done: u64,
/// }
///
/// impl Write for Counting<'_> {
///     fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
///         let n = self.sink.write(buf)?;
///         self.done += n as u64;
///         println!("{} bytes", self.done);
///         Ok(n)
///     }
///
///     fn flush(&mut self) -> std::io::Result<()> {
///         self.sink.flush()
///     }
/// }
/// ```
pub trait Fetch {
    /// The error this fetcher fails with, surfaced in
    /// [`UpdateError::Fetch`] alongside the filename that failed.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Stream one asset into `sink`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// [`FetchError::Transport`] for anything the fetcher itself hit, and
    /// [`FetchError::Sink`] when `sink` refused the bytes - a full disk, or a
    /// wrapper cancelling the download.
    fn fetch_to(
        &self,
        filename: &str,
        sink: &mut (dyn Write + Send),
    ) -> Result<u64, FetchError<Self::Error>>;

    /// The whole asset in memory.
    ///
    /// The default collects [`fetch_to`](Fetch::fetch_to) into a `Vec`, which is
    /// what the manifest wants and what a table does not.
    fn fetch(&self, filename: &str) -> Result<Vec<u8>, FetchError<Self::Error>> {
        let mut buf = Vec::new();
        self.fetch_to(filename, &mut buf)?;

        Ok(buf)
    }
}

impl<F, E> Fetch for F
where
    F: Fn(&str) -> Result<Vec<u8>, E>,
    E: std::error::Error + Send + Sync + 'static,
{
    type Error = E;

    fn fetch_to(
        &self,
        filename: &str,
        sink: &mut (dyn Write + Send),
    ) -> Result<u64, FetchError<E>> {
        let bytes = self(filename).map_err(FetchError::Transport)?;
        sink.write_all(&bytes).map_err(FetchError::Sink)?;

        Ok(bytes.len() as u64)
    }

    fn fetch(&self, filename: &str) -> Result<Vec<u8>, FetchError<E>> {
        self(filename).map_err(FetchError::Transport)
    }
}

/// Fetch one release asset by filename, asynchronously - the [`Fetch`]
/// counterpart driven by [`HashStore::update_async`].
///
/// The returned futures must be `Send` so the update can run on multi-threaded
/// executors. Any `Fn(&str) -> Fut` closure returning such a future is an
/// `AsyncFetch`; the future cannot borrow the filename, so build owned state
/// (e.g. the URL) before the `async move` block:
///
/// ```ignore
/// let fetch = |filename: &str| {
///     let url = format!("{base}/{filename}");
///     async move {
///         let response = client.get(&url).send().await?.error_for_status()?;
///         Ok::<_, HttpFetchError>(response.bytes().await?.to_vec())
///     }
/// };
/// ```
pub trait AsyncFetch {
    /// The error this fetcher fails with, surfaced in
    /// [`UpdateError::Fetch`] alongside the filename that failed.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Stream one asset into `sink`, returning the number of bytes written.
    ///
    /// The sink is written from the future, so it is borrowed for the whole
    /// await - see [`Fetch::fetch_to`] for what the two failure modes mean.
    fn fetch_to<'a>(
        &'a self,
        filename: &'a str,
        sink: &'a mut (dyn Write + Send),
    ) -> impl Future<Output = Result<u64, FetchError<Self::Error>>> + Send + 'a;

    /// The whole asset in memory; the async twin of [`Fetch::fetch`].
    ///
    /// The default body holds `&self` across an await, so it needs a `Sync`
    /// fetcher. Override it if yours is not one.
    fn fetch(
        &self,
        filename: &str,
    ) -> impl Future<Output = Result<Vec<u8>, FetchError<Self::Error>>> + Send
    where
        Self: Sync,
    {
        // Owned so the future borrows nothing but `self`.
        let filename = filename.to_owned();
        async move {
            let mut buf = Vec::new();
            self.fetch_to(&filename, &mut buf).await?;

            Ok(buf)
        }
    }
}

impl<F, Fut, E> AsyncFetch for F
where
    F: Fn(&str) -> Fut,
    // `'static` is the documented contract already: the future cannot borrow the
    // filename, so it can also outlive the sink it is handed.
    Fut: Future<Output = Result<Vec<u8>, E>> + Send + 'static,
    E: std::error::Error + Send + Sync + 'static,
{
    type Error = E;

    fn fetch_to<'a>(
        &'a self,
        filename: &'a str,
        sink: &'a mut (dyn Write + Send),
    ) -> impl Future<Output = Result<u64, FetchError<E>>> + Send + 'a {
        let fetched = self(filename);
        async move {
            let bytes = fetched.await.map_err(FetchError::Transport)?;
            sink.write_all(&bytes).map_err(FetchError::Sink)?;

            Ok(bytes.len() as u64)
        }
    }

    fn fetch(&self, filename: &str) -> impl Future<Output = Result<Vec<u8>, FetchError<E>>> + Send {
        let fetched = self(filename);
        async move { fetched.await.map_err(FetchError::Transport) }
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

/// What an update would do to one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableStatus {
    /// The cache already holds exactly this file.
    Current,

    /// The cache has no version of this table.
    Absent,

    /// The cache holds a different version.
    Stale,

    /// The manifest points at this table, but the file is gone from the cache
    /// directory - an interrupted GC, or someone tidying up by hand.
    FileMissing,

    /// Published in a `.hashdb` format version this build cannot open, so an
    /// update would skip it and leave whatever the cache holds in place.
    Unsupported,
}

impl TableStatus {
    /// Whether an update would download this table (without
    /// [`force`](UpdateOptions::force), which downloads everything).
    pub fn needs_update(self) -> bool {
        matches!(self, Self::Absent | Self::Stale | Self::FileMissing)
    }
}

impl std::fmt::Display for TableStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Current => "up to date",
            Self::Absent => "not installed",
            Self::Stale => "outdated",
            Self::FileMissing => "file missing",
            Self::Unsupported => "unsupported format",
        })
    }
}

/// One table as the release publishes it, next to what the cache holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDiff {
    /// The table this describes.
    pub table: Table,

    /// What an update would do to it.
    pub status: TableStatus,

    /// The entry the release publishes.
    pub remote: TableEntry,

    /// The entry the cache holds, absent when it holds none.
    pub local: Option<TableEntry>,
}

/// What a lock-free [`check`](HashStore::check) found.
///
/// Shaped like [`UpdateReport`] on purpose - the same run, described before it
/// happens rather than after.
#[derive(Debug, Clone, Default)]
pub struct CheckReport {
    /// One diff per remote table this build knows, in manifest order.
    pub tables: Vec<TableDiff>,

    /// Remote manifest ids this build has no [`Table`] for (a newer release).
    pub unknown_tables: Vec<String>,
}

impl CheckReport {
    /// How many tables an update would download - the "3 tables behind" number.
    pub fn behind(&self) -> usize {
        self.tables
            .iter()
            .filter(|diff| diff.status.needs_update())
            .count()
    }

    /// True when an update would install nothing.
    pub fn is_up_to_date(&self) -> bool {
        self.behind() == 0
    }
}

/// A remote table this build cannot read, and the format version it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedTable {
    /// The table the release published.
    pub table: Table,

    /// The `.hashdb` format version it was published in.
    pub format_version: u16,
}

/// What a completed update run installed and cleaned up.
#[derive(Debug, Default)]
pub struct UpdateReport {
    /// Tables that were downloaded, verified, and installed.
    pub installed: Vec<Table>,

    /// Remote manifest ids this build has no [`Table`] for (a newer release).
    /// Skipped, never fatal.
    pub unknown_tables: Vec<String>,

    /// Tables published in a `.hashdb` format version this build cannot open.
    /// Skipped, never fatal - whatever version the cache already holds keeps
    /// being served.
    pub unsupported_tables: Vec<UnsupportedTable>,

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

        let remote_manifest = fetch_manifest(remote)?;
        let mut report = UpdateReport::default();
        let planned = self.plan(&remote_manifest, options, &mut report)?;

        // Stage a verified download for every planned table; `staged` cleans up
        // on any error.
        let mut items = Vec::new();
        let mut staged = Staged(Vec::new());
        for download in &planned {
            let tmp = self.stage_path(download, &mut staged);
            let mut sink = staging_sink(&tmp)?;
            let sha256 = fetch_into(remote, &download.entry.file, &mut sink)?;

            items.push(verified(download, &tmp, sha256)?);
        }

        self.finish(items, staged, remote_manifest.last_run.clone(), report)
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

        let remote_manifest = fetch_manifest_async(remote).await?;
        let mut report = UpdateReport::default();

        let mut items = Vec::new();
        let mut staged = Staged(Vec::new());
        for download in &self.plan(&remote_manifest, options, &mut report)? {
            let tmp = self.stage_path(download, &mut staged);
            let mut sink = staging_sink(&tmp)?;
            let sha256 = fetch_into_async(remote, &download.entry.file, &mut sink).await?;

            items.push(verified(download, &tmp, sha256)?);
        }

        self.finish(items, staged, remote_manifest.last_run.clone(), report)
    }

    /// Compare the cache against a published release without changing either.
    ///
    /// Fetches the remote manifest, diffs it against the local one, and returns
    /// a status for every table both know about. Nothing is downloaded, nothing
    /// is installed, and the update lock is never taken - so a UI can poll this
    /// on a timer, and a startup check can run while `mimir update` is midway
    /// through a download.
    ///
    /// The answer is a snapshot: a release published a moment later makes it
    /// stale, exactly as with any other status read.
    ///
    /// # Errors
    ///
    /// [`CheckError::Fetch`] if the manifest cannot be retrieved, and
    /// [`CheckError::Manifest`] if either manifest is unreadable. A cache that
    /// has never been populated is not an error: every table comes back
    /// [`TableStatus::Absent`].
    pub fn check<F: Fetch + ?Sized>(
        &self,
        remote: &F,
    ) -> Result<CheckReport, CheckError<F::Error>> {
        let remote = fetch_manifest(remote)?;
        self.diff(&remote)
    }

    /// Async twin of [`check`](HashStore::check).
    pub async fn check_async<F: AsyncFetch + ?Sized>(
        &self,
        remote: &F,
    ) -> Result<CheckReport, CheckError<F::Error>> {
        let remote = fetch_manifest_async(remote).await?;
        self.diff(&remote)
    }

    /// Diff a fetched remote manifest against the local one.
    fn diff<E>(&self, remote: &Manifest) -> Result<CheckReport, CheckError<E>> {
        let local = self.local_manifest()?;

        let mut report = CheckReport::default();
        for (id, entry) in &remote.tables {
            let Some(table) = Table::from_id(id) else {
                report.unknown_tables.push(id.clone());
                continue;
            };
            let local = local.as_ref().and_then(|m| m.entry(table));

            report.tables.push(TableDiff {
                table,
                status: self.status_of(local, entry),
                remote: entry.clone(),
                local: local.cloned(),
            });
        }

        Ok(report)
    }

    /// What an update would do to one table. The file-presence check is what
    /// makes a manually deleted `.lhdb` reinstall rather than read as current.
    fn status_of(&self, local: Option<&TableEntry>, remote: &TableEntry) -> TableStatus {
        if !remote.is_supported() {
            return TableStatus::Unsupported;
        }
        let Some(local) = local else {
            return TableStatus::Absent;
        };

        if local.sha256 != remote.sha256 {
            TableStatus::Stale
        } else if !self.dir().join(&local.file).is_file() {
            TableStatus::FileMissing
        } else {
            TableStatus::Current
        }
    }

    /// The local manifest, with "never published to" read as `None` rather than
    /// as a failure.
    fn local_manifest(&self) -> Result<Option<Manifest>, ManifestError> {
        match self.manifest() {
            Ok(manifest) => Ok(Some(manifest)),
            Err(ManifestError::Missing(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Decide what to download: every remote table whose sha256 differs from
    /// the local manifest or whose file went missing (all of them under
    /// [`force`](UpdateOptions::force)). Remote ids this build does not know,
    /// and tables in a format it cannot open, are recorded in `report` and
    /// skipped; a malformed remote filename is fatal.
    fn plan<'a, E>(
        &self,
        remote: &'a Manifest,
        options: UpdateOptions,
        report: &mut UpdateReport,
    ) -> Result<Vec<PlannedDownload<'a>>, UpdateError<E>> {
        let local = self.local_manifest()?;

        let mut planned = Vec::new();
        for (id, entry) in &remote.tables {
            let Some(table) = Table::from_id(id) else {
                report.unknown_tables.push(id.clone());
                continue;
            };

            let status = self.status_of(local.as_ref().and_then(|m| m.entry(table)), entry);
            // Downloading a table this build cannot open would replace a working
            // pointer with an unreadable one, so leave the local version alone.
            if status == TableStatus::Unsupported {
                report.unsupported_tables.push(UnsupportedTable {
                    table,
                    format_version: entry.format_version,
                });
                continue;
            }
            if !status.needs_update() && !options.force {
                continue;
            }

            let version =
                version_of(table, &entry.file).ok_or_else(|| UpdateError::BadRemoteFilename {
                    id: id.clone(),
                    file: entry.file.clone(),
                })?;

            planned.push(PlannedDownload {
                table,
                version,
                entry,
            });
        }

        Ok(planned)
    }

    /// Where one planned download is staged, registered for cleanup before a
    /// single byte is written so a failure mid-stream litters nothing.
    fn stage_path(&self, download: &PlannedDownload<'_>, staged: &mut Staged) -> PathBuf {
        let tmp = self
            .dir()
            .join(format!("{}.download.tmp", download.entry.file));
        staged.0.push(tmp.clone());

        tmp
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

/// Failure to obtain the remote manifest, before either caller has wrapped it in
/// its own error type.
pub(crate) enum ManifestFetch<E> {
    Fetch { file: String, source: FetchError<E> },
    Parse(ManifestError),
}

impl<E> From<ManifestError> for ManifestFetch<E> {
    fn from(e: ManifestError) -> Self {
        Self::Parse(e)
    }
}

impl<E> From<ManifestFetch<E>> for UpdateError<E> {
    fn from(e: ManifestFetch<E>) -> Self {
        match e {
            ManifestFetch::Fetch { file, source } => Self::Fetch { file, source },
            ManifestFetch::Parse(e) => Self::Manifest(e),
        }
    }
}

impl<E> From<ManifestFetch<E>> for CheckError<E> {
    fn from(e: ManifestFetch<E>) -> Self {
        match e {
            ManifestFetch::Fetch { file, source } => Self::Fetch { file, source },
            ManifestFetch::Parse(e) => Self::Manifest(e),
        }
    }
}

/// Fetch the remote manifest from this build's format channel, falling back to
/// the unversioned asset.
///
/// The channel is what keeps an old build updating across a format bump: it asks
/// for the manifest describing the format it can read, and a release that still
/// builds that format still publishes one. The fallback covers releases made
/// before channels existed, so a failure there says nothing the channel error
/// did not already say - the caller sees the first one.
fn fetch_manifest<F: Fetch + ?Sized>(remote: &F) -> Result<Manifest, ManifestFetch<F::Error>> {
    let channel = Manifest::asset_for_format(ltk_hashdb::FORMAT_VERSION);
    match remote.fetch(&channel) {
        Ok(bytes) => Ok(Manifest::from_slice(&bytes)?),
        Err(source) => match remote.fetch(MANIFEST_FILE) {
            Ok(bytes) => Ok(Manifest::from_slice(&bytes)?),
            Err(_) => Err(ManifestFetch::Fetch {
                file: channel,
                source,
            }),
        },
    }
}

/// Async twin of [`fetch_manifest`].
async fn fetch_manifest_async<F: AsyncFetch + ?Sized>(
    remote: &F,
) -> Result<Manifest, ManifestFetch<F::Error>> {
    let channel = Manifest::asset_for_format(ltk_hashdb::FORMAT_VERSION);
    // `fetch_to` rather than `fetch`: the required method carries its own `Send`
    // guarantee, so this works for a fetcher that is not `Sync`.
    let mut buf = Vec::new();
    match remote.fetch_to(&channel, &mut buf).await {
        Ok(_) => Ok(Manifest::from_slice(&buf)?),
        Err(source) => {
            let mut buf = Vec::new();
            match remote.fetch_to(MANIFEST_FILE, &mut buf).await {
                Ok(_) => Ok(Manifest::from_slice(&buf)?),
                Err(_) => Err(ManifestFetch::Fetch {
                    file: channel,
                    source,
                }),
            }
        }
    }
}

/// A buffered, hashing sink over a fresh staging file.
///
/// Hashing as the bytes go past is what removes the second pass: the download is
/// verified by the time it is on disk, and `commit` renames it rather than
/// copying it and reading it again.
fn staging_sink(tmp: &Path) -> std::io::Result<fsutil::HashingWriter<BufWriter<File>>> {
    Ok(fsutil::HashingWriter::new(BufWriter::new(File::create(
        tmp,
    )?)))
}

/// Stream one asset into `sink` and return its hex sha256.
fn fetch_into<F: Fetch + ?Sized>(
    remote: &F,
    filename: &str,
    sink: &mut fsutil::HashingWriter<BufWriter<File>>,
) -> Result<String, UpdateError<F::Error>> {
    remote
        .fetch_to(filename, sink)
        .map_err(|source| UpdateError::Fetch {
            file: filename.to_string(),
            source,
        })?;

    Ok(sink.finish()?)
}

/// Async twin of [`fetch_into`].
async fn fetch_into_async<F: AsyncFetch + ?Sized>(
    remote: &F,
    filename: &str,
    sink: &mut fsutil::HashingWriter<BufWriter<File>>,
) -> Result<String, UpdateError<F::Error>> {
    remote
        .fetch_to(filename, sink)
        .await
        .map_err(|source| UpdateError::Fetch {
            file: filename.to_string(),
            source,
        })?;

    Ok(sink.finish()?)
}

/// Turn a staged download into a [`CommitItem`], or reject it.
///
/// The digest came off the stream, so this is a string compare rather than
/// another pass over 38 MiB.
fn verified<E>(
    download: &PlannedDownload<'_>,
    tmp: &Path,
    sha256: String,
) -> Result<CommitItem, UpdateError<E>> {
    if sha256 != download.entry.sha256 {
        return Err(UpdateError::ChecksumMismatch {
            file: download.entry.file.clone(),
            expected: download.entry.sha256.clone(),
            actual: sha256,
        });
    }

    // The release says where this table came from; that travels with the table
    // rather than being re-derived from whatever run installs it.
    let mut item = CommitItem::staged(download.table, download.version, tmp, sha256);
    item.source = download.entry.source.clone();

    Ok(item)
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
