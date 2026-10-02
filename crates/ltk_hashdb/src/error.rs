//! Error types for the `.hashdb` format reader/writer, one per fallible
//! operation so each signature names exactly what it can fail with.

use thiserror::Error;

use crate::{HashKind, KeyConfig, KeyWidth};

/// Errors from opening a `.hashdb` file ([`HashDb::open`] / [`HashDb::open_bytes`]):
/// I/O, or the untrusted header/section-bounds validation rejecting the file.
///
/// [`HashDb::open`]: crate::HashDb::open
/// [`HashDb::open_bytes`]: crate::HashDb::open_bytes
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenError {
    /// Reading or mapping the file failed.
    #[error("io error")]
    Io(#[from] std::io::Error),

    /// The file does not start with [`MAGIC`](crate::MAGIC).
    #[error("bad magic: not a hashdb file")]
    BadMagic,

    /// The header names a format version this build cannot read.
    #[error("unsupported format version {0}")]
    UnsupportedVersion(u16),

    /// A header field is out of range or inconsistent.
    #[error("malformed header: {0}")]
    MalformedHeader(&'static str),

    /// A section or the seek table is inconsistent with the header, including a
    /// frame larger than [`MAX_FRAME_SIZE`](crate::MAX_FRAME_SIZE).
    #[error("malformed file: {0}")]
    Malformed(&'static str),

    /// The compressed arena's seek table could not be parsed.
    #[error(transparent)]
    Compression(#[from] CompressionError),
}

/// Errors from the opt-in integrity checks ([`HashDb::verify`],
/// [`HashDb::verify_index`]) and from [`HashDb::try_get`].
///
/// [`HashDb::verify`]: crate::HashDb::verify
/// [`HashDb::verify_index`]: crate::HashDb::verify_index
/// [`HashDb::try_get`]: crate::HashDb::try_get
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// The stored checksum does not match the file's sections.
    #[error("checksum mismatch")]
    ChecksumMismatch,

    /// The sections describe an invalid table.
    #[error("malformed file: {0}")]
    Malformed(&'static str),

    /// Decompressing a frame failed.
    #[error("io error")]
    Io(#[from] std::io::Error),

    /// The compressed arena is inconsistent with its seek table.
    #[error(transparent)]
    Compression(#[from] CompressionError),
}

/// Errors from building a table ([`HashDbWriter::build`]): invalid input
/// entries, a bad configuration, or I/O while writing.
///
/// [`HashDbWriter::build`]: crate::HashDbWriter::build
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BuildError {
    /// Writing the output failed.
    #[error("io error")]
    Io(#[from] std::io::Error),

    /// One key was inserted with two different paths.
    #[error("duplicate key {key:#x} with conflicting paths")]
    DuplicateKey {
        /// The conflicting key.
        key: u64,
    },

    /// A key does not fit in a [`KeyWidth::U32`] table.
    #[error("key {key:#x} does not fit in a u32 table")]
    KeyOutOfRange {
        /// The out-of-range key.
        key: u64,
    },

    /// A path is longer than the format's `u16` length field allows.
    #[error("path for key {key:#x} is {len} bytes; lengths are u16 (max 65535)")]
    PathTooLong {
        /// The key the path was inserted under.
        key: u64,

        /// The path's length in bytes.
        len: usize,
    },

    /// [`Compression::Zeekstd`](crate::Compression::Zeekstd) `frame_size` is
    /// outside `1..=`[`MAX_FRAME_SIZE`](crate::MAX_FRAME_SIZE).
    #[error(
        "zeekstd frame_size {frame_size} is outside 1..={}",
        crate::MAX_FRAME_SIZE
    )]
    InvalidFrameSize {
        /// The rejected frame size.
        frame_size: u32,
    },

    /// A 64-bit hash algorithm was recorded for a [`KeyWidth::U32`] table, so
    /// [`HashDb::hash_path`](crate::HashDb::hash_path) could never produce one
    /// of its keys.
    #[error("hash kind {hash_kind} produces 64-bit keys but the table is {key_width}")]
    HashKindTooWide {
        /// The recorded algorithm.
        hash_kind: HashKind,

        /// The table's key width.
        key_width: KeyWidth,
    },

    /// Compressing the arena failed.
    #[error(transparent)]
    Compression(#[from] CompressionError),
}

/// A failure inside the zstd seekable-format layer.
///
/// Opaque so the compression library can change without a breaking release.
/// The underlying error is available through [`std::error::Error::source`].
#[derive(Debug, Error)]
#[error("zstd seekable format error")]
pub struct CompressionError(#[source] zeekstd::Error);

/// Converts zeekstd results without a public `From<zeekstd::Error>` impl, which
/// would put zeekstd in this crate's API.
pub(crate) trait ZeekstdResultExt<T> {
    fn zeek(self) -> Result<T, CompressionError>;
}

impl<T> ZeekstdResultExt<T> for Result<T, zeekstd::Error> {
    fn zeek(self) -> Result<T, CompressionError> {
        self.map_err(CompressionError)
    }
}

/// A base rejected by [`LayeredHashDb`] because it does not hash its keys the way
/// the rest of the layer does.
///
/// [`LayeredHashDb`]: crate::LayeredHashDb
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error(
    "base {index} is keyed {found}, but the layer is keyed {expected}; a base that hashes \
     differently can never be hit by a caller's precomputed probe"
)]
#[non_exhaustive]
pub struct KeyConfigMismatch {
    /// Position of the rejected base, counting the ones already layered.
    pub index: usize,

    /// What the layer hashes under: its first base's configuration.
    pub expected: KeyConfig,

    /// What the rejected base hashes under.
    pub found: KeyConfig,
}
