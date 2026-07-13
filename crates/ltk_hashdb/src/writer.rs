//! Streaming builder for `.hashdb` files.

use std::io::{Seek, Write};

use xxhash_rust::xxh3::Xxh3;

use crate::header::{
    Header, OffsetWidth, FLAG_ARENA_COMPRESSED, FLAG_CASE_INSENSITIVE, HEADER_SIZE,
};
use crate::{BuildError, Casing, Compression, HashKind, KeyWidth};

/// Collects `(key, path)` pairs, then [`HashDbWriter::build`] sorts by key, dedups,
/// assigns arena offsets, and writes the file.
pub struct HashDbWriter {
    key_width: KeyWidth,
    compression: Compression,
    hash_kind: HashKind,
    casing: Casing,
    entries: Vec<(u64, Box<str>)>,
}

/// Sizes reported by a successful [`HashDbWriter::build`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildStats {
    pub entries: usize,
    pub arena_decompressed_size: u64,
    pub arena_compressed_size: u64,
    pub file_len: u64,
}

impl HashDbWriter {
    pub fn new(key_width: KeyWidth, compression: Compression) -> Self {
        Self {
            key_width,
            compression,
            hash_kind: HashKind::Unspecified,
            casing: Casing::Sensitive,
            entries: Vec::new(),
        }
    }

    /// Record the algorithm the keys were hashed with, so readers can hash new
    /// paths via `HashDb::hash_path`.
    pub fn hash_kind(mut self, kind: HashKind) -> Self {
        self.hash_kind = kind;
        self
    }

    /// Record whether the keys hash the lowercased path ([`Casing::Insensitive`],
    /// all League tables) or the path as given. Defaults to [`Casing::Sensitive`].
    pub fn casing(mut self, casing: Casing) -> Self {
        self.casing = casing;
        self
    }

    pub fn insert(&mut self, key: u64, path: &str) {
        self.entries.push((key, path.into()));
    }

    pub fn extend<'a>(&mut self, it: impl IntoIterator<Item = (u64, &'a str)>) {
        self.entries
            .extend(it.into_iter().map(|(k, p)| (k, Box::from(p))));
    }

    /// Sort by key, dedup, assign offsets, and write
    /// header + keys + offsets + lengths + arena.
    ///
    /// A key mapped to two different paths is a [`BuildError::DuplicateKey`].
    pub fn build<W: Write + Seek>(mut self, mut out: W) -> Result<BuildStats, BuildError> {
        self.entries.sort_unstable();
        self.entries.dedup();
        if let Some(w) = self.entries.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(BuildError::DuplicateKey { key: w[0].0 });
        }
        if self.key_width == KeyWidth::U32 {
            if let Some(&(key, _)) = self.entries.iter().find(|(k, _)| *k > u32::MAX as u64) {
                return Err(BuildError::KeyOutOfRange { key });
            }
        }

        // Everything is assembled in memory (~350 MB for the largest table), so the
        // checksum and header are known before any output is written.
        //
        // The arena is laid out in path order, not key order: keys are hashes, so path
        // order packs each directory into the same frames (~4× smaller, and batch
        // lookups touch fewer frames). Identical paths under different keys store once.
        let mut by_path: Vec<usize> = (0..self.entries.len()).collect();
        by_path.sort_unstable_by(|&a, &b| self.entries[a].1.cmp(&self.entries[b].1));

        let mut entry_offsets = vec![0u64; self.entries.len()];
        let mut arena = Vec::new();
        let mut prev: Option<(&str, u64)> = None;
        for &i in &by_path {
            let (key, path) = &self.entries[i];
            if path.len() > u16::MAX as usize {
                return Err(BuildError::PathTooLong {
                    key: *key,
                    len: path.len(),
                });
            }
            let offset = match prev {
                Some((p, offset)) if p == &**path => offset,
                _ => {
                    let offset = arena.len() as u64;
                    arena.extend_from_slice(path.as_bytes());
                    offset
                }
            };
            entry_offsets[i] = offset;
            prev = Some((path, offset));
        }
        let arena_decompressed_size = arena.len() as u64;
        let offset_width = if arena_decompressed_size <= u32::MAX as u64 {
            OffsetWidth::U32
        } else {
            OffsetWidth::U64
        };

        let key_bytes = self.key_width.bytes();
        let mut keys = Vec::with_capacity(self.entries.len() * key_bytes);
        for &(key, _) in &self.entries {
            push_uint(&mut keys, key, key_bytes);
        }

        let offset_bytes = offset_width.bytes();
        let mut offsets = Vec::with_capacity(self.entries.len() * offset_bytes);
        for &offset in &entry_offsets {
            push_uint(&mut offsets, offset, offset_bytes);
        }

        let mut lengths = Vec::with_capacity(self.entries.len() * 2);
        for (_, path) in &self.entries {
            push_uint(&mut lengths, path.len() as u64, 2);
        }

        // The arena as stored: raw, or a zeekstd seekable stream decompressing to it.
        let (stored_arena, mut flags) = match self.compression {
            Compression::None => (arena, 0),
            Compression::Zeekstd { frame_size, level } => {
                if frame_size == 0 {
                    return Err(BuildError::ZeroFrameSize);
                }
                let mut compressed = Vec::new();
                let mut encoder = zeekstd::EncodeOptions::new()
                    .compression_level(level)
                    .frame_size_policy(zeekstd::FrameSizePolicy::Uncompressed(frame_size))
                    .into_encoder(&mut compressed)?;
                encoder.write_all(&arena)?;
                encoder.finish()?;
                (compressed, FLAG_ARENA_COMPRESSED)
            }
        };
        if self.casing == Casing::Insensitive {
            flags |= FLAG_CASE_INSENSITIVE;
        }

        // Section offsets. The offsets section is padded to its own width; that only
        // bites when a u32-key table has an odd entry count and spills to u64 offsets.
        let keys_offset = HEADER_SIZE as u64;
        let offsets_offset =
            (keys_offset + keys.len() as u64).next_multiple_of(offset_width.bytes() as u64);
        let pad = (offsets_offset - keys_offset) as usize - keys.len();
        let arena_offset = offsets_offset + offsets.len() as u64 + lengths.len() as u64;

        let mut hasher = Xxh3::new();
        hasher.update(&keys);
        hasher.update(&offsets);
        hasher.update(&lengths);
        hasher.update(&stored_arena);

        let header = Header {
            hash_kind: self.hash_kind,
            flags,
            key_width: self.key_width,
            offset_width,
            entry_count: self.entries.len() as u64,
            keys_offset,
            offsets_offset,
            arena_offset,
            arena_decompressed_size,
            arena_compressed_size: stored_arena.len() as u64,
            checksum: hasher.digest(),
        };

        out.write_all(&header.encode())?;
        out.write_all(&keys)?;
        out.write_all(&[0u8; 8][..pad])?;
        out.write_all(&offsets)?;
        out.write_all(&lengths)?;
        out.write_all(&stored_arena)?;
        out.flush()?;

        Ok(BuildStats {
            entries: self.entries.len(),
            arena_decompressed_size,
            arena_compressed_size: stored_arena.len() as u64,
            file_len: arena_offset + stored_arena.len() as u64,
        })
    }
}

/// Append `value` to `buf` as `width` little-endian bytes (2, 4, or 8); the packing
/// `read_uint` reads back. `value` must already fit in `width` bytes.
fn push_uint(buf: &mut Vec<u8>, value: u64, width: usize) {
    buf.extend_from_slice(&value.to_le_bytes()[..width]);
}
