//! [`HashStore`]: the shared cache directory and everything that reads or mutates it -
//! opening the active table, committing new immutable versions, and GC'ing old ones.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ltk_hashdb::{HashDb, KeyConfig, LayeredHashDb, WeakHashDb};

use crate::manifest::{version_of, Manifest, Source, TableEntry};
use crate::{
    dir, fsutil, CommitError, GcError, LockHolder, ManifestError, NoCacheDirError, OpenError,
    Table, UniverseMismatch, UpdateLock,
};

/// The manifest filename inside the cache directory.
pub(crate) const MANIFEST_FILE: &str = "manifest.json";
/// Extension for published table files (League Toolkit convention).
const TABLE_EXT: &str = "lhdb";
/// Longest accepted version label, in bytes.
const MAX_VERSION_LEN: usize = 64;

/// A shared, versioned, multi-process cache of hash tables rooted at one directory.
///
/// Construction is cheap and does not touch the filesystem; the directory is created
/// on the first [`try_lock_update`](HashStore::try_lock_update).
///
/// # Directory contract
///
/// Tables are memory-mapped, which is only sound while their bytes do not change.
/// This crate never modifies a table file after writing it: new versions get new
/// names, and [`gc`](HashStore::gc) only deletes. Other programs must not modify
/// `.lhdb` files in the cache directory either; change it through this crate or
/// the `mimir` CLI.
///
/// Readers need no coordination. [`commit`](HashStore::commit) and
/// [`gc`](HashStore::gc) take the [`UpdateLock`], so only one process changes the
/// cache at a time.
///
/// A store also remembers the tables it has opened through
/// [`open_shared`](HashStore::open_shared), weakly - clones of a store share that
/// register, two stores built separately do not, and a table drops out of it as soon as
/// the last handle to it does.
#[derive(Debug, Clone)]
pub struct HashStore {
    dir: PathBuf,

    /// Tables opened through `open_shared`, keyed by their active file. Weak, so the
    /// register never keeps a superseded version mapped.
    opened: Arc<Mutex<HashMap<PathBuf, WeakHashDb>>>,
}

/// One table to install in a [`commit`](HashStore::commit) call.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CommitItem {
    /// Which logical table this file is.
    pub table: Table,

    /// Version label used in the immutable filename (`<table>-<version>.lhdb`), e.g. a
    /// date or patch string. 1-64 characters from `[A-Za-z0-9._-]`, not starting
    /// with `.`.
    pub version: String,

    /// The freshly built `.lhdb` to install; copied into the cache under its
    /// versioned name.
    pub path: PathBuf,

    /// Where this table's inputs came from, recorded on its
    /// [`TableEntry`](crate::TableEntry). Falls back to the run-wide source
    /// [`commit`](HashStore::commit) is given.
    pub source: Option<Source>,

    /// Set when `path` is a file staged inside the cache directory for this
    /// commit to consume, carrying the digest of its contents.
    ///
    /// [`commit`](HashStore::commit) then renames the file into place and trusts
    /// this digest, rather than copying the bytes and reading them back to hash
    /// them - which for a 38 MiB table is the difference between touching it
    /// once and touching it three times. Set it through
    /// [`staged`](CommitItem::staged), never by hand: a wrong digest is recorded
    /// in the manifest as if it were right.
    pub staged_sha256: Option<String>,
}

impl CommitItem {
    /// A table built somewhere else, to be copied into the cache.
    pub fn new(table: Table, version: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            table,
            version: version.into(),
            path: path.into(),
            source: None,
            staged_sha256: None,
        }
    }

    /// A table already staged inside the cache directory, with `sha256` computed
    /// over the file as it was written.
    ///
    /// [`commit`](HashStore::commit) consumes it: the file is renamed into place
    /// and is gone from `path` afterwards either way. This is what the updater
    /// uses, having hashed the download as it streamed.
    pub fn staged(
        table: Table,
        version: impl Into<String>,
        path: impl Into<PathBuf>,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            staged_sha256: Some(sha256.into()),
            ..Self::new(table, version, path)
        }
    }

    /// Record where this table in particular was built from, overriding the
    /// run-wide source.
    pub fn with_source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }
}

/// What [`HashStore::gc`] did.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct GcReport {
    /// Files that were deleted.
    pub deleted: Vec<PathBuf>,

    /// Files the OS refused to delete, usually because a reader still has them
    /// mapped on Windows. A later `gc` retries them.
    pub retained: Vec<PathBuf>,
}

impl HashStore {
    /// Resolve the cache directory from the environment / platform. Does not
    /// create it.
    pub fn discover() -> Result<Self, NoCacheDirError> {
        Ok(Self::at(dir::resolve()?))
    }

    /// Use an explicit cache directory (tests, `--dir` overrides).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            opened: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path to `manifest.json`.
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join(MANIFEST_FILE)
    }

    /// Read and parse the manifest. Errors with [`ManifestError::Missing`] if the cache
    /// has never been published to.
    pub fn manifest(&self) -> Result<Manifest, ManifestError> {
        Manifest::read(self.manifest_path())
    }

    /// The active on-disk path for `table`, per the manifest.
    ///
    /// # Errors
    ///
    /// [`OpenError::Manifest`] if the manifest cannot be read,
    /// [`OpenError::TableNotFound`] if it has no entry for `table`, and
    /// [`OpenError::InvalidFilename`] if the entry's filename is not
    /// `<table>-<version>.lhdb`, so a manifest cannot point outside the cache.
    pub fn path_for(&self, table: Table) -> Result<PathBuf, OpenError> {
        let manifest = self.manifest()?;
        let entry = manifest
            .entry(table)
            .ok_or(OpenError::TableNotFound(table))?;
        if version_of(table, &entry.file).is_none() {
            return Err(OpenError::InvalidFilename {
                table,
                file: entry.file.clone(),
            });
        }

        Ok(self.dir.join(&entry.file))
    }

    /// Open the active version of `table` read-only (manifest → active file → mmap).
    ///
    /// Structure is validated on open; the download-time sha256 in the manifest is
    /// trusted, so this stays cheap and lazy. Use [`HashDb::verify`] for a full
    /// checksum pass.
    ///
    /// Every call maps the file afresh. Use [`open_shared`](HashStore::open_shared)
    /// when the same table may already be open in this process.
    ///
    /// # Errors
    ///
    /// As [`path_for`](HashStore::path_for), plus [`OpenError::HashDb`] if the
    /// file cannot be opened or fails validation, and
    /// [`OpenError::WrongKeyConfig`] if its key config is not
    /// [`Table::key_config`].
    pub fn open(&self, table: Table) -> Result<HashDb, OpenError> {
        retry_if_swept(|| {
            let db = HashDb::open(self.path_for(table)?)?;
            check_key_config(table, &db).map_err(|(expected, found)| {
                OpenError::WrongKeyConfig {
                    table,
                    expected,
                    found,
                }
            })?;

            Ok(db)
        })
    }

    /// Open the active version of `table`, reusing a handle this store already has.
    ///
    /// [`open`](HashStore::open) maps the file and parses its seek table every time -
    /// on `game` that is over ten thousand frame records for a table the process may
    /// already have open. This hands back the existing handle instead, and because the
    /// register is keyed on the manifest's active filename, an update published in the
    /// meantime opens the new version by itself rather than serving the old one.
    ///
    /// Prefer it wherever a table is opened more than once. The returned handle is a
    /// [`HashDb`] like any other - cheap to clone, shared frame cache.
    ///
    /// # Errors
    ///
    /// Fails like [`open`](HashStore::open): a missing manifest, a table the manifest
    /// does not carry, or a file that does not validate.
    pub fn open_shared(&self, table: Table) -> Result<HashDb, OpenError> {
        retry_if_swept(|| self.open_shared_once(table))
    }

    fn open_shared_once(&self, table: Table) -> Result<HashDb, OpenError> {
        let path = self.path_for(table)?;
        if let Some(db) = self.registered(&path) {
            return Ok(db);
        }

        // Opened outside the lock: two threads racing on one table each get a working
        // handle and the loser's simply isn't the one registered, which is cheaper than
        // holding a lock across an mmap and a seek-table parse.
        let db = HashDb::open(&path)?;
        check_key_config(table, &db).map_err(|(expected, found)| OpenError::WrongKeyConfig {
            table,
            expected,
            found,
        })?;

        let mut opened = self.opened.lock().unwrap_or_else(PoisonError::into_inner);
        opened.insert(path, db.downgrade());
        // Superseded versions leave dead entries behind; sweep them while we hold the
        // lock, so a long-lived store doesn't accumulate one per update.
        opened.retain(|_, weak| weak.upgrade().is_some());

        Ok(db)
    }

    fn registered(&self, path: &Path) -> Option<HashDb> {
        let opened = self.opened.lock().unwrap_or_else(PoisonError::into_inner);
        opened.get(path).and_then(WeakHashDb::upgrade)
    }

    /// Open several tables, pairing each with its result so callers can warn-and-skip
    /// missing ones instead of aborting on the first error. Results are returned in
    /// `tables` order.
    pub fn open_many(&self, tables: &[Table]) -> Vec<(Table, Result<HashDb, OpenError>)> {
        tables.iter().map(|&t| (t, self.open(t))).collect()
    }

    /// Open `tables`, layer the ones that opened into a [`LayeredHashDb`] (in the
    /// given priority order - earlier tables shadow later ones), and return the
    /// per-table open errors for the caller to log.
    ///
    /// A tool stays usable when a table is missing: its hashes just miss. This is
    /// the shape most WAD consumers want - e.g.
    /// `open_layered(&[Table::Game, Table::Lcu])`.
    ///
    /// Tables are opened through [`open_shared`](HashStore::open_shared), so calling
    /// this twice does not map anything twice.
    ///
    /// # Errors
    ///
    /// [`UniverseMismatch`] if `tables` spans more than one
    /// [`HashUniverse`](crate::HashUniverse) - `binentries` under `binfields`, say,
    /// where one table would answer the other's hashes with an unrelated path. That
    /// is decided before anything is opened.
    ///
    /// A table that is missing, unreadable, or keyed differently than it claims is
    /// *not* an error here: it lands in the returned per-table list and is left out
    /// of the layer.
    pub fn open_layered(
        &self,
        tables: &[Table],
    ) -> Result<(LayeredHashDb, Vec<(Table, OpenError)>), UniverseMismatch> {
        if let Some((&first, rest)) = tables.split_first() {
            let expected = first.universe();
            if let Some(&table) = rest.iter().find(|t| t.universe() != expected) {
                return Err(UniverseMismatch {
                    first,
                    expected,
                    table,
                    found: table.universe(),
                });
            }
        }

        let mut layered = LayeredHashDb::new();
        let mut errors = Vec::new();
        for &table in tables {
            match self.open_shared(table) {
                // One universe implies one key config, so this only fires on a file
                // that is not the table it is filed under - a mislabelled download,
                // not a caller mistake. Skip it like any other unusable table.
                Ok(db) => {
                    if let Err(e) = layered.push_base(db) {
                        errors.push((table, e.into()));
                    }
                }
                Err(e) => errors.push((table, e)),
            }
        }

        Ok((layered, errors))
    }

    /// Try to become the single updater without blocking. `Ok(None)` means another
    /// process is already updating. Hold the returned guard across
    /// download/build/[`commit`](HashStore::commit)/[`gc`](HashStore::gc).
    ///
    /// Ask [`lock_holder`](HashStore::lock_holder) who that other process is
    /// before telling a user to wait for it.
    pub fn try_lock_update(&self) -> std::io::Result<Option<UpdateLock>> {
        std::fs::create_dir_all(&self.dir)?;
        UpdateLock::try_acquire(&self.dir)
    }

    /// Become the single updater, waiting up to `timeout` for the current one to
    /// finish.
    ///
    /// For a tool that would rather queue behind a running update than tell the
    /// user to try again - a setup script, say. `Ok(None)` means the timeout ran
    /// out and someone still holds it. A zero timeout is exactly
    /// [`try_lock_update`](HashStore::try_lock_update).
    pub fn lock_update_timeout(&self, timeout: Duration) -> std::io::Result<Option<UpdateLock>> {
        std::fs::create_dir_all(&self.dir)?;
        UpdateLock::acquire_timeout(&self.dir, timeout)
    }

    /// Who is updating this cache right now, if anyone.
    ///
    /// `Ok(None)` means nobody holds the lock - not that the answer is unknown.
    /// A held lock whose body is missing or unreadable also reads as `None`,
    /// since the body is written best-effort and nothing depends on it.
    ///
    /// The pid can name a process that has since died: the OS releases the lock
    /// when it does, so this reports `None` again from that moment.
    pub fn lock_holder(&self) -> std::io::Result<Option<LockHolder>> {
        UpdateLock::holder(&self.dir)
    }

    /// Install one or more freshly built tables and atomically switch the manifest
    /// to them.
    ///
    /// Each source is installed under an immutable `<table>-<version>.lhdb` name:
    /// copied, or renamed when the item was staged in this directory
    /// ([`CommitItem::staged`]). The manifest is replaced only after every file
    /// is on disk, so a reader never sees a manifest pointing at a partial table.
    /// `lock` must come from this store's
    /// [`try_lock_update`](HashStore::try_lock_update) or
    /// [`lock_update_timeout`](HashStore::lock_update_timeout); readers need no
    /// coordination.
    ///
    /// `source` describes the run and lands in
    /// [`Manifest::last_run`](crate::Manifest::last_run); it is also the
    /// provenance recorded for any item that does not carry its own
    /// ([`CommitItem::with_source`]). Tables this run does not touch keep the
    /// provenance they were installed with - a run that publishes `game` must
    /// not restamp the seven tables built from something else.
    ///
    /// Committing zero items still refreshes the timestamp and the run record.
    ///
    /// # Errors
    ///
    /// - [`CommitError::InvalidVersion`] for a bad version label
    /// - [`CommitError::HashDb`] / [`CommitError::WrongKeyConfig`] if a file is not a
    ///   valid table with the table's [`key_config`](Table::key_config)
    /// - [`CommitError::VersionReused`] if the versioned file already exists with
    ///   different content
    /// - [`CommitError::Io`] / [`CommitError::Manifest`] for I/O failures
    ///
    /// On error the manifest is unchanged.
    pub fn commit(
        &self,
        lock: &UpdateLock,
        items: &[CommitItem],
        source: Option<Source>,
    ) -> Result<Manifest, CommitError> {
        // Holding the lock is the requirement; there is nothing else to check.
        let _ = lock;
        std::fs::create_dir_all(&self.dir)?;

        // Start from the current manifest so unpublished tables keep their pointers.
        let mut manifest = match self.manifest() {
            Ok(m) => m,
            Err(ManifestError::Missing(_)) => Manifest::empty(),
            Err(e) => return Err(e.into()),
        };
        manifest.generated_at = crate::manifest::now_rfc3339();
        manifest.last_run = source.clone();

        for item in items {
            if !is_valid_version(&item.version) {
                return Err(CommitError::InvalidVersion(item.version.clone()));
            }
            let filename = format!("{}-{}.{}", item.table.id(), item.version, TABLE_EXT);
            let dest = self.dir.join(&filename);

            // Opening the built file validates it and yields entry count + key width.
            let (entries, key_width) = {
                let db = HashDb::open(&item.path)?;
                check_key_config(item.table, &db).map_err(|(expected, found)| {
                    CommitError::WrongKeyConfig {
                        table: item.table,
                        expected,
                        found,
                    }
                })?;
                (db.len() as u64, db.key_width().bytes() as u8)
            };

            // Published versions are immutable, so the file may already exist (a
            // `--force` refresh). Same bytes: no-op - never rename over it, a reader
            // may hold it mmap'd (fails on Windows). Different bytes: upstream reused
            // a version label; refuse.
            let sha256 = if dest.exists() {
                let existing = fsutil::sha256_file(&dest)?;
                let incoming = match &item.staged_sha256 {
                    Some(sha256) => sha256.clone(),
                    None => fsutil::sha256_file(&item.path)?,
                };
                if existing != incoming {
                    return Err(CommitError::VersionReused {
                        table: item.table,
                        version: item.version.clone(),
                    });
                }

                // A staged file is ours to dispose of, and its twin is already
                // installed. A file built elsewhere is the caller's; leave it.
                if item.staged_sha256.is_some() {
                    let _ = std::fs::remove_file(&item.path);
                }

                existing
            } else if let Some(sha256) = &item.staged_sha256 {
                // Already in this directory and already hashed: an in-volume
                // move, no second pass over the bytes.
                fsutil::rename_into_place(&item.path, &dest)?;
                sha256.clone()
            } else {
                fsutil::atomic_copy(&item.path, &dest)?;
                fsutil::sha256_file(&dest)?
            };

            let size_bytes = std::fs::metadata(&dest)?.len();

            manifest.tables.insert(
                item.table.id().to_string(),
                TableEntry {
                    file: filename,
                    sha256,
                    entries,
                    key_width,
                    size_bytes: Some(size_bytes),
                    version: item.version.clone(),
                    source: item.source.clone().or_else(|| source.clone()),
                    // We just opened it, and `open` is what enforces the version.
                    format_version: ltk_hashdb::FORMAT_VERSION,
                },
            );
        }

        manifest.write_atomic(self.manifest_path())?;
        Ok(manifest)
    }

    /// Delete versioned `.lhdb` files the manifest no longer references, and
    /// leftover `.tmp` files from interrupted writes.
    ///
    /// `lock` must come from this store's
    /// [`try_lock_update`](HashStore::try_lock_update) or
    /// [`lock_update_timeout`](HashStore::lock_update_timeout). It guarantees
    /// that no other process is writing a `.tmp` file this would delete.
    ///
    /// Files the OS refuses to delete (on Windows, files a reader still has
    /// mapped) are listed in [`GcReport::retained`] and retried on a later run.
    /// Without a manifest nothing is deleted.
    ///
    /// # Errors
    ///
    /// [`GcError::Manifest`] if the manifest cannot be read, [`GcError::Io`] if
    /// the directory cannot be listed.
    pub fn gc(&self, lock: &UpdateLock) -> Result<GcReport, GcError> {
        // Holding the lock is the requirement; there is nothing else to check.
        let _ = lock;
        let manifest = match self.manifest() {
            Ok(m) => m,
            Err(ManifestError::Missing(_)) => return Ok(GcReport::default()),
            Err(e) => return Err(e.into()),
        };
        let referenced: std::collections::HashSet<&str> =
            manifest.tables.values().map(|t| t.file.as_str()).collect();

        let mut report = GcReport::default();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };

            let is_table = name.ends_with(&format!(".{TABLE_EXT}")) && !referenced.contains(name);
            let is_stray_tmp = name.ends_with(".tmp");
            if !is_table && !is_stray_tmp {
                continue;
            }

            let path = entry.path();
            match std::fs::remove_file(&path) {
                Ok(()) => report.deleted.push(path),
                // A still-mapped file (Windows) or a transient race - keep it, retry next time.
                Err(_) => report.retained.push(path),
            }
        }
        Ok(report)
    }
}

/// A version label is 1-64 characters from `[A-Za-z0-9._-]` and does not start
/// with `.`, so `<table>-<version>.lhdb` is a plain filename on every platform:
/// no separators, no `:` (an NTFS alternate data stream), no control characters.
pub(crate) fn is_valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= MAX_VERSION_LEN
        && !version.starts_with('.')
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Reject a table file whose header does not match `table`'s key config, such
/// as a bin table filed under `game`. Returns the expected and found configs.
fn check_key_config(table: Table, db: &HashDb) -> Result<(), (KeyConfig, KeyConfig)> {
    let (expected, found) = (table.key_config(), db.key_config());
    if expected == found {
        Ok(())
    } else {
        Err((expected, found))
    }
}

/// Run `open` once more if the file vanished: an update can replace the manifest
/// and delete the file it named between our manifest read and our open, and the
/// new manifest names a file that exists.
fn retry_if_swept(
    mut open: impl FnMut() -> Result<HashDb, OpenError>,
) -> Result<HashDb, OpenError> {
    match open() {
        Err(OpenError::HashDb(ltk_hashdb::OpenError::Io(e))) if e.kind() == ErrorKind::NotFound => {
            open()
        }
        res => res,
    }
}
