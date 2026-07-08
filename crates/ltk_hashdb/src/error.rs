//! Error type for the `.hashdb` format reader/writer.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error")]
    Io(#[from] std::io::Error),

    #[error("bad magic: not a hashdb file")]
    BadMagic,

    #[error("unsupported format version {0}")]
    UnsupportedVersion(u16),

    #[error("malformed header: {0}")]
    MalformedHeader(&'static str),

    #[error("malformed file: {0}")]
    Malformed(&'static str),

    #[error("checksum mismatch")]
    ChecksumMismatch,

    #[error("duplicate key {key:#x} with conflicting paths")]
    DuplicateKey { key: u64 },

    #[error("key {key:#x} does not fit in a u32 table")]
    KeyOutOfRange { key: u64 },

    #[error("path for key {key:#x} is {len} bytes; lengths are u16 (max 65535)")]
    PathTooLong { key: u64, len: usize },

    #[error("zstd seekable format error")]
    Zeekstd(#[from] zeekstd::Error),
}
