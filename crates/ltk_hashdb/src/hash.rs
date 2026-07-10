//! The hash algorithms used by the logical tables.
//!
//! Each table records its algorithm (`hash_kind` header byte) and casing rule
//! (`case_insensitive` header flag) so consumers can hash new paths via
//! [`crate::HashDb::hash_path`]. Unit tests pin the case-insensitive results to
//! `ltk_hash`'s `WadHash`/`BinHash` (League paths are ASCII, where they coincide).

use xxhash_rust::xxh3::xxh3_64;
use xxhash_rust::xxh64::xxh64;

use crate::KeyWidth;

/// Whether a table's keys hash the path as given or its lowercased form.
///
/// Stored as the `case_insensitive` header flag, orthogonal to [`HashKind`]:
/// the algorithm says *how* the bytes are hashed, the casing says *which* bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Casing {
    /// Hash the path bytes exactly as given.
    #[default]
    Sensitive,

    /// Lowercase the path before hashing (all League tables). The mapping is
    /// Unicode-aware ([`str::to_lowercase`]); on the ASCII paths League ships
    /// it reduces to plain `A-Z` → `a-z`, and non-ASCII paths get sensible
    /// case-insensitivity for free.
    Insensitive,
}

/// The hash algorithm a table's keys were produced with. The casing rule is
/// recorded separately (see [`Casing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum HashKind {
    /// Not recorded. [`HashKind::hash`] falls back on key width:
    /// u64 → [`HashKind::Xxh64`], u32 → [`HashKind::Fnv1a32`].
    #[default]
    Unspecified = 0,

    /// XXH64, seed 0 (game/lcu WAD tables, RST xxh64 tables).
    Xxh64 = 1,

    /// FNV-1a 32 (bin entries/types/fields/hashes).
    Fnv1a32 = 2,

    /// XXH3-64, no seed (RST v5+ stringtables).
    Xxh3 = 3,
}

impl HashKind {
    pub(crate) fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Unspecified),
            1 => Some(Self::Xxh64),
            2 => Some(Self::Fnv1a32),
            3 => Some(Self::Xxh3),
            _ => None,
        }
    }

    /// Hash `path` with this algorithm under `casing`. `key_width` resolves the
    /// [`HashKind::Unspecified`] fallback.
    ///
    /// Insensitive hashing is allocation-free for ASCII paths (all League data):
    /// they lowercase into a stack buffer. Only non-ASCII or longer-than-buffer
    /// paths pay the full Unicode [`str::to_lowercase`] allocation. This sits on
    /// the hunt engine's hot path - millions of candidates per round - where the
    /// stack path measures ~2-3× faster than the allocating one.
    pub fn hash(self, path: &str, casing: Casing, key_width: KeyWidth) -> u64 {
        let kind = match self {
            Self::Unspecified => match key_width {
                KeyWidth::U32 => Self::Fnv1a32,
                KeyWidth::U64 => Self::Xxh64,
            },
            other => other,
        };

        match casing {
            Casing::Sensitive => kind.hash_bytes(path.as_bytes()),
            Casing::Insensitive if path.is_ascii() && path.len() <= LOWER_STACK => {
                let mut buf = [0u8; LOWER_STACK];
                let lowered = &mut buf[..path.len()];
                lowered.copy_from_slice(path.as_bytes());
                lowered.make_ascii_lowercase();
                kind.hash_bytes(lowered)
            }
            Casing::Insensitive => kind.hash_bytes(path.to_lowercase().as_bytes()),
        }
    }

    /// `self` must be a concrete algorithm ([`HashKind::Unspecified`] already
    /// resolved by [`HashKind::hash`]).
    fn hash_bytes(self, bytes: &[u8]) -> u64 {
        match self {
            Self::Xxh64 => xxh64(bytes, 0),
            Self::Fnv1a32 => fnv1a32(bytes) as u64,
            Self::Xxh3 => xxh3_64(bytes),
            Self::Unspecified => unreachable!(),
        }
    }
}

/// Mixed-case ASCII paths up to this length lowercase on the stack; longer ones
/// fall back to a heap allocation. Real paths max out around 200 bytes.
const LOWER_STACK: usize = 512;

/// FNV-1a 32 over raw bytes (`ltk_hash::BinHash` only exposes a lowercasing form).
fn fnv1a32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5u32, |h, &b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

#[cfg(test)]
mod tests {
    use ltk_hash::{BinHash, Hash as _, WadHash};

    use super::*;

    /// League parity: on ASCII paths (all League data) the case-insensitive
    /// results must equal `ltk_hash`'s `WadHash`/`BinHash`.
    #[test]
    fn insensitive_matches_ltk_hash() {
        let p = "DATA/Characters/Aatrox/Aatrox.bin";
        assert_eq!(
            HashKind::Xxh64.hash(p, Casing::Insensitive, KeyWidth::U64),
            *WadHash::hash_str(p)
        );
        assert_eq!(
            HashKind::Fnv1a32.hash(p, Casing::Insensitive, KeyWidth::U32),
            *BinHash::hash_str(p) as u64
        );
        // Known FNV-1a-lower vector (from ltk_hash's own tests).
        assert_eq!(
            HashKind::Fnv1a32.hash("TEST", Casing::Insensitive, KeyWidth::U32),
            0xafd071e5
        );
    }

    #[test]
    fn insensitive_lowercases_before_hashing() {
        for kind in [HashKind::Xxh64, HashKind::Fnv1a32, HashKind::Xxh3] {
            let a = kind.hash("ASSETS/Foo.DDS", Casing::Insensitive, KeyWidth::U64);
            let b = kind.hash("assets/foo.dds", Casing::Insensitive, KeyWidth::U64);
            assert_eq!(a, b, "{kind:?}");

            // The lowercasing is Unicode-aware, not ASCII-only.
            let a = kind.hash("assets/É.dds", Casing::Insensitive, KeyWidth::U64);
            let b = kind.hash("assets/é.dds", Casing::Insensitive, KeyWidth::U64);
            assert_eq!(a, b, "{kind:?} (unicode)");
        }
    }

    #[test]
    fn sensitive_distinguishes_case() {
        for kind in [HashKind::Xxh64, HashKind::Fnv1a32, HashKind::Xxh3] {
            let upper = kind.hash("ASSETS/Foo.DDS", Casing::Sensitive, KeyWidth::U64);
            let lower = kind.hash("assets/foo.dds", Casing::Sensitive, KeyWidth::U64);
            assert_ne!(upper, lower, "{kind:?}");
        }
    }

    /// On an already-lowercase path the two casing rules must agree - sensitive
    /// hashing is the same algorithm, just without the lowercasing step.
    #[test]
    fn sensitive_agrees_with_insensitive_on_lowercase_input() {
        let p = "data/characters/aatrox/aatrox.bin";
        for kind in [HashKind::Xxh64, HashKind::Fnv1a32, HashKind::Xxh3] {
            assert_eq!(
                kind.hash(p, Casing::Sensitive, KeyWidth::U64),
                kind.hash(p, Casing::Insensitive, KeyWidth::U64),
                "{kind:?}"
            );
        }
        // Same known vector as above, reachable case-sensitively via lowercase input.
        assert_eq!(
            HashKind::Fnv1a32.hash("test", Casing::Sensitive, KeyWidth::U32),
            0xafd071e5
        );
    }

    /// By definition `Insensitive` must equal lowercase-then-`Sensitive`; pin the
    /// stack-buffer / heap fast paths (and the buffer boundary) to it.
    #[test]
    fn insensitive_fast_paths_match_reference() {
        let long_mixed = "A".repeat(600) + "/File.DDS";
        let mut cases = vec![
            String::new(),
            "a".into(),
            "assets/foo.dds".into(), // ASCII: stack buffer
            "ASSETS/Foo.DDS".into(),
            "ässets/FÖÖ.dds".into(), // non-ASCII: heap
            "É".into(),
            long_mixed, // ASCII past the stack buffer: heap
        ];
        for len in [511, 512, 513] {
            cases.push("A".repeat(len)); // exactly around the stack-buffer boundary
        }

        for kind in [HashKind::Xxh64, HashKind::Fnv1a32, HashKind::Xxh3] {
            for path in &cases {
                assert_eq!(
                    kind.hash(path, Casing::Insensitive, KeyWidth::U64),
                    kind.hash(&path.to_lowercase(), Casing::Sensitive, KeyWidth::U64),
                    "{kind:?} {path:?}"
                );
            }
        }
    }

    #[test]
    fn unspecified_falls_back_on_key_width() {
        let p = "data/characters/aatrox/aatrox.bin";
        for casing in [Casing::Sensitive, Casing::Insensitive] {
            assert_eq!(
                HashKind::Unspecified.hash(p, casing, KeyWidth::U64),
                HashKind::Xxh64.hash(p, casing, KeyWidth::U64)
            );
            assert_eq!(
                HashKind::Unspecified.hash(p, casing, KeyWidth::U32),
                HashKind::Fnv1a32.hash(p, casing, KeyWidth::U32)
            );
        }
    }
}
