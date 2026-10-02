//! Error types for the shared cache, one per operation so each signature
//! names exactly what it can fail with.

use std::path::PathBuf;

use ltk_hashdb::KeyConfig;
use thiserror::Error;

use crate::{HashUniverse, Table};

/// Errors from resolving the platform cache directory
/// ([`HashStore::discover`](crate::HashStore::discover)).
#[derive(Debug, Error)]
#[error("could not determine a platform cache directory")]
pub struct NoCacheDirError;

/// A string that names no [`Table`] ([`Table::from_str`](std::str::FromStr::from_str)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTableError {
    input: String,
}

impl ParseTableError {
    pub(crate) fn new(input: &str) -> Self {
        Self {
            input: input.to_owned(),
        }
    }

    /// The string that failed to parse.
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl std::fmt::Display for ParseTableError {
    /// Lists every accepted id, so a typo is one message away from being fixed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown table {:?}; expected one of ", self.input)?;
        for (i, table) in Table::ALL.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(table.id())?;
        }
        Ok(())
    }
}

impl std::error::Error for ParseTableError {}

/// Errors from reading, parsing, or writing `manifest.json`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// Reading or writing the file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The file is not a valid manifest document.
    #[error("manifest json error")]
    Json(#[from] serde_json::Error),

    /// There is no manifest at this path: nothing was ever committed to the cache.
    #[error("no manifest at {0}")]
    Missing(PathBuf),

    /// The schema version is older than the first published one.
    #[error("manifest schema {0} predates the first published one")]
    UnsupportedSchema(u32),

    /// The manifest needs a newer reader than this build.
    #[error(
        "this manifest requires a reader that understands schema {required}, and this build \
         understands {supported}"
    )]
    ReaderTooOld {
        /// The lowest schema that can read the manifest.
        required: u32,

        /// The schema this build reads.
        supported: u32,
    },
}

/// Errors from opening a cached table ([`HashStore::open`](crate::HashStore::open) /
/// [`HashStore::path_for`](crate::HashStore::path_for)).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The manifest could not be read.
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    /// The manifest has no entry for this table.
    #[error("table {0} is not in the manifest")]
    TableNotFound(Table),

    /// The manifest entry's filename is not `<table>-<version>.lhdb` with a
    /// valid version label.
    #[error("table {table}: invalid filename {file:?} in the manifest")]
    InvalidFilename {
        /// The table whose entry is invalid.
        table: Table,

        /// The filename the manifest gave.
        file: String,
    },

    /// The table file could not be opened or failed validation.
    #[error("opening the table file")]
    HashDb(#[from] ltk_hashdb::OpenError),

    /// The table file's key config is not the table's
    /// [`key_config`](Table::key_config): the file is not the table the
    /// manifest files it under.
    #[error("table {table}: file is keyed {found}, but the table is keyed {expected}")]
    WrongKeyConfig {
        /// The table being opened.
        table: Table,

        /// The table's key config.
        expected: KeyConfig,

        /// The file's key config.
        found: KeyConfig,
    },

    /// A table's key config does not match the tables layered before it in
    /// [`open_layered`](crate::HashStore::open_layered).
    #[error("the table file does not hash its keys the way the layer does")]
    KeyConfig(#[from] ltk_hashdb::KeyConfigMismatch),
}

/// Refusal to layer tables drawn from different hash universes
/// ([`HashStore::open_layered`](crate::HashStore::open_layered)).
#[derive(Debug, Clone, Copy, Error)]
#[error(
    "cannot layer {table} ({found}) with {first} ({expected}): tables from different hash \
     universes would answer each other's hashes with unrelated paths"
)]
#[non_exhaustive]
pub struct UniverseMismatch {
    /// The first table in the requested set, whose universe the rest must match.
    pub first: Table,

    /// That table's universe.
    pub expected: HashUniverse,

    /// The table that does not belong to it.
    pub table: Table,

    /// The universe it belongs to instead.
    pub found: HashUniverse,
}

/// Errors from installing tables ([`HashStore::commit`](crate::HashStore::commit)).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CommitError {
    /// Copying a file or creating the cache directory failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The current manifest could not be read, or the new one written.
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    /// A source file is not a valid table.
    #[error("opening the built table file")]
    HashDb(#[from] ltk_hashdb::OpenError),

    /// A source file's key config is not the table's [`key_config`](Table::key_config).
    #[error("table {table}: built file is keyed {found}, but the table is keyed {expected}")]
    WrongKeyConfig {
        /// The table being installed.
        table: Table,

        /// The table's key config.
        expected: KeyConfig,

        /// The file's key config.
        found: KeyConfig,
    },

    /// A version label is empty, too long, starts with `.`, or uses characters
    /// outside `[A-Za-z0-9._-]`.
    #[error(
        "invalid version label {0:?}: must be 1-64 characters from [A-Za-z0-9._-] \
         and not start with '.'"
    )]
    InvalidVersion(String),

    /// The versioned file already exists with different content.
    #[error(
        "table {table:?}: version {version:?} is already published with different content; \
         published versions are immutable"
    )]
    VersionReused {
        /// The table being installed.
        table: Table,

        /// The reused version label.
        version: String,
    },
}

/// Errors from removing unreferenced files ([`HashStore::gc`](crate::HashStore::gc)).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GcError {
    /// Listing the cache directory failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The manifest could not be read.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

/// Why a streaming fetch stopped ([`Fetch::fetch_to`](crate::Fetch::fetch_to)).
///
/// `Transport` is a failure in the fetcher. `Sink` is a failure writing the
/// bytes: a full disk, or a wrapping sink cancelling the download by refusing
/// the next chunk.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FetchError<E> {
    /// The fetcher failed.
    #[error(transparent)]
    Transport(E),

    /// The sink refused the bytes.
    #[error("writing the fetched bytes")]
    Sink(#[source] std::io::Error),
}

/// Errors from a lock-free comparison ([`HashStore::check`](crate::HashStore::check)).
///
/// Smaller than [`UpdateError`]: `check` installs nothing, so there is no
/// download to verify and no commit to fail.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CheckError<E> {
    /// The local or remote manifest could not be read or parsed.
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    /// Fetching the remote manifest failed.
    #[error("fetching {file}")]
    Fetch {
        /// The filename being fetched.
        file: String,

        /// Why the fetch failed.
        #[source]
        source: FetchError<E>,
    },
}

/// Errors from an update run ([`HashStore::update`](crate::HashStore::update) /
/// [`HashStore::update_async`](crate::HashStore::update_async)).
///
/// Generic over the fetcher's error type ([`Fetch::Error`](crate::Fetch::Error) /
/// [`AsyncFetch::Error`](crate::AsyncFetch::Error)), so a failed download
/// returns the fetcher's own error type.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum UpdateError<E> {
    /// A local file operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The local or remote manifest could not be read or parsed.
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    /// Downloading a file failed.
    #[error("fetching {file}")]
    Fetch {
        /// The filename being fetched.
        file: String,

        /// Why the fetch failed.
        #[source]
        source: FetchError<E>,
    },

    /// A downloaded file's sha256 does not match the remote manifest.
    #[error("{file}: sha256 mismatch (manifest {expected}, downloaded {actual})")]
    ChecksumMismatch {
        /// The downloaded filename.
        file: String,

        /// The sha256 the remote manifest lists.
        expected: String,

        /// The sha256 of the downloaded bytes.
        actual: String,
    },

    /// A remote manifest entry's filename is not `<table>-<version>.lhdb` with
    /// a valid version label.
    #[error("table {id}: malformed filename {file:?} in the remote manifest")]
    BadRemoteFilename {
        /// The table id from the remote manifest.
        id: String,

        /// The filename the remote manifest gave.
        file: String,
    },

    /// Installing the downloaded tables failed. Nothing was installed.
    #[error("installing the downloaded tables")]
    Commit(#[from] CommitError),
}
