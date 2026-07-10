//! [`HashStore`]: the shared cache directory and everything that reads or mutates it -
//! opening the active table, publishing new immutable versions, and GC'ing old ones.

use std::path::{Path, PathBuf};

use ltk_hashdb::HashDb;

use crate::manifest::{Manifest, Source, TableEntry};
use crate::{dir, fsutil, Error, Result, Table, UpdateLock};

/// The manifest filename inside the cache directory.
pub(crate) const MANIFEST_FILE: &str = "manifest.json";
/// The single-updater lock filename.
const UPDATE_LOCK_FILE: &str = ".update.lock";
/// Extension for published table files (League Toolkit convention).
const TABLE_EXT: &str = "lhdb";

/// A shared, versioned, multi-process cache of hash tables rooted at one directory.
///
/// Construction is cheap and does not touch the filesystem; the directory is created
/// lazily on the first [`publish`](HashStore::publish).
#[derive(Debug, Clone)]
pub struct HashStore {
    dir: PathBuf,
}

/// One table to install in a [`publish`](HashStore::publish) call.
#[derive(Debug, Clone)]
pub struct PublishItem {
    /// Which logical table this file is.
    pub table: Table,

    /// Version label used in the immutable filename (`<table>-<version>.lhdb`), e.g. a
    /// date or patch string. Must be non-empty and free of path separators.
    pub version: String,

    /// The freshly built `.lhdb` to install; copied into the cache under its versioned
    /// name. May live anywhere (typically a temp file from the build step).
    pub path: PathBuf,
}

impl PublishItem {
    pub fn new(table: Table, version: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            table,
            version: version.into(),
            path: path.into(),
        }
    }
}

/// What [`HashStore::gc`] did: files removed vs. files it left in place because the OS
/// refused to delete them (still mapped by a reader - the common Windows case).
#[derive(Debug, Default, Clone)]
pub struct GcReport {
    pub deleted: Vec<PathBuf>,
    pub retained: Vec<PathBuf>,
}

impl HashStore {
    /// Resolve the cache directory from the environment / platform. Does not
    /// create it.
    pub fn discover() -> Result<Self> {
        Ok(Self {
            dir: dir::resolve()?,
        })
    }

    /// Use an explicit cache directory (tests, `--dir` overrides).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path to `manifest.json`.
    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join(MANIFEST_FILE)
    }

    /// Read and parse the manifest. Errors with [`Error::MissingManifest`] if the cache
    /// has never been published to.
    pub fn manifest(&self) -> Result<Manifest> {
        Manifest::read(&self.manifest_path())
    }

    /// The active on-disk path for `table`, per the manifest.
    pub fn path_for(&self, table: Table) -> Result<PathBuf> {
        let manifest = self.manifest()?;
        let entry = manifest.entry(table).ok_or(Error::TableNotFound(table))?;
        Ok(self.dir.join(&entry.file))
    }

    /// Open the active version of `table` read-only (manifest → active file → mmap).
    ///
    /// Structure is validated on open; the download-time sha256 in the manifest is
    /// trusted, so this stays cheap and lazy. Use [`HashDb::verify`] for a full
    /// checksum pass.
    pub fn open(&self, table: Table) -> Result<HashDb> {
        Ok(HashDb::open(self.path_for(table)?)?)
    }

    /// Try to become the single updater without blocking. `Ok(None)` means another
    /// process is already updating. Hold the returned guard across
    /// download/build/[`publish`](HashStore::publish)/[`gc`](HashStore::gc).
    pub fn try_lock_update(&self) -> Result<Option<UpdateLock>> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(UpdateLock::try_acquire(&self.dir.join(UPDATE_LOCK_FILE))?)
    }

    /// Install one or more freshly built tables and atomically flip the manifest to
    /// point at them.
    ///
    /// Each source is copied into the cache under an immutable `<table>-<version>.lhdb`
    /// name; only after every file is durable is the manifest swapped, so a reader never
    /// sees a pointer to a partially written table. Callers that also mutate the cache
    /// concurrently should hold [`try_lock_update`](HashStore::try_lock_update); readers
    /// need no coordination.
    ///
    /// Returns the new manifest. Publishing zero items is a no-op that still refreshes
    /// the manifest timestamp (and `source`, if given).
    pub fn publish(&self, items: &[PublishItem], source: Option<Source>) -> Result<Manifest> {
        std::fs::create_dir_all(&self.dir)?;

        // Start from the current manifest so unpublished tables keep their pointers.
        let mut manifest = match self.manifest() {
            Ok(m) => m,
            Err(Error::MissingManifest(_)) => Manifest::empty(),
            Err(e) => return Err(e),
        };
        manifest.generated_at = crate::manifest::now_rfc3339();
        if source.is_some() {
            manifest.source = source;
        }

        for item in items {
            validate_version(&item.version)?;
            let filename = format!("{}-{}.{}", item.table.id(), item.version, TABLE_EXT);
            let dest = self.dir.join(&filename);

            // Read entry count + key width straight from the built file (this also
            // validates it is a well-formed .hashdb before we commit to it).
            let (entries, key_width) = {
                let db = HashDb::open(&item.path)?;
                (db.len() as u64, db.key_width().bytes() as u8)
            };

            fsutil::atomic_copy(&item.path, &dest)?;
            let sha256 = fsutil::sha256_file(&dest)?;

            manifest.tables.insert(
                item.table.id().to_string(),
                TableEntry {
                    file: filename,
                    sha256,
                    entries,
                    key_width,
                },
            );
        }

        manifest.write_atomic(&self.manifest_path())?;
        Ok(manifest)
    }

    /// Delete versioned `.lhdb` files no longer referenced by the manifest, plus stray
    /// `.tmp` leftovers. Files the OS refuses to delete - still mapped by a
    /// reader, which on Windows fails - are left in place and reported in
    /// [`GcReport::retained`] to retry on a later run. A missing manifest deletes nothing.
    pub fn gc(&self) -> Result<GcReport> {
        let manifest = match self.manifest() {
            Ok(m) => m,
            Err(Error::MissingManifest(_)) => return Ok(GcReport::default()),
            Err(e) => return Err(e),
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

/// A version label must be a single path component: non-empty and free of separators,
/// so `<table>-<version>.lhdb` can never escape the cache directory.
fn validate_version(version: &str) -> Result<()> {
    let bad = version.is_empty()
        || version.contains('/')
        || version.contains('\\')
        || version.contains(std::path::MAIN_SEPARATOR);
    if bad {
        return Err(Error::InvalidVersion(version.to_string()));
    }
    Ok(())
}
