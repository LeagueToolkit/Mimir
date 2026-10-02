//! The `.hashdb` binary format: a read-only table mapping integer keys to
//! string values (paths, in the League Toolkit case), laid out as:
//!
//! - a fixed 80-byte header
//! - a sorted, binary-searchable array of keys
//! - per-entry offset and length arrays
//! - a string arena (raw or zeekstd-seekable), path-ordered so similar paths share frames
//!
//! See `docs/FORMAT.md` in the repository for the byte-level spec.
//!
//! Every opener validates the untrusted header and section bounds, and one
//! lookup allocates at most a few [`MAX_FRAME_SIZE`] frames whatever the file
//! claims. [`HashDb::verify`] runs the full checksum pass.

#![warn(missing_docs, missing_debug_implementations)]

mod cache;
mod error;
mod hash;
mod header;
mod layered;
mod path;
mod reader;
mod writer;

pub use error::{BuildError, CompressionError, KeyConfigMismatch, OpenError, VerifyError};
pub use hash::{Casing, HashKind, KeyConfig};
pub use header::{FORMAT_VERSION, HEADER_SIZE, MAGIC};
pub use layered::LayeredHashDb;
pub use path::PathRef;
pub use reader::{HashDb, HashDbOptions, WeakHashDb, DEFAULT_FRAME_CACHE_BYTES};
pub use writer::{BuildStats, HashDbWriter};

/// Largest decompressed zeekstd frame, in bytes, that the writer produces and
/// the reader accepts (1 MiB).
///
/// Lookups decompress whole frames, so this bounds what one lookup can allocate
/// on an untrusted file; the seekable format itself allows frames of up to
/// 1 GiB. Published tables use 16 KiB frames.
pub const MAX_FRAME_SIZE: u32 = 1 << 20;

/// Width of the integer keys in a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyWidth {
    /// 32-bit keys (bin tables: FNV-1a).
    U32,

    /// 64-bit keys (game/lcu: XXH64, RST: full XXH64/XXH3).
    U64,
}

impl KeyWidth {
    /// Width in bytes (4 or 8), as stored in the header.
    pub fn bytes(self) -> usize {
        match self {
            Self::U32 => 4,
            Self::U64 => 8,
        }
    }
}

impl std::fmt::Display for KeyWidth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::U32 => "u32",
            Self::U64 => "u64",
        })
    }
}

/// Arena compression strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Compression {
    /// Raw concatenated arena, read directly from the file's bytes.
    None,

    /// Zstandard Seekable Format arena, one frame decompressed per hit.
    Zeekstd {
        /// Decompressed frame size in bytes, `1..=`[`MAX_FRAME_SIZE`].
        frame_size: u32,

        /// zstd compression level. Decompression speed does not depend on it.
        level: i32,
    },
}

/// Whether a table carries the arena-order index in the file.
///
/// The arena is laid out in path order but the offsets are stored in key order,
/// so walking the arena forward means knowing the permutation between them. It
/// is what [`HashDb::values`], [`HashDb::prefix`], [`HashDb::iter`] and
/// [`HashDb::verify`] all walk, and a reader that does not find it in the file
/// reconstructs it on first use.
///
/// So this is a space/time trade and nothing else: every operation works either
/// way, and both orders are identical. See `docs/BENCHMARKS.md` for the measured
/// figures behind the summary below.
///
/// [`HashDb::values`]: crate::HashDb::values
/// [`HashDb::prefix`]: crate::HashDb::prefix
/// [`HashDb::iter`]: crate::HashDb::iter
/// [`HashDb::verify`]: crate::HashDb::verify
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ArenaOrder {
    /// Leave it out; the reader rebuilds it the first time it is needed.
    ///
    /// The default, and what every table published so far does. The rebuild is a
    /// sort over the offsets - about a third of a second on the 2.3M-entry game
    /// table - after which it is shared by every clone of the table for as long
    /// as one is open.
    #[default]
    Omitted,

    /// Store it: `entry_count` × 1..8 bytes, sized to the entry count.
    ///
    /// Turns the rebuild into a memory map: no sort, no per-process copy, and
    /// the pages are shared across every process that opens the file. The cost
    /// is file size - about 16% on the game table, less on the smaller ones -
    /// paid by every consumer, including the ones that only ever call
    /// [`HashDb::get`](crate::HashDb::get).
    Stored,
}

impl Default for Compression {
    /// The publishing config: 16 KiB frames at level 19 (see `docs/BENCHMARKS.md`).
    fn default() -> Self {
        Self::Zeekstd {
            frame_size: 16 << 10,
            level: 19,
        }
    }
}

/// Compiles the README's code blocks as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
