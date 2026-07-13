//! The fixed 80-byte `.hashdb` file header.
//!
//! Byte layout (all integers little-endian):
//!
//! ```text
//! 0..8    magic                    [u8;8]  b"HASHDB\0\0"
//! 8..10   version                  u16
//! 10      hash_kind                u8      see HashKind
//! 11      flags                    u8      bit0: arena_compressed, bit1: case_insensitive
//! 12      key_width                u8      4 = u32 table, 8 = u64 table
//! 13      offset_width             u8      4 or 8; width of arena offsets
//! 14..16  reserved                 [u8;2]  written as zero, ignored on read
//! 16..24  entry_count              u64
//! 24..32  keys_offset              u64     file offset, 8-aligned
//! 32..40  offsets_offset           u64     file offset, offset_width-aligned
//! 40..48  arena_offset             u64
//! 48..56  arena_decompressed_size  u64
//! 56..64  arena_compressed_size    u64     == decompressed if raw
//! 64..72  checksum                 u64     xxh3-64 of keys‖offsets‖lengths‖arena (as stored)
//! 72..80  reserved                 [u8;8]  written as zero, ignored on read
//! ```
//!
//! The lengths section (`entry_count` × u16) has no header field: it sits
//! immediately after the offsets, at `offsets_offset + entry_count × offset_width`.

use crate::{Casing, HashKind, KeyWidth, OpenError};

pub const MAGIC: [u8; 8] = *b"HASHDB\0\0";
pub const FORMAT_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 80;

/// Header flag: the arena is a zeekstd seekable stream rather than raw bytes.
pub(crate) const FLAG_ARENA_COMPRESSED: u8 = 1 << 0;

/// Header flag: the keys hash the lowercased path ([`Casing::Insensitive`]).
pub(crate) const FLAG_CASE_INSENSITIVE: u8 = 1 << 1;

const KNOWN_FLAGS: u8 = FLAG_ARENA_COMPRESSED | FLAG_CASE_INSENSITIVE;

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
}

impl Header {
    pub fn arena_compressed(&self) -> bool {
        self.flags & FLAG_ARENA_COMPRESSED != 0
    }

    pub fn casing(&self) -> Casing {
        if self.flags & FLAG_CASE_INSENSITIVE != 0 {
            Casing::Insensitive
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
            return Err(OpenError::MalformedHeader("unknown flag bits set"));
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

        let u64_at = |i: usize| u64::from_le_bytes(buf[i..i + 8].try_into().unwrap());
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
        })
    }
}
