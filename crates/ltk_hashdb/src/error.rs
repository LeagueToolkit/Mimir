//! Error types for the `.hashdb` format reader/writer, one per fallible
//! operation so each signature names exactly what it can fail with.

use thiserror::Error;

/// Errors from opening a `.hashdb` file ([`HashDb::open`] / [`HashDb::open_bytes`]):
/// I/O, or the untrusted header/section-bounds validation rejecting the file.
///
/// [`HashDb::open`]: crate::HashDb::open
/// [`HashDb::open_bytes`]: crate::HashDb::open_bytes
#[derive(Debug, Error)]
pub enum OpenError {
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

    #[error("zstd seekable format error")]
    Zeekstd(#[from] zeekstd::Error),
}

/// Errors from the opt-in full integrity check ([`HashDb::verify`]).
///
/// [`HashDb::verify`]: crate::HashDb::verify
#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("checksum mismatch")]
    ChecksumMismatch,

    #[error("malformed file: {0}")]
    Malformed(&'static str),

    #[error("io error")]
    Io(#[from] std::io::Error),

    #[error("zstd seekable format error")]
    Zeekstd(#[from] zeekstd::Error),
}

/// Errors from building a table ([`HashDbWriter::build`]): invalid input
/// entries, a bad compression config, or I/O while writing.
///
/// [`HashDbWriter::build`]: crate::HashDbWriter::build
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("io error")]
    Io(#[from] std::io::Error),

    #[error("duplicate key {key:#x} with conflicting paths")]
    DuplicateKey { key: u64 },

    #[error("key {key:#x} does not fit in a u32 table")]
    KeyOutOfRange { key: u64 },

    #[error("path for key {key:#x} is {len} bytes; lengths are u16 (max 65535)")]
    PathTooLong { key: u64, len: usize },

    #[error("zeekstd frame_size must be nonzero")]
    ZeroFrameSize,

    #[error("zstd seekable format error")]
    Zeekstd(#[from] zeekstd::Error),
}
