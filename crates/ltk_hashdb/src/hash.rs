//! The hash algorithms used by the logical tables.
//!
//! Each table records its algorithm (`hash_kind` header byte) so consumers can hash
//! new paths via [`crate::HashDb::hash_path`]. Wad/bin hashing delegates to `ltk_hash`.

use ltk_hash::{BinHash, Hash as _, WadHash};
use xxhash_rust::xxh3::xxh3_64;

use crate::KeyWidth;

/// The hash algorithm a table's keys were produced with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum HashKind {
    /// Not recorded. [`HashKind::hash`] falls back on key width:
    /// u64 → [`HashKind::Xxh64Lower`], u32 → [`HashKind::Fnv1a32Lower`].
    #[default]
    Unspecified = 0,

    /// XXH64 of the lowercased path (game/lcu WAD tables, RST xxh64 tables).
    Xxh64Lower = 1,

    /// FNV-1a 32 of the lowercased path (bin entries/types/fields/hashes).
    Fnv1a32Lower = 2,

    /// XXH3-64 of the lowercased path (RST v5+ stringtables).
    Xxh3Lower = 3,
}

impl HashKind {
    pub(crate) fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Unspecified),
            1 => Some(Self::Xxh64Lower),
            2 => Some(Self::Fnv1a32Lower),
            3 => Some(Self::Xxh3Lower),
            _ => None,
        }
    }

    /// Hash `path` with this algorithm. `key_width` resolves the
    /// [`HashKind::Unspecified`] fallback.
    pub fn hash(self, path: &str, key_width: KeyWidth) -> u64 {
        let kind = match self {
            Self::Unspecified => match key_width {
                KeyWidth::U32 => Self::Fnv1a32Lower,
                KeyWidth::U64 => Self::Xxh64Lower,
            },
            other => other,
        };
        match kind {
            Self::Xxh64Lower => *WadHash::hash_str(path),
            Self::Fnv1a32Lower => *BinHash::hash_str(path) as u64,
            Self::Xxh3Lower => xxh3_64(path.to_ascii_lowercase().as_bytes()),
            Self::Unspecified => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ltk_hash() {
        let p = "DATA/Characters/Aatrox/Aatrox.bin";
        assert_eq!(
            HashKind::Xxh64Lower.hash(p, KeyWidth::U64),
            *WadHash::hash_str(p)
        );
        assert_eq!(
            HashKind::Fnv1a32Lower.hash(p, KeyWidth::U32),
            *BinHash::hash_str(p) as u64
        );
        // Known FNV-1a-lower vector (from ltk_hash's own tests).
        assert_eq!(
            HashKind::Fnv1a32Lower.hash("TEST", KeyWidth::U32),
            0xafd071e5
        );
    }

    #[test]
    fn lowercases_before_hashing() {
        let a = HashKind::Xxh64Lower.hash("ASSETS/Foo.DDS", KeyWidth::U64);
        let b = HashKind::Xxh64Lower.hash("assets/foo.dds", KeyWidth::U64);
        assert_eq!(a, b);
    }

    #[test]
    fn unspecified_falls_back_on_key_width() {
        let p = "data/characters/aatrox/aatrox.bin";
        assert_eq!(
            HashKind::Unspecified.hash(p, KeyWidth::U64),
            HashKind::Xxh64Lower.hash(p, KeyWidth::U64)
        );
        assert_eq!(
            HashKind::Unspecified.hash(p, KeyWidth::U32),
            HashKind::Fnv1a32Lower.hash(p, KeyWidth::U32)
        );
    }
}
