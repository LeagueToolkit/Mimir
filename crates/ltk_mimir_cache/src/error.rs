//! Error type for the shared cache.

use std::path::PathBuf;

use thiserror::Error;

use crate::Table;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("hashdb error")]
    HashDb(#[from] ltk_hashdb::Error),

    #[error("manifest json error")]
    Json(#[from] serde_json::Error),

    #[error("could not determine a platform cache directory")]
    NoCacheDir,

    #[error("no manifest at {0}")]
    MissingManifest(PathBuf),

    #[error("unsupported manifest schema version {0}")]
    UnsupportedSchema(u32),

    #[error("table {0:?} is not in the manifest")]
    TableNotFound(Table),

    #[error("invalid version label {0:?}: must be non-empty and free of path separators")]
    InvalidVersion(String),

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
}
