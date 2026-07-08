//! The `.hashdb` binary format: a read-only, mmap-backed table mapping integer
//! keys to string values (paths, in the League Toolkit case), laid out as:
//!
//! - a fixed 80-byte header
//! - a sorted, binary-searchable array of keys
//! - per-entry offset and length arrays
//! - a string arena (raw or zeekstd-seekable), path-ordered so similar paths share frames
//!
//! See `docs/FORMAT.md` for the byte-level spec.

mod error;
mod extended;
mod hash;
mod header;
mod reader;
mod writer;

pub use error::{Error, Result};
pub use extended::ExtendedHashDb;
pub use hash::HashKind;
pub use header::{FORMAT_VERSION, HEADER_SIZE, MAGIC};
pub use reader::HashDb;
pub use writer::{BuildStats, HashDbWriter};

/// Width of the integer keys in a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl Default for Compression {
    /// Publishing config: 16 KiB frames (the size/latency knee) at level 19.
    fn default() -> Self {
        Self::Zeekstd {
            frame_size: 16 << 10,
            level: 19,
        }
    }
}
