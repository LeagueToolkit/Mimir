//! The `.hashdb` binary format: a read-only, mmap-backed table mapping integer
//! keys to string values (paths, in the League Toolkit case), laid out as:
//!
//! - a fixed 80-byte header
//! - a sorted, binary-searchable array of keys
//! - per-entry offset and length arrays
//! - a string arena (raw or zeekstd-seekable), path-ordered so similar paths share frames
//!
//! See `docs/FORMAT.md` for the byte-level spec.

mod cache;
mod error;
mod hash;
mod header;
mod layered;
mod path;
mod reader;
mod writer;

pub use error::{BuildError, KeyConfigMismatch, OpenError, VerifyError};
pub use hash::{Casing, HashKind, KeyConfig};
pub use header::{FORMAT_VERSION, HEADER_SIZE, MAGIC};
pub use layered::LayeredHashDb;
pub use path::PathRef;
pub use reader::{HashDb, HashDbOptions, WeakHashDb, DEFAULT_FRAME_CACHE_BYTES};
pub use writer::{BuildStats, HashDbWriter};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// Raw concatenated arena, borrowed directly from the mmap.
    None,

    /// Zstandard Seekable Format arena, one frame decompressed per hit.
    ///
    /// - `frame_size`: decompressed frame size in bytes
    /// - `level`: zstd compression level (decompression speed is independent of it)
    Zeekstd { frame_size: u32, level: i32 },
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    /// Publishing config: 16 KiB frames (the size/latency knee) at level 19.
    fn default() -> Self {
        Self::Zeekstd {
            frame_size: 16 << 10,
            level: 19,
        }
    }
}
