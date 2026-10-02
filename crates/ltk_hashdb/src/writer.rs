//! Streaming builder for `.hashdb` files.

use std::io::Write;

use xxhash_rust::xxh3::Xxh3;

use crate::error::ZeekstdResultExt;
use crate::header::{
    arena_order_width, ArenaOrderRef, Header, OffsetWidth, FLAG_ARENA_COMPRESSED,
    FLAG_CASE_INSENSITIVE, HEADER_SIZE,
};
use crate::{
    ArenaOrder, BuildError, Casing, Compression, HashKind, KeyConfig, KeyWidth, MAX_FRAME_SIZE,
};

/// Collects `(key, path)` pairs, then [`HashDbWriter::build`] sorts by key, dedups,
/// assigns arena offsets, and writes the file.
pub struct HashDbWriter {
    key_width: KeyWidth,
    compression: Compression,
    hash_kind: HashKind,
    casing: Casing,
    arena_order: ArenaOrder,
    entries: Vec<(u64, Box<str>)>,
}

impl std::fmt::Debug for HashDbWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashDbWriter")
            .field("key_width", &self.key_width)
            .field("compression", &self.compression)
            .field("hash_kind", &self.hash_kind)
            .field("casing", &self.casing)
            .field("arena_order", &self.arena_order)
            .field("entries", &self.entries.len())
            .finish()
    }
}

/// Sizes reported by a successful [`HashDbWriter::build`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BuildStats {
    /// Number of entries written, after duplicates were removed.
    pub entries: usize,

    /// Total length of the stored paths, in bytes.
    pub arena_decompressed_size: u64,

    /// Size of the arena on disk, in bytes. Equal to `arena_decompressed_size`
    /// for [`Compression::None`].
    pub arena_compressed_size: u64,

    /// Bytes spent on the arena-order section, `0` when it was omitted.
    pub arena_order_size: u64,

    /// Size of the whole file, in bytes.
    pub file_len: u64,
}

impl HashDbWriter {
    /// A writer for `key_width` keys, with the algorithm unrecorded and the paths
    /// hashed as given; [`hash_kind`](Self::hash_kind) and [`casing`](Self::casing)
    /// fill those in.
    pub fn new(key_width: KeyWidth, compression: Compression) -> Self {
        Self {
            key_width,
            compression,
            hash_kind: HashKind::Unspecified,
            casing: Casing::Sensitive,
            arena_order: ArenaOrder::Omitted,
            entries: Vec::new(),
        }
    }

    /// A writer for a table whose key configuration is already known - a League
    /// table, say, whose [`Table::key_config`] states all three at once.
    ///
    /// [`Table::key_config`]: https://docs.rs/ltk_mimir_cache/latest/ltk_mimir_cache/enum.Table.html#method.key_config
    pub fn with_key_config(config: KeyConfig, compression: Compression) -> Self {
        Self::new(config.key_width(), compression)
            .hash_kind(config.hash_kind())
            .casing(config.casing())
    }

    /// Record the algorithm the keys were hashed with, so readers can hash new
    /// paths with [`HashDb::hash_path`](crate::HashDb::hash_path).
    pub fn hash_kind(mut self, kind: HashKind) -> Self {
        self.hash_kind = kind;
        self
    }

    /// Record whether the keys hash the ASCII-lowercased path ([`Casing::AsciiInsensitive`],
    /// all League tables) or the path as given. Defaults to [`Casing::Sensitive`].
    pub fn casing(mut self, casing: Casing) -> Self {
        self.casing = casing;
        self
    }

    /// Write the arena-order section, or leave it out (the default).
    ///
    /// The writer already knows the permutation - it is the sort it does to lay
    /// the arena out - so storing it costs build time nothing and file size
    /// [`ArenaOrder::Stored`]'s documented share. See that variant for what a
    /// reader does with it and what it does without it.
    pub fn arena_order(mut self, arena_order: ArenaOrder) -> Self {
        self.arena_order = arena_order;
        self
    }

    /// Add one entry.
    pub fn insert(&mut self, key: u64, path: &str) {
        self.entries.push((key, path.into()));
    }

    /// Add several entries.
    pub fn extend<'a>(&mut self, it: impl IntoIterator<Item = (u64, &'a str)>) {
        self.entries
            .extend(it.into_iter().map(|(k, p)| (k, Box::from(p))));
    }

    /// Sort by key, remove duplicates, assign offsets, and write the header,
    /// keys, offsets, lengths, arena, and (optionally) arena-order section to
    /// `out`. The whole file is assembled in memory before anything is written.
    ///
    /// # Errors
    ///
    /// - [`BuildError::DuplicateKey`] if a key was inserted with two different paths
    /// - [`BuildError::KeyOutOfRange`] if a key does not fit a [`KeyWidth::U32`] table
    /// - [`BuildError::PathTooLong`] if a path exceeds 65535 bytes
    /// - [`BuildError::InvalidFrameSize`] if the zeekstd frame size is outside
    ///   `1..=`[`MAX_FRAME_SIZE`]
    /// - [`BuildError::HashKindTooWide`] if a 64-bit [`HashKind`] is recorded
    ///   for a [`KeyWidth::U32`] table
    /// - [`BuildError::Io`] / [`BuildError::Compression`] if writing or
    ///   compressing fails
    pub fn build<W: Write>(mut self, mut out: W) -> Result<BuildStats, BuildError> {
        if self.key_width == KeyWidth::U32 && self.hash_kind.is_64_bit() {
            return Err(BuildError::HashKindTooWide {
                hash_kind: self.hash_kind,
                key_width: self.key_width,
            });
        }
        if let Compression::Zeekstd { frame_size, .. } = self.compression {
            if frame_size == 0 || frame_size > MAX_FRAME_SIZE {
                return Err(BuildError::InvalidFrameSize { frame_size });
            }
        }

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
                let mut compressed = Vec::new();
                let mut encoder = zeekstd::EncodeOptions::new()
                    .compression_level(level)
                    .frame_size_policy(zeekstd::FrameSizePolicy::Uncompressed(frame_size))
                    .into_encoder(&mut compressed)
                    .zeek()?;
                encoder.write_all(&arena)?;
                encoder.finish().zeek()?;
                (compressed, FLAG_ARENA_COMPRESSED)
            }
        };
        if self.casing == Casing::AsciiInsensitive {
            flags |= FLAG_CASE_INSENSITIVE;
        }

        // `by_path` *is* the arena-order permutation - entry indices in the order
        // their paths sit in the arena - so the section is a repacking of a sort
        // that already happened, not a second one.
        let order = match self.arena_order {
            ArenaOrder::Omitted => None,
            ArenaOrder::Stored => {
                let width = arena_order_width(self.entries.len() as u64);
                let mut packed = Vec::with_capacity(by_path.len() * width + 8);
                for &i in &by_path {
                    push_uint(&mut packed, i as u64, width);
                }

                let mut hasher = Xxh3::new();
                hasher.update(&packed);
                packed.extend_from_slice(&hasher.digest().to_le_bytes());

                Some((packed, width))
            }
        };

        // Section offsets. The offsets section is padded to its own width; that only
        // bites when a u32-key table has an odd entry count and spills to u64 offsets.
        let keys_offset = HEADER_SIZE as u64;
        let offsets_offset =
            (keys_offset + keys.len() as u64).next_multiple_of(offset_width.bytes() as u64);
        let pad = (offsets_offset - keys_offset) as usize - keys.len();
        let arena_offset = offsets_offset + offsets.len() as u64 + lengths.len() as u64;
        let arena_end = arena_offset + stored_arena.len() as u64;

        // The header's checksum stays keys‖offsets‖lengths‖arena, exactly as a
        // reader built before the arena-order section computes it; the section
        // carries its own digest instead.
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
            arena_order: order.as_ref().map(|&(_, width)| ArenaOrderRef {
                offset: arena_end,
                width,
            }),
        };

        out.write_all(&header.encode())?;
        out.write_all(&keys)?;
        out.write_all(&[0u8; 8][..pad])?;
        out.write_all(&offsets)?;
        out.write_all(&lengths)?;
        out.write_all(&stored_arena)?;
        let arena_order_size = match &order {
            Some((packed, _)) => {
                out.write_all(packed)?;
                packed.len() as u64
            }
            None => 0,
        };
        out.flush()?;

        Ok(BuildStats {
            entries: self.entries.len(),
            arena_decompressed_size,
            arena_compressed_size: stored_arena.len() as u64,
            arena_order_size,
            file_len: arena_end + arena_order_size,
        })
    }
}

/// Append `value` to `buf` as `width` little-endian bytes (2, 4, or 8); the packing
/// `read_uint` reads back. `value` must already fit in `width` bytes.
fn push_uint(buf: &mut Vec<u8>, value: u64, width: usize) {
    buf.extend_from_slice(&value.to_le_bytes()[..width]);
}
