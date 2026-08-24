//! Error types for the shared cache, one per operation so each signature
//! names exactly what it can fail with.

use std::path::PathBuf;

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
pub enum ManifestError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("manifest json error")]
    Json(#[from] serde_json::Error),

    #[error("no manifest at {0}")]
    Missing(PathBuf),

    #[error("manifest schema {0} predates the first published one")]
    UnsupportedSchema(u32),

    #[error(
        "this manifest requires a reader that understands schema {required}, and this build \
         understands {supported}"
    )]
    ReaderTooOld { required: u32, supported: u32 },
}

/// Errors from opening a cached table ([`HashStore::open`](crate::HashStore::open) /
/// [`HashStore::path_for`](crate::HashStore::path_for)).
#[derive(Debug, Error)]
pub enum OpenError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    #[error("table {0} is not in the manifest")]
    TableNotFound(Table),

    #[error("opening the table file")]
    HashDb(#[from] ltk_hashdb::OpenError),

    #[error("the table file does not hash its keys the way this table is defined to")]
    KeyConfig(#[from] ltk_hashdb::KeyConfigMismatch),
}

/// Refusal to layer tables drawn from different hash universes
/// ([`HashStore::open_layered`](crate::HashStore::open_layered)).
#[derive(Debug, Clone, Copy, Error)]
#[error(
    "cannot layer {table} ({found}) with {first} ({expected}): a hash means nothing outside \
     its own universe, so one table would answer the other with a confident, wrong path"
)]
pub struct UniverseMismatch {
    /// The first table in the requested set - the universe the rest must match.
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
pub enum CommitError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Manifest(#[from] ManifestError),

    #[error("opening the built table file")]
    HashDb(#[from] ltk_hashdb::OpenError),

    #[error("invalid version label {0:?}: must be non-empty and free of path separators")]
    InvalidVersion(String),

    #[error(
        "table {table:?}: version {version:?} is already published with different content; \
         published versions are immutable"
    )]
    VersionReused { table: Table, version: String },
}

/// Errors from sweeping unreferenced files ([`HashStore::gc`](crate::HashStore::gc)).
#[derive(Debug, Error)]
pub enum GcError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

/// Errors from an update run ([`HashStore::update`](crate::HashStore::update)).
///
/// Generic over the fetcher's error type ([`Fetch::Error`](crate::Fetch::Error) /
/// [`AsyncFetch::Error`](crate::AsyncFetch::Error)), so a failed download
/// surfaces the transport's concrete error instead of a boxed one.
#[derive(Debug, Error)]
pub enum UpdateError<E> {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Manifest(#[from] ManifestError),

    #[error("fetching {file}")]
    Fetch {
        file: String,
        #[source]
        source: E,
    },

    #[error("{file}: sha256 mismatch (manifest {expected}, downloaded {actual})")]
    ChecksumMismatch {
        file: String,
        expected: String,
        actual: String,
    },

    #[error("table {id}: malformed filename {file:?} in the remote manifest")]
    BadRemoteFilename { id: String, file: String },

    #[error("installing the downloaded tables")]
    Commit(#[from] CommitError),
}
