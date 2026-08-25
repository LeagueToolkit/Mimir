//! The fixed 80-byte `.hashdb` file header.
//!
//! Byte layout (all integers little-endian):
//!
//! ```text
//! 0..8    magic                    [u8;8]  b"HASHDB\0\0"
//! 8..10   version                  u16
//! 10      hash_kind                u8      see HashKind
//! 11      flags                    u8      required; bit0: arena_compressed, bit1: case_insensitive
//! 12      key_width                u8      4 = u32 table, 8 = u64 table
//! 13      offset_width             u8      4 or 8; width of arena offsets
//! 14      opt_flags                u8      optional; unknown bits ignored, none defined yet
//! 15      arena_order_width        u8      1..=8 when an arena-order section is present, else 0
//! 16..24  entry_count              u64
//! 24..32  keys_offset              u64     file offset, 8-aligned
//! 32..40  offsets_offset           u64     file offset, offset_width-aligned
//! 40..48  arena_offset             u64
//! 48..56  arena_decompressed_size  u64
//! 56..64  arena_compressed_size    u64     == decompressed if raw
//! 64..72  checksum                 u64     xxh3-64 of keys‖offsets‖lengths‖arena (as stored)
//! 72..80  arena_order_offset       u64     file offset of the arena-order section; 0 = absent
//! ```
//!
//! The lengths section (`entry_count` × u16) has no header field: it sits
//! immediately after the offsets, at `offsets_offset + entry_count × offset_width`.
//!
//! Bytes 15 and 72..80 were reserved-and-zero through every build that shipped
//! before the arena-order section existed, which is what lets that section be
//! added without a version bump: an older reader sees a file whose reserved
//! fields it ignores, and reads it exactly as it always did. A capability that
//! needs no field of its own announces itself in `opt_flags` instead; this one
//! needs an offset, and a section offset of zero is already unambiguous, so it
//! is its own announcement rather than a flag that could disagree with it.
//!
//! The two flag bytes differ in what an *unknown* bit means. A bit in `flags`
//! changes how the file has to be read, so a build that does not know it must
//! refuse the file; a bit in `opt_flags` only announces something a build may
//! take advantage of, so a build that does not know it reads the file as if the
//! bit were clear. Since `version` is an equality gate, `opt_flags` is the only
//! way to announce a new capability to readers that already shipped.

use crate::{Casing, HashKind, KeyWidth, OpenError};

pub const MAGIC: [u8; 8] = *b"HASHDB\0\0";
pub const FORMAT_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 80;

/// Header flag: the arena is a zeekstd seekable stream rather than raw bytes.
pub(crate) const FLAG_ARENA_COMPRESSED: u8 = 1 << 0;

/// Header flag: the keys hash the ASCII-lowercased path
/// ([`Casing::AsciiInsensitive`]).
///
/// Should a Unicode-aware rule ever be wanted, it gets a bit in `opt_flags`
/// rather than a second `flags` bit, so that builds predating it keep reading
/// the file - they resolve every ASCII path as before and miss the rest.
pub(crate) const FLAG_CASE_INSENSITIVE: u8 = 1 << 1;

/// Every required flag bit this build understands; any other rejects the file.
const KNOWN_FLAGS: u8 = FLAG_ARENA_COMPRESSED | FLAG_CASE_INSENSITIVE;

/// Where the optional arena-order section sits, and how wide its entries are.
///
/// See `docs/FORMAT.md`; the section is `entry_count` packed entry indices in
/// arena order, followed by an 8-byte checksum of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArenaOrderRef {
    pub offset: u64,
    pub width: usize,
}

impl ArenaOrderRef {
    /// Bytes the whole section occupies, checksum included, or `None` on overflow.
    pub fn len(&self, entry_count: u64) -> Option<u64> {
        entry_count
            .checked_mul(self.width as u64)?
            .checked_add(ARENA_ORDER_CHECKSUM_SIZE as u64)
    }
}

/// Trailing xxh3-64 over the section's packed entries.
///
/// The header's own `checksum` deliberately does not cover this section: it is
/// defined as keys‖offsets‖lengths‖arena, and a reader built before the section
/// existed still computes it that way. Giving the section its own digest keeps
/// both readers right about the same file.
pub(crate) const ARENA_ORDER_CHECKSUM_SIZE: usize = 8;

/// Bytes needed to hold any entry index of a table this size, 1..=8.
///
/// The narrowest packing that still addresses every entry - 3 bytes for the ~2.3M
/// entry `game` table, where a `u32` array would spend a quarter of its bytes on
/// zeroes.
pub(crate) fn arena_order_width(entry_count: u64) -> usize {
    let max = entry_count.saturating_sub(1);
    let bits = u64::BITS - max.leading_zeros();

    (bits.div_ceil(8) as usize).max(1)
}

/// Width of the arena offsets: u32 unless the raw arena exceeds 4 GiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffsetWidth {
    U32,
    U64,
}

impl OffsetWidth {
    pub fn bytes(self) -> usize {
        match self {
            Self::U32 => 4,
            Self::U64 => 8,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Header {
    pub hash_kind: HashKind,
    pub flags: u8,
    pub key_width: KeyWidth,
    pub offset_width: OffsetWidth,
    pub entry_count: u64,
    pub keys_offset: u64,
    pub offsets_offset: u64,
    pub arena_offset: u64,
    pub arena_decompressed_size: u64,
    pub arena_compressed_size: u64,
    pub checksum: u64,

    /// The arena-order section, when the writer emitted one.
    pub arena_order: Option<ArenaOrderRef>,
}

impl Header {
    pub fn arena_compressed(&self) -> bool {
        self.flags & FLAG_ARENA_COMPRESSED != 0
    }

    pub fn casing(&self) -> Casing {
        if self.flags & FLAG_CASE_INSENSITIVE != 0 {
            Casing::AsciiInsensitive
        } else {
            Casing::Sensitive
        }
    }

    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..8].copy_from_slice(&MAGIC);
        buf[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf[10] = self.hash_kind as u8;
        buf[11] = self.flags;
        buf[12] = self.key_width.bytes() as u8;
        buf[13] = self.offset_width.bytes() as u8;
        // Byte 14 (`opt_flags`) stays zero: this build announces no optional
        // capability.
        if let Some(order) = self.arena_order {
            buf[15] = order.width as u8;
            buf[72..80].copy_from_slice(&order.offset.to_le_bytes());
        }
        buf[16..24].copy_from_slice(&self.entry_count.to_le_bytes());
        buf[24..32].copy_from_slice(&self.keys_offset.to_le_bytes());
        buf[32..40].copy_from_slice(&self.offsets_offset.to_le_bytes());
        buf[40..48].copy_from_slice(&self.arena_offset.to_le_bytes());
        buf[48..56].copy_from_slice(&self.arena_decompressed_size.to_le_bytes());
        buf[56..64].copy_from_slice(&self.arena_compressed_size.to_le_bytes());
        buf[64..72].copy_from_slice(&self.checksum.to_le_bytes());
        buf
    }

    /// Decode and validate the header's own fields; the reader checks section
    /// bounds against the file length.
    pub fn decode(bytes: &[u8]) -> Result<Self, OpenError> {
        let buf: &[u8; HEADER_SIZE] = bytes
            .get(..HEADER_SIZE)
            .and_then(|s| s.try_into().ok())
            .ok_or(OpenError::MalformedHeader("file shorter than header"))?;

        if buf[0..8] != MAGIC {
            return Err(OpenError::BadMagic);
        }
        let version = u16::from_le_bytes(buf[8..10].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(OpenError::UnsupportedVersion(version));
        }
        let hash_kind =
            HashKind::from_u8(buf[10]).ok_or(OpenError::MalformedHeader("unknown hash_kind"))?;
        let flags = buf[11];
        if flags & !KNOWN_FLAGS != 0 {
            return Err(OpenError::MalformedHeader("unknown required flag bits set"));
        }
        let key_width = match buf[12] {
            4 => KeyWidth::U32,
            8 => KeyWidth::U64,
            _ => return Err(OpenError::MalformedHeader("key_width must be 4 or 8")),
        };
        let offset_width = match buf[13] {
            4 => OffsetWidth::U32,
            8 => OffsetWidth::U64,
            _ => return Err(OpenError::MalformedHeader("offset_width must be 4 or 8")),
        };
        // `opt_flags` (byte 14) is deliberately not validated: an optional flag
        // this build does not know describes a capability it simply will not
        // use, and rejecting the file over one would defeat the point of having
        // a second flag byte at all.

        let u64_at = |i: usize| u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());

        // Absent is the norm and reads as zero, so the width is only meaningful
        // - and only checked - once an offset claims a section is there.
        let arena_order = match u64_at(72) {
            0 => None,
            offset => match buf[15] {
                width @ 1..=8 => Some(ArenaOrderRef {
                    offset,
                    width: width as usize,
                }),
                _ => {
                    return Err(OpenError::MalformedHeader(
                        "arena_order_width must be 1..=8",
                    ))
                }
            },
        };

        Ok(Self {
            hash_kind,
            flags,
            key_width,
            offset_width,
            entry_count: u64_at(16),
            keys_offset: u64_at(24),
            offsets_offset: u64_at(32),
            arena_offset: u64_at(40),
            arena_decompressed_size: u64_at(48),
            arena_compressed_size: u64_at(56),
            checksum: u64_at(64),
            arena_order,
        })
    }
}
