//! Read-only, mmap-backed `.hashdb` hash table.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use xxhash_rust::xxh3::Xxh3;
use zeekstd::SeekTable;

use crate::header::Header;
use crate::{Casing, HashKind, KeyWidth, OpenError, VerifyError};

/// A read-only `.hashdb` hash table.
///
/// `open` validates the (untrusted) header and section bounds. Lookups binary-search
/// the mmap'd key array, so a miss never touches the arena; a hit on a compressed
/// arena decompresses only the containing frame(s).
pub struct HashDb {
    backing: Backing,
    header: Header,
    keys: Range<usize>,
    offsets: Range<usize>,
    lengths: Range<usize>,
    arena: Range<usize>,

    /// Present iff the arena is a zeekstd seekable stream.
    seek_table: Option<SeekTable>,

    /// Frames decompressed so far; misses must never bump it (see unit tests).
    decompressions: AtomicU64,
}

enum Backing {
    Mmap(memmap2::Mmap),
    Bytes(Cow<'static, [u8]>),
}

impl Backing {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Mmap(m) => m,
            Self::Bytes(b) => b,
        }
    }
}

/// A decompressed run of frames (raw-arena range it covers + the bytes), so
/// in-order consumers decompress each frame once rather than once per entry.
type FrameCache = Option<(Range<u64>, Vec<u8>)>;

impl HashDb {
    /// mmap `path` read-only and validate it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OpenError> {
        let file = File::open(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        Self::from_backing(Backing::Mmap(mmap))
    }

    /// Open an in-memory image (embedded tables, tests).
    pub fn open_bytes(bytes: impl Into<Cow<'static, [u8]>>) -> Result<Self, OpenError> {
        Self::from_backing(Backing::Bytes(bytes.into()))
    }

    fn from_backing(backing: Backing) -> Result<Self, OpenError> {
        let data = backing.bytes();
        let header = Header::decode(data)?;

        if !header.arena_compressed()
            && header.arena_compressed_size != header.arena_decompressed_size
        {
            return Err(OpenError::MalformedHeader(
                "raw arena sizes disagree (compressed != decompressed)",
            ));
        }

        let keys_len = header
            .entry_count
            .checked_mul(header.key_width.bytes() as u64)
            .ok_or(OpenError::MalformedHeader("entry_count overflows"))?;
        let offsets_len = header
            .entry_count
            .checked_mul(header.offset_width.bytes() as u64)
            .ok_or(OpenError::MalformedHeader("entry_count overflows"))?;
        let lengths_offset = header
            .offsets_offset
            .checked_add(offsets_len)
            .ok_or(OpenError::MalformedHeader("section extent overflows"))?;
        let lengths_len = header.entry_count * 2;

        let keys = section(data.len(), header.keys_offset, keys_len)?;
        let offsets = section(data.len(), header.offsets_offset, offsets_len)?;
        let lengths = section(data.len(), lengths_offset, lengths_len)?;
        let arena = section(
            data.len(),
            header.arena_offset,
            header.arena_compressed_size,
        )?;

        // Parse the trailing seek table and pin its totals against the header, so
        // frame offsets can be trusted on later reads.
        let seek_table = if header.arena_compressed() {
            let mut cursor = std::io::Cursor::new(&data[arena.clone()]);
            let st = SeekTable::from_seekable(&mut cursor)?;
            let total = match st.num_frames() {
                0 => 0,
                n => st.frame_end_decomp(n - 1)?,
            };
            if total != header.arena_decompressed_size {
                return Err(OpenError::Malformed(
                    "seek table decompressed size disagrees with header",
                ));
            }
            if st.max_frame_size_decomp() as usize > zeekstd::SEEKABLE_MAX_FRAME_SIZE {
                return Err(OpenError::Malformed(
                    "frame exceeds seekable-format maximum",
                ));
            }
            Some(st)
        } else {
            None
        };

        // Per-entry extents aren't validated here (keeps `open` O(1)); each read
        // bounds-checks its own, reading out-of-bounds as a miss. `verify()` reports them.
        Ok(Self {
            backing,
            header,
            keys,
            offsets,
            lengths,
            arena,
            seek_table,
            decompressions: AtomicU64::new(0),
        })
    }

    /// Look up a hash. Raw arenas borrow the path from the mmap; compressed arenas
    /// decompress its frame(s). Returns `None` for a miss or an entry that won't
    /// decompress (corrupt file - see [`HashDb::verify`]).
    pub fn get(&self, hash: u64) -> Option<Cow<'_, str>> {
        self.index_of(hash).and_then(|i| self.str_at(i))
    }

    /// Membership test; never touches the arena.
    pub fn contains(&self, hash: u64) -> bool {
        self.index_of(hash).is_some()
    }

    /// Bulk lookup. Hits resolve in arena order so each frame decompresses at most
    /// once (same-directory hashes cluster into the same frames). Yielded in input order.
    pub fn get_batch<'a>(
        &'a self,
        hashes: &'a [u64],
    ) -> impl Iterator<Item = (u64, Option<Cow<'a, str>>)> + 'a {
        let indices: Vec<Option<usize>> = hashes.iter().map(|&h| self.index_of(h)).collect();
        let mut order: Vec<usize> = (0..hashes.len()).collect();
        // Resolve hits in arena order (misses sort last) so each frame decompresses once.
        order.sort_unstable_by_key(|&p| indices[p].map_or(u64::MAX, |i| self.offset_at(i)));

        let mut results: Vec<Option<Cow<'a, str>>> = Vec::new();
        results.resize_with(hashes.len(), || None);
        let mut cache: FrameCache = None;
        for p in order {
            if let Some(i) = indices[p] {
                results[p] = self.str_at_cached(i, &mut cache);
            }
        }
        hashes.iter().copied().zip(results)
    }

    pub fn len(&self) -> usize {
        self.header.entry_count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.header.entry_count == 0
    }

    pub fn key_width(&self) -> KeyWidth {
        self.header.key_width
    }

    pub fn hash_kind(&self) -> HashKind {
        self.header.hash_kind
    }

    /// Whether the keys hash the lowercased path (from the `case_insensitive`
    /// header flag).
    pub fn casing(&self) -> Casing {
        self.header.casing()
    }

    /// Whether the arena is zeekstd-compressed on disk.
    pub fn is_compressed(&self) -> bool {
        self.header.arena_compressed()
    }

    /// Total length of all path strings (the raw arena), in bytes.
    pub fn arena_decompressed_size(&self) -> u64 {
        self.header.arena_decompressed_size
    }

    /// Bytes the arena occupies on disk (== decompressed size for raw arenas).
    pub fn arena_compressed_size(&self) -> u64 {
        self.header.arena_compressed_size
    }

    /// Hash a path string with **this table's** algorithm and casing rule (from
    /// the `hash_kind` header field - falling back on key width when
    /// unspecified - and the `case_insensitive` flag).
    pub fn hash_path(&self, path: &str) -> u64 {
        self.header
            .hash_kind
            .hash(path, self.header.casing(), self.header.key_width)
    }

    /// Iterate entries in arena order (path order, **not** key order) so each frame
    /// decompresses once. Entries that fail to decompress are skipped; `verify()` reports them.
    pub fn iter(&self) -> impl Iterator<Item = (u64, Cow<'_, str>)> {
        let mut cache: FrameCache = None;
        self.arena_order().into_iter().filter_map(move |i| {
            let path = self.str_at_cached(i, &mut cache)?;
            Some((self.key_at(i), path))
        })
    }

    /// Opt-in fully-resident mode: decode everything into an owned map.
    pub fn load_all(&self) -> HashMap<u64, Box<str>> {
        self.iter()
            .map(|(k, s)| (k, s.into_owned().into_boxed_str()))
            .collect()
    }

    /// Full integrity check, skipped by `open`:
    /// - xxh3 checksum over the stored sections
    /// - keys strictly ascending
    /// - every entry in bounds and valid UTF-8 in the arena
    pub fn verify(&self) -> Result<(), VerifyError> {
        let data = self.backing.bytes();
        let mut hasher = Xxh3::new();
        hasher.update(&data[self.keys.clone()]);
        hasher.update(&data[self.offsets.clone()]);
        hasher.update(&data[self.lengths.clone()]);
        hasher.update(&data[self.arena.clone()]);
        if hasher.digest() != self.header.checksum {
            return Err(VerifyError::ChecksumMismatch);
        }

        let n = self.len();
        for i in 1..n {
            if self.key_at(i - 1) >= self.key_at(i) {
                return Err(VerifyError::Malformed("keys not strictly ascending"));
            }
        }

        if self.seek_table.is_none() {
            let arena = &data[self.arena.clone()];
            for i in 0..n {
                let slice = self
                    .extent_of(i)
                    .and_then(|(start, end)| arena.get(start as usize..end as usize))
                    .ok_or(VerifyError::Malformed("entry extends past the arena"))?;
                if std::str::from_utf8(slice).is_err() {
                    return Err(VerifyError::Malformed("entry is not valid UTF-8"));
                }
            }
            return Ok(());
        }

        // Compressed: walk entries in arena order so each frame decompresses once
        // and only the current run is resident - never the whole arena at once.
        let mut cache: FrameCache = None;
        for i in self.arena_order() {
            let (start, end) = self
                .extent_of(i)
                .ok_or(VerifyError::Malformed("entry extends past the arena"))?;
            if start == end {
                continue;
            }
            let slice = self.frame_bytes(start, end, &mut cache)?;
            if std::str::from_utf8(slice).is_err() {
                return Err(VerifyError::Malformed("entry is not valid UTF-8"));
            }
        }
        Ok(())
    }

    /// Entry indices sorted by arena offset (path order); walking them this way
    /// decompresses each frame once, keeping only the current run resident.
    fn arena_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.len()).collect();
        order.sort_unstable_by_key(|&i| self.offset_at(i));
        order
    }

    fn index_of(&self, hash: u64) -> Option<usize> {
        let mut lo = 0usize;
        let mut hi = self.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.key_at(mid) < hash {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        (lo < self.len() && self.key_at(lo) == hash).then_some(lo)
    }

    fn key_at(&self, i: usize) -> u64 {
        let w = self.header.key_width.bytes();
        read_uint(self.backing.bytes(), self.keys.start + i * w, w)
    }

    fn offset_at(&self, i: usize) -> u64 {
        let w = self.header.offset_width.bytes();
        read_uint(self.backing.bytes(), self.offsets.start + i * w, w)
    }

    fn len_at(&self, i: usize) -> u16 {
        read_uint(self.backing.bytes(), self.lengths.start + i * 2, 2) as u16
    }

    /// Entry `i`'s extent in the decompressed arena, or `None` if out of bounds.
    fn extent_of(&self, i: usize) -> Option<(u64, u64)> {
        let start = self.offset_at(i);
        let end = start.checked_add(self.len_at(i) as u64)?;
        (end <= self.header.arena_decompressed_size).then_some((start, end))
    }

    /// The path for entry `i`, or `None` if out of bounds. Invalid UTF-8 is replaced
    /// lossily rather than panicking; `verify()` reports both.
    fn str_at(&self, i: usize) -> Option<Cow<'_, str>> {
        self.str_at_cached(i, &mut None)
    }

    fn str_at_cached(&self, i: usize, cache: &mut FrameCache) -> Option<Cow<'_, str>> {
        let (start, end) = self.extent_of(i)?;
        if self.seek_table.is_none() {
            let range = self.arena.start + start as usize..self.arena.start + end as usize;
            return Some(String::from_utf8_lossy(&self.backing.bytes()[range]));
        }
        if start == end {
            return Some(Cow::Borrowed(""));
        }
        let bytes = self.frame_bytes(start, end, cache).ok()?;
        Some(Cow::Owned(String::from_utf8_lossy(bytes).into_owned()))
    }

    /// Decompressed bytes for extent `start..end`, filling `cache` with the
    /// containing frame(s) on a miss. Caller must handle the empty (`start == end`) case.
    ///
    /// Errors mean a corrupt file ([`VerifyError`]); the lookup path swallows
    /// them into a miss, only `verify` surfaces them.
    fn frame_bytes<'c>(
        &self,
        start: u64,
        end: u64,
        cache: &'c mut FrameCache,
    ) -> Result<&'c [u8], VerifyError> {
        let covered = cache
            .as_ref()
            .is_some_and(|(range, _)| range.start <= start && end <= range.end);
        if !covered {
            let st = self.seek_table.as_ref().unwrap();
            let lo = st.frame_index_decomp(start);
            let hi = st.frame_index_decomp(end - 1);
            let (cov_start, bytes) = self.read_frames(lo, hi)?;
            *cache = Some((cov_start..cov_start + bytes.len() as u64, bytes));
        }
        let (range, bytes) = cache.as_ref().unwrap();
        Ok(&bytes[(start - range.start) as usize..(end - range.start) as usize])
    }

    /// Decompress frames `lo..=hi`, returning the run's decompressed-space start
    /// offset plus the bytes. Frame content is untrusted, so every extent and
    /// output size is checked.
    fn read_frames(&self, lo: u32, hi: u32) -> Result<(u64, Vec<u8>), VerifyError> {
        let st = self.seek_table.as_ref().expect("compressed arena");
        let arena = &self.backing.bytes()[self.arena.clone()];
        let d_start = st.frame_start_decomp(lo)?;
        let total = st.frame_end_decomp(hi)? - d_start;
        // Cap the capacity hint at the (header-pinned) arena size so a corrupt seek
        // table can't force a huge allocation; a full read still allocates once.
        let cap = total.min(self.header.arena_decompressed_size) as usize;
        // Decompress each frame straight into `out` via a cursor - one reusable
        // context, no per-frame buffer, no second copy.
        let mut out = std::io::Cursor::new(Vec::with_capacity(cap));
        let mut dctx = zstd::bulk::Decompressor::new()?;
        for f in lo..=hi {
            let c_start = st.frame_start_comp(f)? as usize;
            let c_end = st.frame_end_comp(f)? as usize;
            let d_size = st.frame_size_decomp(f)? as usize;
            let frame = arena
                .get(c_start..c_end)
                .ok_or(VerifyError::Malformed("frame extent out of arena bounds"))?;
            self.decompressions.fetch_add(1, Ordering::Relaxed);
            out.get_mut().reserve(d_size);
            let pos = out.position();
            let n = dctx.decompress_to_buffer(frame, &mut out)?;
            if n != d_size {
                return Err(VerifyError::Malformed(
                    "frame decompressed to unexpected size",
                ));
            }
            out.set_position(pos + n as u64);
        }
        Ok((d_start, out.into_inner()))
    }
}

/// Read a little-endian uint of `width` bytes (2, 4, or 8) at `at`, widened to `u64`.
/// Funnels every variable-width table read so no call site matches on the width.
fn read_uint(data: &[u8], at: usize, width: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf[..width].copy_from_slice(&data[at..at + width]);
    u64::from_le_bytes(buf)
}

fn section(file_len: usize, offset: u64, len: u64) -> Result<Range<usize>, OpenError> {
    let end = offset
        .checked_add(len)
        .ok_or(OpenError::MalformedHeader("section extent overflows"))?;
    if end > file_len as u64 {
        return Err(OpenError::MalformedHeader(
            "section extends past end of file",
        ));
    }
    Ok(offset as usize..end as usize)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::atomic::Ordering;

    use super::HashDb;
    use crate::{Compression, HashDbWriter, KeyWidth};

    fn compressed_db(frame_size: u32) -> HashDb {
        let mut w = HashDbWriter::new(
            KeyWidth::U64,
            Compression::Zeekstd {
                frame_size,
                level: 3,
            },
        );
        for i in 0..100u64 {
            w.insert(
                i * 3,
                &format!("assets/characters/champ{i}/skins/skin{i}.bin"),
            );
        }
        let mut out = Cursor::new(Vec::new());
        w.build(&mut out).expect("build");
        HashDb::open_bytes(out.into_inner()).expect("open")
    }

    /// A miss is decided by the key array alone - never a frame decompression.
    #[test]
    fn misses_never_decompress() {
        let db = compressed_db(128);
        for probe in [1u64, 2, 500, u64::MAX] {
            assert_eq!(db.get(probe), None);
        }
        assert!(!db.contains(999));
        assert_eq!(db.decompressions.load(Ordering::Relaxed), 0);

        assert!(db.get(0).is_some());
        assert!(db.decompressions.load(Ordering::Relaxed) > 0);
    }

    /// In-order iteration decompresses each frame once, not once per entry.
    #[test]
    fn iter_decompresses_each_frame_once() {
        let db = compressed_db(128);
        assert_eq!(db.iter().count(), 100);
        let frames = db.seek_table.as_ref().unwrap().num_frames() as u64;
        assert!(frames > 1, "fixture should span multiple frames");
        // Boundary-straddling entries decompress both frames, so allow one re-read each.
        assert!(db.decompressions.load(Ordering::Relaxed) <= 2 * frames);
    }
}
