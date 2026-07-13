//! Error types for the shared cache, one per operation so each signature
//! names exactly what it can fail with.

use std::path::PathBuf;

use thiserror::Error;

use crate::Table;

/// Errors from resolving the platform cache directory
/// ([`HashStore::discover`](crate::HashStore::discover)).
#[derive(Debug, Error)]
#[error("could not determine a platform cache directory")]
pub struct NoCacheDirError;

/// Errors from reading, parsing, or writing `manifest.json`.
#[derive(Debug, Error)]
pub enum ManifestError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("manifest json error")]
    Json(#[from] serde_json::Error),

    #[error("no manifest at {0}")]
    Missing(PathBuf),

    #[error("unsupported manifest schema version {0}")]
    UnsupportedSchema(u32),
}

/// Errors from opening a cached table ([`HashStore::open`](crate::HashStore::open) /
/// [`HashStore::path_for`](crate::HashStore::path_for)).
#[derive(Debug, Error)]
pub enum OpenError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),

    #[error("table {0:?} is not in the manifest")]
    TableNotFound(Table),

    #[error("opening the table file")]
    HashDb(#[from] ltk_hashdb::OpenError),
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
#[derive(Debug, Error)]
pub enum UpdateError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Manifest(#[from] ManifestError),

    #[error("fetching {file}")]
    Fetch {
        file: String,
        #[source]
        source: crate::FetchError,
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
