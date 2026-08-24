//! Read-only, mmap-backed `.hashdb` hash table.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use xxhash_rust::xxh3::Xxh3;
use zeekstd::SeekTable;

use crate::cache::{Frame, FrameCache};
use crate::header::Header;
use crate::{Casing, HashKind, KeyConfig, KeyWidth, OpenError, PathRef, VerifyError};

/// Decompressed frame bytes a table caches by default: 4 MiB, i.e. 256 frames at the
/// published 16 KiB frame size.
///
/// Enough that a consumer walking a WAD's chunks in path order keeps hitting the frames
/// it just decompressed, small enough to be an unremarkable cost per open table.
pub const DEFAULT_FRAME_CACHE_BYTES: usize = 4 << 20;

/// Open-time knobs for a [`HashDb`].
///
/// ```no_run
/// # use ltk_hashdb::HashDb;
/// // A table used for one bulk pass needs no cache; one behind a UI wants a bigger one.
/// let scratch = HashDb::options().frame_cache_bytes(0).open("game.lhdb")?;
/// let resident = HashDb::options().frame_cache_bytes(32 << 20).open("game.lhdb")?;
/// # Ok::<(), ltk_hashdb::OpenError>(())
/// ```
#[derive(Debug, Clone, Copy)]
pub struct HashDbOptions {
    frame_cache_bytes: usize,
}

impl Default for HashDbOptions {
    fn default() -> Self {
        Self {
            frame_cache_bytes: DEFAULT_FRAME_CACHE_BYTES,
        }
    }
}

impl HashDbOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cap the decompressed frames this table keeps cached; `0` disables caching.
    ///
    /// Only compressed arenas cache anything - a raw arena is read straight out of the
    /// mmap, so the budget is ignored.
    pub fn frame_cache_bytes(mut self, bytes: usize) -> Self {
        self.frame_cache_bytes = bytes;
        self
    }

    /// mmap `path` read-only and validate it.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be opened or mapped, or if the header or section
    /// bounds do not validate - see [`OpenError`].
    ///
    /// # Safety of the mapping
    ///
    /// See [`HashDb::open`] for the obligation a caller takes on by mapping a file.
    pub fn open(self, path: impl AsRef<Path>) -> Result<HashDb, OpenError> {
        let file = File::open(path)?;
        // SAFETY: `Mmap::map` is unsound if the mapped bytes change underneath us, so
        // every writer of a `.hashdb` this crate ships must leave published files
        // immutable. `ltk_mimir_cache` upholds that: `commit` writes a new versioned
        // filename and renames it into place rather than over an existing one, and `gc`
        // only unlinks (an unlinked file keeps its pages alive for anyone still mapping
        // it). A caller mapping a file some other process may truncate or rewrite in
        // place gets undefined behaviour, not an error - `HashDb::open` documents this.
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        HashDb::from_backing(Backing::Mmap(mmap), self)
    }

    /// Open an in-memory image (embedded tables, tests).
    ///
    /// # Errors
    ///
    /// Fails if the header or section bounds do not validate - see [`OpenError`].
    pub fn open_bytes(self, bytes: impl Into<Cow<'static, [u8]>>) -> Result<HashDb, OpenError> {
        HashDb::from_backing(Backing::Bytes(bytes.into()), self)
    }
}

/// A read-only `.hashdb` hash table.
///
/// `open` validates the (untrusted) header and section bounds. Lookups binary-search
/// the mmap'd key array, so a miss never touches the arena; a hit on a compressed
/// arena decompresses only the containing frame, and keeps it cached for the lookups
/// after it.
///
/// Cloning is cheap - every clone shares one mapping, one seek table, and one frame
/// cache - so a `HashDb` is passed around rather than reopened.
#[derive(Clone)]
pub struct HashDb {
    inner: Arc<Inner>,
}

/// A handle that does not keep its table mapped.
///
/// For a registry of open tables: keep these rather than [`HashDb`]s so the registry
/// does not pin every table anyone ever opened. [`upgrade`](WeakHashDb::upgrade) hands
/// back a live handle while one is still held elsewhere.
#[derive(Clone, Debug)]
pub struct WeakHashDb {
    inner: Weak<Inner>,
}

impl WeakHashDb {
    /// A live handle to the table, or `None` once the last one was dropped.
    pub fn upgrade(&self) -> Option<HashDb> {
        self.inner.upgrade().map(|inner| HashDb { inner })
    }
}

/// Everything a table's clones share: the bytes, where the sections are, and the
/// frames decompressed out of them so far.
struct Inner {
    backing: Backing,
    header: Header,
    keys: Range<usize>,
    offsets: Range<usize>,
    lengths: Range<usize>,
    arena: Range<usize>,

    /// Present iff the arena is a zeekstd seekable stream.
    seek_table: Option<SeekTable>,

    cache: FrameCache,

    /// Frames decompressed so far; misses must never bump it (see unit tests).
    decompressions: AtomicU64,

    /// Cleared for good the first time a lookup swallows a decompression failure,
    /// so "this build knows nothing" can be told apart from "this file is broken".
    healthy: AtomicBool,
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

/// Entry bytes as they were found: lent by the mmap, lent by a cached frame, or
/// spliced together because the entry crossed a frame boundary.
enum Bytes<'a> {
    Borrowed(&'a [u8]),
    Frame {
        frame: Arc<Frame>,
        start: usize,
        len: usize,
    },
    Spliced(Vec<u8>),
}

impl Bytes<'_> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::Frame { frame, start, len } => &frame.bytes()[*start..*start + *len],
            Self::Spliced(bytes) => bytes,
        }
    }
}

impl<'a> From<Bytes<'a>> for PathRef<'a> {
    fn from(bytes: Bytes<'a>) -> Self {
        match bytes {
            Bytes::Borrowed(bytes) => match String::from_utf8_lossy(bytes) {
                Cow::Borrowed(path) => PathRef::borrowed(path),
                Cow::Owned(path) => PathRef::owned(path),
            },
            Bytes::Frame { frame, start, len } => PathRef::from_frame(frame, start, len),
            Bytes::Spliced(bytes) => match String::from_utf8(bytes) {
                Ok(path) => PathRef::owned(path),
                Err(e) => PathRef::owned(String::from_utf8_lossy(e.as_bytes()).into_owned()),
            },
        }
    }
}

thread_local! {
    /// One decompression context per thread: creating one per frame would cost more
    /// than the decompression, and sharing one across threads would serialize them.
    static DCTX: RefCell<Option<zstd::bulk::Decompressor<'static>>> = const { RefCell::new(None) };
}

/// Run `f` against this thread's decompression context, creating it on first use.
fn with_dctx<R>(
    f: impl FnOnce(&mut zstd::bulk::Decompressor<'static>) -> Result<R, VerifyError>,
) -> Result<R, VerifyError> {
    DCTX.with(|cell| {
        let mut slot = cell.borrow_mut();
        let dctx = match &mut *slot {
            Some(dctx) => dctx,
            slot => slot.insert(zstd::bulk::Decompressor::new()?),
        };

        f(dctx)
    })
}

impl HashDb {
    /// mmap `path` read-only and validate it.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be opened or mapped, or if the header or section
    /// bounds do not validate - see [`OpenError`].
    ///
    /// # Safety of the mapping
    ///
    /// The mapping is only sound while the file's bytes do not change. Published
    /// `.hashdb` files are immutable by contract - a new version ships as a new
    /// filename, and old versions are only ever unlinked - which is what discharges
    /// the `unsafe` here. If you manage your own tables, uphold the same rule: never
    /// truncate or rewrite a file in place while a `HashDb` maps it. Doing so is
    /// undefined behaviour rather than an error you can catch. Build to a temporary
    /// name and rename, or use [`open_bytes`](HashDb::open_bytes) for images you
    /// mutate.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OpenError> {
        HashDbOptions::default().open(path)
    }

    /// Open an in-memory image (embedded tables, tests).
    ///
    /// # Errors
    ///
    /// Fails if the header or section bounds do not validate - see [`OpenError`].
    pub fn open_bytes(bytes: impl Into<Cow<'static, [u8]>>) -> Result<Self, OpenError> {
        HashDbOptions::default().open_bytes(bytes)
    }

    /// Open-time knobs - the frame cache budget, today.
    pub fn options() -> HashDbOptions {
        HashDbOptions::default()
    }

    /// A handle that does not keep this table mapped - see [`WeakHashDb`].
    pub fn downgrade(&self) -> WeakHashDb {
        WeakHashDb {
            inner: Arc::downgrade(&self.inner),
        }
    }

    fn from_backing(backing: Backing, options: HashDbOptions) -> Result<Self, OpenError> {
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

        // Size the cache to this table: never more slots than it has frames, and
        // nothing at all for a raw arena, which is read straight out of the mmap.
        let cache = match &seek_table {
            Some(st) => FrameCache::new(
                options.frame_cache_bytes,
                st.max_frame_size_decomp() as usize,
                st.num_frames() as usize,
            ),
            None => FrameCache::new(0, 0, 0),
        };

        // Per-entry extents aren't validated here (keeps `open` O(1)); each read
        // bounds-checks its own, reading out-of-bounds as a miss. `verify()` reports them.
        Ok(Self {
            inner: Arc::new(Inner {
                backing,
                header,
                keys,
                offsets,
                lengths,
                arena,
                seek_table,
                cache,
                decompressions: AtomicU64::new(0),
                healthy: AtomicBool::new(true),
            }),
        })
    }

    /// Look up a hash. The path is lent by the mmap or by a cached frame, so a hit
    /// allocates nothing. Returns `None` for a miss or an entry that won't decompress
    /// (corrupt file - see [`HashDb::verify`]).
    pub fn get(&self, hash: u64) -> Option<PathRef<'_>> {
        let i = self.inner.index_of(hash)?;
        self.inner.lookup(i).map(PathRef::from)
    }

    /// Look up a hash, surfacing a corrupt arena instead of reporting it as a miss.
    ///
    /// [`get`](HashDb::get) follows the format's rule that an entry which will not
    /// decompress reads as a miss. That is right for resolving names and wrong for
    /// telling a user why every name in an install came back unknown, so this returns
    /// the failure at the call site instead. `Ok(None)` is a genuine miss.
    ///
    /// # Errors
    ///
    /// Fails with [`VerifyError`] when the entry's frame will not decompress or its
    /// extent runs outside the arena - always a corrupt file.
    pub fn try_get(&self, hash: u64) -> Result<Option<PathRef<'_>>, VerifyError> {
        let Some(i) = self.inner.index_of(hash) else {
            return Ok(None);
        };

        Ok(self.inner.bytes_at(i)?.map(PathRef::from))
    }

    /// Copy a path into `buf`, replacing what was there. `false` for a miss.
    ///
    /// The counterpart to [`get`](HashDb::get) for a caller looping over a reusable
    /// buffer: it copies the bytes out and holds no frame afterwards, where a retained
    /// [`PathRef`] keeps its frame resident.
    pub fn get_into(&self, hash: u64, buf: &mut String) -> bool {
        let Some(i) = self.inner.index_of(hash) else {
            return false;
        };
        let Some(bytes) = self.inner.lookup(i) else {
            return false;
        };

        buf.clear();
        match std::str::from_utf8(bytes.as_slice()) {
            Ok(path) => buf.push_str(path),
            Err(_) => buf.push_str(&String::from_utf8_lossy(bytes.as_slice())),
        }

        true
    }

    /// Whether every lookup so far has read cleanly.
    ///
    /// Turns false for good the first time a lookup hits an entry that will not
    /// decompress - bit rot, a truncated write - and stays false. Nothing re-verifies
    /// an installed table after its download checksum, so this is the cheap signal that
    /// a table needs [`verify`](HashDb::verify) rather than a redownload of the world.
    pub fn is_healthy(&self) -> bool {
        self.inner.healthy.load(Ordering::Relaxed)
    }

    /// Membership test; never touches the arena.
    pub fn contains(&self, hash: u64) -> bool {
        self.inner.index_of(hash).is_some()
    }

    /// Bulk lookup. Hits resolve in arena order so each frame decompresses at most
    /// once (same-directory hashes cluster into the same frames). Yielded in input order.
    pub fn get_batch<'a>(
        &'a self,
        hashes: &[u64],
    ) -> impl Iterator<Item = (u64, Option<PathRef<'a>>)> + 'a {
        let indices: Vec<Option<usize>> = hashes.iter().map(|&h| self.inner.index_of(h)).collect();
        let mut order: Vec<usize> = (0..hashes.len()).collect();
        // Resolve hits in arena order (misses sort last) so each frame decompresses once.
        order.sort_unstable_by_key(|&p| indices[p].map_or(u64::MAX, |i| self.inner.offset_at(i)));

        let mut results: Vec<Option<PathRef<'a>>> = Vec::new();
        results.resize_with(hashes.len(), || None);
        for p in order {
            if let Some(i) = indices[p] {
                results[p] = self.inner.lookup(i).map(PathRef::from);
            }
        }

        let out: Vec<(u64, Option<PathRef<'a>>)> = hashes.iter().copied().zip(results).collect();
        out.into_iter()
    }

    /// Resolve a batch without collecting it, calling `f` per hash as it resolves.
    ///
    /// The allocation-free batch: where [`get_batch`](HashDb::get_batch) materialises
    /// every result before yielding the first, this hands each one straight to `f`.
    /// Calls arrive in **arena order**, not input order - that is what lets each frame
    /// decompress once - so the first argument is the hash's position in `hashes`.
    pub fn for_each_batch(&self, hashes: &[u64], mut f: impl FnMut(usize, u64, Option<&str>)) {
        let indices: Vec<Option<usize>> = hashes.iter().map(|&h| self.inner.index_of(h)).collect();
        let mut order: Vec<usize> = (0..hashes.len()).collect();
        order.sort_unstable_by_key(|&p| indices[p].map_or(u64::MAX, |i| self.inner.offset_at(i)));

        for p in order {
            let path = indices[p].and_then(|i| self.inner.lookup(i));
            match path {
                Some(bytes) => {
                    let path = PathRef::from(bytes);
                    f(p, hashes[p], Some(&path));
                }
                None => f(p, hashes[p], None),
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.header.entry_count == 0
    }

    pub fn key_width(&self) -> KeyWidth {
        self.inner.header.key_width
    }

    pub fn hash_kind(&self) -> HashKind {
        self.inner.header.hash_kind
    }

    /// Whether the keys hash the ASCII-lowercased path (from the
    /// `case_insensitive` header flag).
    pub fn casing(&self) -> Casing {
        self.inner.header.casing()
    }

    /// Width, algorithm, and casing as one value - what a probe must be hashed
    /// under to be answerable here, and what every base of a
    /// [`LayeredHashDb`](crate::LayeredHashDb) must agree on.
    pub fn key_config(&self) -> KeyConfig {
        KeyConfig::new(self.key_width(), self.hash_kind(), self.casing())
    }

    /// Whether the arena is zeekstd-compressed on disk.
    pub fn is_compressed(&self) -> bool {
        self.inner.header.arena_compressed()
    }

    /// Total length of all path strings (the raw arena), in bytes.
    pub fn arena_decompressed_size(&self) -> u64 {
        self.inner.header.arena_decompressed_size
    }

    /// Bytes the arena occupies on disk (== decompressed size for raw arenas).
    pub fn arena_compressed_size(&self) -> u64 {
        self.inner.header.arena_compressed_size
    }

    /// Number of zstd frames this table has decompressed over its lifetime.
    #[cfg(test)]
    pub(crate) fn decompressions(&self) -> u64 {
        self.inner.decompressions.load(Ordering::Relaxed)
    }

    /// Hash a path string with **this table's** algorithm and casing rule (from
    /// the `hash_kind` header field - falling back on key width when
    /// unspecified - and the `case_insensitive` flag).
    pub fn hash_path(&self, path: &str) -> u64 {
        self.key_config().hash(path)
    }

    /// Iterate entries in arena order (path order, **not** key order) so each frame
    /// decompresses once. Entries that fail to decompress are skipped; `verify()` reports them.
    pub fn iter(&self) -> impl Iterator<Item = (u64, PathRef<'_>)> {
        self.inner.arena_order().into_iter().filter_map(move |i| {
            let bytes = self.inner.lookup(i)?;
            Some((self.inner.key_at(i), PathRef::from(bytes)))
        })
    }

    /// Opt-in fully-resident mode: decode everything into an owned map.
    pub fn load_all(&self) -> HashMap<u64, Box<str>> {
        self.iter().map(|(k, path)| (k, path.into())).collect()
    }

    /// Full integrity check, skipped by `open`:
    /// - xxh3 checksum over the stored sections
    /// - keys strictly ascending
    /// - every entry in bounds and valid UTF-8 in the arena
    ///
    /// The last of those decompresses the whole arena. Use
    /// [`verify_index`](HashDb::verify_index) when the question is only whether
    /// the file survived being stored.
    ///
    /// # Errors
    ///
    /// Fails with [`VerifyError`] on the first problem it finds: a checksum mismatch,
    /// out-of-order keys, an entry that runs past the arena, one that is not valid
    /// UTF-8, or a frame that will not decompress.
    pub fn verify(&self) -> Result<(), VerifyError> {
        let inner = &*self.inner;
        inner.verify_checksum()?;
        inner.verify_key_order()?;

        // Compressed arenas are walked in arena order so each frame decompresses once
        // and only the current run is resident - never the whole arena at once. A raw
        // arena is already resident, so key order is as good and needs no permutation.
        if inner.seek_table.is_none() {
            for i in 0..inner.len() {
                inner.verify_entry(i)?;
            }
        } else {
            for i in inner.arena_order() {
                inner.verify_entry(i)?;
            }
        }

        Ok(())
    }

    /// The cheap tier between [`open`](HashDb::open) and
    /// [`verify`](HashDb::verify): the checksum and the key ordering, with not
    /// one path decoded.
    ///
    /// This still hashes **every stored byte, arena included**, so it catches
    /// what actually happens to an installed table - bit rot, a truncating
    /// write, a half-finished copy. What it skips is proving the file is
    /// *well-formed*: that each entry's extent lands inside the decompressed
    /// arena, that every frame decompresses, that every path is UTF-8. Damage
    /// changes bytes and is caught here; a table that was built wrong needs
    /// [`verify`](HashDb::verify).
    ///
    /// The difference is the arena: `verify` decompresses all of it (and sorts a
    /// permutation to do so in arena order), where this reads the file once and
    /// stops. On the 42 MiB `game` table - 2.3M entries - that measures ~85 ms
    /// against ~940 ms, warm.
    ///
    /// # Errors
    ///
    /// [`VerifyError::ChecksumMismatch`] if the stored bytes do not hash to the
    /// header's digest, or [`VerifyError::Malformed`] if the keys are not
    /// strictly ascending.
    pub fn verify_index(&self) -> Result<(), VerifyError> {
        let inner = &*self.inner;
        inner.verify_checksum()?;
        inner.verify_key_order()
    }
}

impl fmt::Debug for HashDb {
    /// Shape only - counts, widths, and flags. Never entries.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = &*self.inner;
        f.debug_struct("HashDb")
            .field("entries", &inner.header.entry_count)
            .field("key_width", &inner.header.key_width)
            .field("hash_kind", &inner.header.hash_kind)
            .field("casing", &inner.header.casing())
            .field("compressed", &inner.header.arena_compressed())
            .field(
                "arena_decompressed_size",
                &inner.header.arena_decompressed_size,
            )
            .field("arena_compressed_size", &inner.header.arena_compressed_size)
            .field(
                "frames",
                &inner.seek_table.as_ref().map_or(0, SeekTable::num_frames),
            )
            .field("cached_frames", &inner.cache.capacity())
            .finish()
    }
}

impl Inner {
    fn len(&self) -> usize {
        self.header.entry_count as usize
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

    /// Entry `i`'s bytes, or `None` if its extent runs past the arena.
    ///
    /// Errors mean a corrupt file ([`VerifyError`]); the lookup path swallows them
    /// into a miss, only `verify` surfaces them.
    fn bytes_at(&self, i: usize) -> Result<Option<Bytes<'_>>, VerifyError> {
        let Some((start, end)) = self.extent_of(i) else {
            return Ok(None);
        };

        let Some(seek_table) = self.seek_table.as_ref() else {
            let range = self.arena.start + start as usize..self.arena.start + end as usize;
            return Ok(Some(Bytes::Borrowed(&self.backing.bytes()[range])));
        };
        if start == end {
            return Ok(Some(Bytes::Borrowed(&[])));
        }

        let first = seek_table.frame_index_decomp(start);
        let last = seek_table.frame_index_decomp(end - 1);
        let frame = self.frame(first)?;
        let frame_start = seek_table.frame_start_decomp(first)?;
        let offset = (start - frame_start) as usize;

        if first == last {
            let len = (end - start) as usize;
            if offset + len > frame.bytes().len() {
                return Err(VerifyError::Malformed("entry extends past its frame"));
            }

            return Ok(Some(Bytes::Frame {
                frame,
                start: offset,
                len,
            }));
        }

        // The entry straddles a frame boundary - roughly one entry per frame. Splice
        // the pieces into a buffer of its own rather than lending out either frame.
        let mut spliced = Vec::with_capacity((end - start) as usize);
        let head = frame
            .bytes()
            .get(offset..)
            .ok_or(VerifyError::Malformed("entry starts past its frame"))?;
        spliced.extend_from_slice(head);

        for index in first + 1..=last {
            let frame = self.frame(index)?;
            let frame_start = seek_table.frame_start_decomp(index)?;
            let take = (end - frame_start).min(frame.bytes().len() as u64) as usize;
            let tail = frame.bytes().get(..take).ok_or(VerifyError::Malformed(
                "frame shorter than the seek table says",
            ))?;
            spliced.extend_from_slice(tail);
        }

        Ok(Some(Bytes::Spliced(spliced)))
    }

    /// Entry `i`'s bytes for a lookup: a failure to decompress reads as a miss, and
    /// trips the health flag on its way past.
    fn lookup(&self, i: usize) -> Option<Bytes<'_>> {
        match self.bytes_at(i) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.healthy.store(false, Ordering::Relaxed);
                None
            }
        }
    }

    /// Check one entry the way `verify` needs it checked: in bounds and valid UTF-8.
    /// The header's digest against the bytes as they sit on disk. Compressed
    /// arenas are hashed compressed, so nothing is decoded to run this.
    fn verify_checksum(&self) -> Result<(), VerifyError> {
        let data = self.backing.bytes();
        let mut hasher = Xxh3::new();
        hasher.update(&data[self.keys.clone()]);
        hasher.update(&data[self.offsets.clone()]);
        hasher.update(&data[self.lengths.clone()]);
        hasher.update(&data[self.arena.clone()]);

        if hasher.digest() != self.header.checksum {
            return Err(VerifyError::ChecksumMismatch);
        }

        Ok(())
    }

    /// Strictly ascending keys - the invariant every lookup's binary search
    /// depends on, and the one a reordered or duplicated key breaks silently.
    fn verify_key_order(&self) -> Result<(), VerifyError> {
        for i in 1..self.len() {
            if self.key_at(i - 1) >= self.key_at(i) {
                return Err(VerifyError::Malformed("keys not strictly ascending"));
            }
        }

        Ok(())
    }

    fn verify_entry(&self, i: usize) -> Result<(), VerifyError> {
        let bytes = self
            .bytes_at(i)?
            .ok_or(VerifyError::Malformed("entry extends past the arena"))?;
        if std::str::from_utf8(bytes.as_slice()).is_err() {
            return Err(VerifyError::Malformed("entry is not valid UTF-8"));
        }

        Ok(())
    }

    /// Frame `index`, decompressing it only if it is not already cached.
    fn frame(&self, index: u32) -> Result<Arc<Frame>, VerifyError> {
        if let Some(frame) = self.cache.get(index) {
            return Ok(frame);
        }

        // Decompressed outside any cache lock, so concurrent readers never wait on each
        // other. Two threads racing on one frame both decompress it; the loser's copy is
        // simply dropped, which is cheaper than serializing every miss.
        let frame = Arc::new(Frame::from(self.decompress(index)?));
        self.cache.insert(index, &frame);

        Ok(frame)
    }

    /// Decompress one frame. Frame content is untrusted, so the extent and the
    /// resulting size are both checked.
    fn decompress(&self, index: u32) -> Result<Vec<u8>, VerifyError> {
        let seek_table = self.seek_table.as_ref().expect("compressed arena");
        let arena = &self.backing.bytes()[self.arena.clone()];
        let start = seek_table.frame_start_comp(index)? as usize;
        let end = seek_table.frame_end_comp(index)? as usize;
        let size = seek_table.frame_size_decomp(index)? as usize;
        let compressed = arena
            .get(start..end)
            .ok_or(VerifyError::Malformed("frame extent out of arena bounds"))?;

        // Cap the capacity hint at the (header-pinned) arena size so a corrupt seek
        // table can't force a huge allocation.
        let capacity = size.min(self.header.arena_decompressed_size as usize);
        let mut buffer = self.cache.take_buffer(capacity);

        self.decompressions.fetch_add(1, Ordering::Relaxed);
        let written = with_dctx(|dctx| Ok(dctx.decompress_to_buffer(compressed, &mut buffer)?))?;
        if written != size {
            return Err(VerifyError::Malformed(
                "frame decompressed to unexpected size",
            ));
        }

        Ok(buffer)
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
        compressed_db_with(frame_size, super::DEFAULT_FRAME_CACHE_BYTES)
    }

    fn compressed_db_with(frame_size: u32, cache_bytes: usize) -> HashDb {
        HashDb::options()
            .frame_cache_bytes(cache_bytes)
            .open_bytes(compressed_bytes(frame_size))
            .expect("open")
    }

    /// 100 clustered paths, keyed `i * 3`, spanning several frames.
    fn compressed_bytes(frame_size: u32) -> Vec<u8> {
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
        out.into_inner()
    }

    /// A miss is decided by the key array alone - never a frame decompression.
    #[test]
    fn misses_never_decompress() {
        let db = compressed_db(128);
        for probe in [1u64, 2, 500, u64::MAX] {
            assert_eq!(db.get(probe), None);
        }
        assert!(!db.contains(999));
        assert_eq!(db.inner.decompressions.load(Ordering::Relaxed), 0);

        assert!(db.get(0).is_some());
        assert!(db.inner.decompressions.load(Ordering::Relaxed) > 0);
    }

    /// In-order iteration decompresses each frame once, not once per entry.
    #[test]
    fn iter_decompresses_each_frame_once() {
        let db = compressed_db(128);
        assert_eq!(db.iter().count(), 100);
        let frames = db.inner.seek_table.as_ref().unwrap().num_frames() as u64;
        assert!(frames > 1, "fixture should span multiple frames");
        // Boundary-straddling entries decompress both frames, so allow one re-read each.
        assert!(db.inner.decompressions.load(Ordering::Relaxed) <= 2 * frames);
    }

    /// The point of the cache: repeating a lookup must not decompress again, and
    /// neighbouring keys share the frame their neighbour just paid for.
    #[test]
    fn cached_frames_are_not_decompressed_twice() {
        let db = compressed_db(128);
        assert!(db.get(0).is_some());
        let after_first = db.decompressions();

        for _ in 0..10 {
            assert!(db.get(0).is_some());
        }
        assert_eq!(db.decompressions(), after_first, "repeat lookups were free");

        // A clone shares the cache rather than starting its own.
        let clone = db.clone();
        assert!(clone.get(0).is_some());
        assert_eq!(clone.decompressions(), after_first);
    }

    /// A table whose arena no longer decompresses must degrade to misses, say so via
    /// `is_healthy`, and surface the reason through `try_get` - never panic.
    #[test]
    fn a_corrupt_frame_reports_unhealthy() {
        let mut bytes = compressed_bytes(128);
        // The seek table lives at the end of the arena, so scribbling over the start
        // leaves the file openable and breaks only the frames it lands on.
        let arena_offset = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
        for byte in &mut bytes[arena_offset..arena_offset + 64] {
            *byte = 0xff;
        }

        let db = HashDb::open_bytes(bytes).expect("open");
        assert!(db.is_healthy(), "nothing has been read yet");

        let keys: Vec<u64> = (0..100u64).map(|i| i * 3).collect();
        let swallowed = keys.iter().filter(|&&k| db.get(k).is_none()).count();
        assert!(swallowed > 0, "the corrupt frame should swallow lookups");
        assert!(!db.is_healthy(), "and say so afterwards");

        // The same lookups report the corruption when asked to.
        let surfaced = keys.iter().filter(|&&k| db.try_get(k).is_err()).count();
        assert_eq!(surfaced, swallowed);

        // Entries in the untouched frames still resolve.
        assert!(keys.iter().any(|&k| db.get(k).is_some()));
    }

    #[test]
    fn for_each_batch_matches_get_batch() {
        let db = compressed_db(128);
        let probes = [0u64, 3, 297, 1, 99, 0];

        let mut streamed: Vec<(u64, Option<String>)> = vec![(0, None); probes.len()];
        db.for_each_batch(&probes, |i, hash, path| {
            streamed[i] = (hash, path.map(str::to_owned));
        });

        let collected: Vec<(u64, Option<String>)> = db
            .get_batch(&probes)
            .map(|(hash, path)| (hash, path.map(|p| p.into_owned())))
            .collect();
        assert_eq!(streamed, collected);
    }

    #[test]
    fn get_into_reuses_the_callers_buffer() {
        let db = compressed_db(128);
        let mut buf = String::from("stale contents");

        assert!(db.get_into(0, &mut buf));
        assert_eq!(buf, "assets/characters/champ0/skins/skin0.bin");

        // A miss leaves the buffer alone rather than clearing it.
        assert!(!db.get_into(1, &mut buf));
        assert_eq!(buf, "assets/characters/champ0/skins/skin0.bin");
    }

    /// With caching off, every lookup pays for its own frame - the old behaviour,
    /// still available for one-shot passes.
    #[test]
    fn a_disabled_cache_decompresses_every_time() {
        let db = compressed_db_with(128, 0);
        assert!(db.get(0).is_some());
        let after_first = db.decompressions();

        assert!(db.get(0).is_some());
        assert!(db.decompressions() > after_first);
    }
}
