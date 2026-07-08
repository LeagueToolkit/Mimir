# The `.hashdb` file format

> Byte-level specification, format version **1**. Implemented by the `ltk_hashdb` crate.

`.hashdb` is a general-purpose, read-only, mmap-backed table mapping integer hash keys to
string values. It is not League-specific; League Toolkit ships its hash tables in this
format under the `.lhdb` extension (identical bytes, League convention).

- Extension: `.hashdb` (League Toolkit tables use `.lhdb`; same format, same magic)
- Magic: `b"HASHDB\0\0"` (8 bytes)
- Endianness: **little-endian only** (targets x86-64 / aarch64)
- One file per logical table (game, lcu, binentries, bintypes, binfields, binhashes, and
  the two rst variants - xxh64 and xxh3 hash lists are separate files).
- The file is immutable once published; updates ship as new versioned files.

## Layout

```
┌──────────────────────────────────────────────┐
│ Header   (80 bytes)                          │
├──────────────────────────────────────────────┤
│ Keys     (entry_count × key_width bytes)     │
├─ padding to offset_width alignment ──────────┤
│ Offsets  (entry_count × offset_width)        │
├──────────────────────────────────────────────┤
│ Lengths  (entry_count × 2)                   │
├──────────────────────────────────────────────┤
│ Arena    (arena_compressed_size bytes)       │
└──────────────────────────────────────────────┘
```

Keys, offsets, and lengths are parallel arrays: entry `i`'s key is `keys[i]`, and its
path is the `lengths[i]` bytes at decompressed-arena position `offsets[i]`. Offsets
carry **no ordering requirement** - the arena is laid out independently of key order
(see *Arena ordering* below), and two entries may share bytes (identical paths are
stored once).

## Header (80 bytes)

| Offset | Field | Type | Notes |
|--------|-------|------|-------|
| 0 | `magic` | `[u8;8]` | `b"HASHDB\0\0"` |
| 8 | `version` | `u16` | currently `1` |
| 10 | `key_width` | `u8` | 4 = u32 table, 8 = u64 table |
| 11 | `flags` | `u8` | bit0: `arena_compressed`; other bits must be 0 |
| 12 | `off/cset_width` | `u8` | 4 or 8; the writer picks 4 while `arena_decompressed_size` fits in a u32, else 8; the reader honors whatever is declared |
| 13 | `hash_kind` | `u8` | algorithm that produced the keys, see below |
| 14 | reserved | `[u8;2]` | written as zero, ignored on read |
| 16 | `entry_count` | `u64` | |
| 24 | `keys_offset` | `u64` | file offset of the keys section; writers must 8-align it (the reference writer emits 80), readers bounds-check and honor the declared value |
| 32 | `offsets_offset` | `u64` | file offset of the offsets section; writers must `offset_width`-align it |
| 40 | `arena_offset` | `u64` | file offset of the arena |
| 48 | `arena_decompressed_size` | `u64` | raw (decompressed) arena length |
| 56 | `arena_compressed_size` | `u64` | arena bytes on disk; == decompressed if raw |
| 64 | `checksum` | `u64` | xxh3-64 of keys ‖ offsets ‖ lengths ‖ arena, each as stored on disk (inter-section padding excluded) |
| 72 | reserved | `[u8;8]` | written as zero, ignored on read |

The lengths section has no header field: it sits immediately after the offsets, at
`offsets_offset + entry_count × offset_width` (u16 entries are always 2-aligned there).

### `hash_kind`

| Value | Name | Algorithm | Used by |
|-------|------|-----------|---------|
| 0 | unspecified | consumers fall back on key width: u64 → xxh64-lower, u32 → fnv1a32-lower | - |
| 1 | xxh64-lower | XXH64, seed 0, over the **ASCII**-lowercased path | game, lcu, rst (xxh64 lists) |
| 2 | fnv1a32-lower | FNV-1a 32 over the **Unicode**-lowercased path (each char through its Unicode lowercase mapping, UTF-8 encoded) | binentries, bintypes, binfields, binhashes |
| 3 | xxh3-lower | XXH3-64 (no seed) over the **ASCII**-lowercased path | rst (v5+, `.xxh3` lists) |

The lowercasing rules follow `ltk_hash` (`WadHash` is ASCII-lowercase, `BinHash` is
Unicode-lowercase); the two coincide for the all-ASCII paths these tables hold in
practice. RST hashes are stored **full-width** (no truncation/masking).

## Sections

- **Keys** - `key_width`-byte unsigned integers, **sorted strictly ascending**,
  `entry_count` of them. Compared as integers (native LE), enabling binary search
  directly over the mmap. A lookup miss is decided here and never touches the arena.
- **Offsets** - `offset_width`-byte positions into the *raw* (decompressed) arena,
  one per entry, parallel to the keys. Arbitrary order; entries may overlap
  (identical paths are shared).
- **Lengths** - u16 path byte-lengths, one per entry, parallel to the keys. Paths
  are capped at 65 535 bytes (the writer rejects longer ones).
- **Arena** - concatenated UTF-8 path strings, **no separators** (offset + length
  fully delimit every string). `flags.arena_compressed = 0` → raw bytes; `= 1` → a
  Zstandard Seekable Format stream whose decompressed content is the raw arena (its
  seek table lives inside the stream; the whole arena is also a valid ordinary zstd
  stream).

Sections are contiguous except for zero padding between keys and offsets when
`offsets_offset` needs realignment (only possible for a u32-key table with u64 offsets).

### Arena ordering

Readers must treat the arena as opaque bytes addressed by (offset, length). The
reference writer sorts the arena **lexicographically by path**, not by key: keys are
hashes, so key order scatters similar paths across the arena, while path order packs
a directory into the same compression frames. Measured on the real CDragon game
table this compresses ~4× better than key order (see `docs/BENCHMARKS.md`) and makes
directory-local batch lookups touch fewer frames. It also lets identical paths under
different keys share one arena extent.

## Lookup

```
get(hash):
    i = binary_search(keys, hash)        # integer comparison over the mmap
    if not found: return None            # miss: zero arena access
    start, len = offsets[i], lengths[i]
    if start + len > arena_decompressed_size: return None   # corrupt file
    raw:        return arena[start .. start+len]            # borrowed, zero-copy
    compressed: seek to `start` by decompressed offset,     # one ~frame decompressed
                read len bytes
```

## Validation (the file is downloaded, i.e. untrusted)

On open, before trusting any offset:

- magic, `version == 1`, `key_width ∈ {4,8}`, `offset_width ∈ {4,8}`,
  known `flags` bits only, known `hash_kind`;
- for raw arenas, `arena_compressed_size == arena_decompressed_size`;
- for compressed arenas: the trailing seek table parses, its total decompressed size
  equals `arena_decompressed_size`, and no frame's decompressed size exceeds the
  seekable-format maximum (1 GiB);
- all section extents in bounds (overflow-checked).

Per-entry extents are **not** validated at open (that would touch every offsets/lengths
page); instead every read bounds-checks its own extent against
`arena_decompressed_size`, and an out-of-bounds entry reports a miss. Frame *contents*
stay untrusted after open too: every frame extent and decompressed size is re-checked
when the frame is read, a lookup whose frame fails to decompress reports a miss, and
invalid UTF-8 is replaced lossily rather than erroring.

`verify()` additionally checks the xxh3 checksum, strict key ordering, and that every
entry's extent is in bounds and valid UTF-8 in the (decompressed) arena - it is opt-in
so `open` stays lazy (the shared-cache manifest carries a sha256 checked at download
time).
