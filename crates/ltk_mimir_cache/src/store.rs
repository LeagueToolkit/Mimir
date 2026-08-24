//! [`HashStore`]: the shared cache directory and everything that reads or mutates it -
//! opening the active table, committing new immutable versions, and GC'ing old ones.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ltk_hashdb::{HashDb, LayeredHashDb, WeakHashDb};

use crate::manifest::{Manifest, Source, TableEntry};
use crate::{
    dir, fsutil, CommitError, GcError, ManifestError, NoCacheDirError, OpenError, Table,
    UniverseMismatch, UpdateLock,
};

/// The manifest filename inside the cache directory.
pub(crate) const MANIFEST_FILE: &str = "manifest.json";
/// The single-updater lock filename.
const UPDATE_LOCK_FILE: &str = ".update.lock";
/// Extension for published table files (League Toolkit convention).
const TABLE_EXT: &str = "lhdb";

/// A shared, versioned, multi-process cache of hash tables rooted at one directory.
///
/// Construction is cheap and does not touch the filesystem; the directory is created
/// lazily on the first [`commit`](HashStore::commit).
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
pub struct CommitItem {
    /// Which logical table this file is.
    pub table: Table,

    /// Version label used in the immutable filename (`<table>-<version>.lhdb`), e.g. a
    /// date or patch string. Must be non-empty and free of path separators.
    pub version: String,

    /// The freshly built `.lhdb` to install; copied into the cache under its
    /// versioned name.
    pub path: PathBuf,
}

impl CommitItem {
    pub fn new(table: Table, version: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            table,
            version: version.into(),
            path: path.into(),
        }
    }
}

/// What [`HashStore::gc`] did: files removed vs. files the OS refused to delete
/// (still mmap'd by a reader - the common Windows case).
#[derive(Debug, Default, Clone)]
pub struct GcReport {
    pub deleted: Vec<PathBuf>,
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
        Manifest::read(&self.manifest_path())
    }

    /// The active on-disk path for `table`, per the manifest.
    pub fn path_for(&self, table: Table) -> Result<PathBuf, OpenError> {
        let manifest = self.manifest()?;
        let entry = manifest
            .entry(table)
            .ok_or(OpenError::TableNotFound(table))?;
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
    pub fn open(&self, table: Table) -> Result<HashDb, OpenError> {
        Ok(HashDb::open(self.path_for(table)?)?)
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
        let path = self.path_for(table)?;
        if let Some(db) = self.registered(&path) {
            return Ok(db);
        }

        // Opened outside the lock: two threads racing on one table each get a working
        // handle and the loser's simply isn't the one registered, which is cheaper than
        // holding a lock across an mmap and a seek-table parse.
        let db = HashDb::open(&path)?;

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
    pub fn try_lock_update(&self) -> std::io::Result<Option<UpdateLock>> {
        std::fs::create_dir_all(&self.dir)?;
        UpdateLock::try_acquire(&self.dir.join(UPDATE_LOCK_FILE))
    }

    /// Install one or more freshly built tables and atomically flip the manifest to
    /// point at them.
    ///
    /// Each source is copied in under an immutable `<table>-<version>.lhdb` name;
    /// the manifest is swapped only after every file is durable, so a reader never
    /// sees a pointer to a partial table. Concurrent mutators should hold
    /// [`try_lock_update`](HashStore::try_lock_update); readers need no coordination.
    ///
    /// Committing zero items still refreshes the timestamp and replaces `source`,
    /// so the manifest always describes the last commit.
    pub fn commit(
        &self,
        items: &[CommitItem],
        source: Option<Source>,
    ) -> Result<Manifest, CommitError> {
        std::fs::create_dir_all(&self.dir)?;

        // Start from the current manifest so unpublished tables keep their pointers.
        let mut manifest = match self.manifest() {
            Ok(m) => m,
            Err(ManifestError::Missing(_)) => Manifest::empty(),
            Err(e) => return Err(e.into()),
        };
        manifest.generated_at = crate::manifest::now_rfc3339();
        manifest.source = source;

        for item in items {
            if !is_valid_version(&item.version) {
                return Err(CommitError::InvalidVersion(item.version.clone()));
            }
            let filename = format!("{}-{}.{}", item.table.id(), item.version, TABLE_EXT);
            let dest = self.dir.join(&filename);

            // Opening the built file validates it and yields entry count + key width.
            let (entries, key_width) = {
                let db = HashDb::open(&item.path)?;
                (db.len() as u64, db.key_width().bytes() as u8)
            };

            // Published versions are immutable, so the file may already exist (a
            // `--force` refresh). Same bytes: no-op - never rename over it, a reader
            // may hold it mmap'd (fails on Windows). Different bytes: upstream reused
            // a version label; refuse.
            let sha256 = if dest.exists() {
                let existing = fsutil::sha256_file(&dest)?;
                if existing != fsutil::sha256_file(&item.path)? {
                    return Err(CommitError::VersionReused {
                        table: item.table,
                        version: item.version.clone(),
                    });
                }
                existing
            } else {
                fsutil::atomic_copy(&item.path, &dest)?;
                fsutil::sha256_file(&dest)?
            };

            manifest.tables.insert(
                item.table.id().to_string(),
                TableEntry {
                    file: filename,
                    sha256,
                    entries,
                    key_width,
                    // We just opened it, and `open` is what enforces the version.
                    format_version: ltk_hashdb::FORMAT_VERSION,
                },
            );
        }

        manifest.write_atomic(&self.manifest_path())?;
        Ok(manifest)
    }

    /// Delete versioned `.lhdb` files the manifest no longer references, plus stray
    /// `.tmp` leftovers. Files the OS refuses to delete (still mmap'd on Windows) are
    /// reported in [`GcReport::retained`] and retried on a later run. A missing
    /// manifest deletes nothing.
    pub fn gc(&self) -> Result<GcReport, GcError> {
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

/// A version label must be a single path component - non-empty, no separators -
/// so `<table>-<version>.lhdb` can never escape the cache directory.
pub(crate) fn is_valid_version(version: &str) -> bool {
    !version.is_empty()
        && !version.contains('/')
        && !version.contains('\\')
        && !version.contains(std::path::MAIN_SEPARATOR)
}
