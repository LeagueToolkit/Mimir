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
├─ optional ───────────────────────────────────┤
│ Arena order (entry_count × width + 8 bytes)  │
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
| 10 | `hash_kind` | `u8` | algorithm that produced the keys, see below |
| 11 | `flags` | `u8` | **required** flags: bit0 `arena_compressed`, bit1 `case_insensitive` (see below); any other bit set **must** reject the file |
| 12 | `key_width` | `u8` | 4 = u32 table, 8 = u64 table |
| 13 | `offset_width` | `u8` | 4 or 8; the writer picks 4 while `arena_decompressed_size` fits in a u32, else 8; the reader honors whatever is declared |
| 14 | `opt_flags` | `u8` | **optional** flags: none defined yet; an unrecognized bit **must** be ignored |
| 15 | `arena_order_width` | `u8` | bytes per entry in the arena-order section, `1..=8`; `0` when there is none |
| 16 | `entry_count` | `u64` | |
| 24 | `keys_offset` | `u64` | file offset of the keys section; writers must 8-align it (the reference writer emits 80), readers bounds-check and honor the declared value |
| 32 | `offsets_offset` | `u64` | file offset of the offsets section; writers must `offset_width`-align it |
| 40 | `arena_offset` | `u64` | file offset of the arena |
| 48 | `arena_decompressed_size` | `u64` | raw (decompressed) arena length |
| 56 | `arena_compressed_size` | `u64` | arena bytes on disk; == decompressed if raw |
| 64 | `checksum` | `u64` | xxh3-64 of keys ‖ offsets ‖ lengths ‖ arena, each as stored on disk (inter-section padding excluded) |
| 72 | `arena_order_offset` | `u64` | file offset of the arena-order section; `0` = the file carries none |

The lengths section has no header field: it sits immediately after the offsets, at
`offsets_offset + entry_count × offset_width` (u16 entries are always 2-aligned there).

### The two flag bytes

`version` is an equality gate - a reader rejects anything that is not `1` - so a
capability added later can only reach readers that already shipped if they are
told to ignore what they do not recognize. That is the whole difference between
the two flag bytes:

| | unknown bit means | use it for |
|---|---|---|
| `flags` (11) | reject the file | anything that changes how the bytes must be interpreted |
| `opt_flags` (14) | read the file as if the bit were clear | anything a reader may exploit but need not |

So an index that speeds up a scan is an `opt_flags` bit; a different arena
encoding is a `flags` bit. When in doubt, ask what a reader that ignores the bit
would produce: a correct answer more slowly, or a wrong one.

A capability that needs a field of its own does not also need a bit. The
arena-order section is announced by its own offset being non-zero - bytes 72..80
and byte 15 were reserved-and-zero in every build that shipped, so a reader
predating the section sees zeroes it already ignores, and there is no flag that
could disagree with the offset beside it.

### `hash_kind`

The algorithm alone - the casing rule is recorded separately in the
`case_insensitive` flag, so non-League consumers can use these algorithms over
case-preserved keys.

| Value | Name | Algorithm | Used by |
|-------|------|-----------|---------|
| 0 | unspecified | consumers fall back on key width: u64 → xxh64, u32 → fnv1a32 | - |
| 1 | xxh64 | XXH64, seed 0 | game, lcu, rst (xxh64 lists) |
| 2 | fnv1a32 | FNV-1a 32 | binentries, bintypes, binfields, binhashes |
| 3 | xxh3 | XXH3-64, no seed | rst (v5+, `.xxh3` lists) |

RST hashes are stored **full-width** (no truncation/masking).

### `case_insensitive` (flags bit1)

Whether the keys hash the **ASCII-lowercased** path (bit set) or the path bytes
as given (bit clear). All League tables set it - the game hashes lowercased
paths - so a consumer hashing a new path must lowercase it first to match.

The mapping is exactly `A-Z` → `a-z`, byte by byte. Every other byte is hashed
untouched, including every byte of a multi-byte UTF-8 sequence, so `É` and `é`
are different keys. This is a deliberate narrowing: a byte substitution has no
locale, no Unicode tables and no toolchain drift behind it, so a key computed
from a given path is the same key forever, and an implementation in any language
is a three-line loop. On League data - all ASCII - it coincides with `ltk_hash`'s
`WadHash`/`BinHash`.

A publisher whose paths are not ASCII should case-fold them however its domain
requires and then hash **case-sensitively**, which keeps the folding rule (and
its version) on the publisher's side of the file rather than the reader's. Should
a Unicode-aware rule ever be specified, it takes an `opt_flags` bit rather than a
second `flags` bit, so that builds predating it keep reading the table: they
resolve every ASCII path exactly as before and miss the rest, which beats
refusing the file outright.

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
- **Arena order** (optional) - `entry_count` entry indices, `arena_order_width`
  bytes each, listing the entries in the order their paths sit in the arena;
  followed by an xxh3-64 of those bytes. Present iff `arena_order_offset != 0`.

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

### Arena order

The arena is laid out by path while offsets and lengths are stored by key, so
walking the arena forward - or binary-searching it for a path prefix - means
knowing the permutation between the two orders. A reader can always recover it by
sorting the offsets; this section is that permutation written down.

```
arena_order[rank] = entry index, for rank in 0 .. entry_count
```

- Entries are `arena_order_width` little-endian bytes each, wide enough to hold
  `entry_count - 1`. The reference writer picks the narrowest such width (3 bytes
  for the ~2.3M-entry League `game` table); a reader honors whatever is declared
  and rejects a width too narrow to address the table.
- The `entry_count × width` bytes are followed by an **xxh3-64 of themselves**.
  The header's `checksum` deliberately does not cover this section: it is defined
  over keys‖offsets‖lengths‖arena, and a reader built before the section existed
  still computes it that way, so the section carries its own digest instead.
- The permutation must name every entry exactly once, and must run forward: for
  consecutive ranks, `(offset, length)` never decreases. That ordering is what
  makes an arena walk decompress each frame once, and is checked by `verify`.
- It records the order, not the *rule*. That the arena is sorted by path is the
  reference writer's layout (see *Arena ordering*) and what a prefix search
  depends on, but the format does not require it and a reader cannot check it
  cheaply.

**Adding it does not change any other byte.** The section is appended after the
arena and the two header fields it fills were reserved and zero, so a reader that
predates it opens the file, reads it, and checksums it exactly as before. It is
therefore optional in both directions: a writer may leave it out, and a reader
may ignore it and sort for the order instead. See `docs/BENCHMARKS.md` for what
each choice costs.

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
  known `flags` bits only (`opt_flags` is *not* checked - see above),
  known `hash_kind`;
- for raw arenas, `arena_compressed_size == arena_decompressed_size`;
- for compressed arenas: the trailing seek table parses, its total decompressed size
  equals `arena_decompressed_size`, and no frame's decompressed size exceeds the
  seekable-format maximum (1 GiB);
- all section extents in bounds (overflow-checked);
- if `arena_order_offset != 0`: `arena_order_width` is `1..=8` and wide enough for
  `entry_count`, and the section is in bounds. Its *contents* are not read at open -
  each rank is range-checked where it is used, and a rank naming an entry that does
  not exist reads as a miss.

Per-entry extents are **not** validated at open (that would touch every offsets/lengths
page); instead every read bounds-checks its own extent against
`arena_decompressed_size`, and an out-of-bounds entry reports a miss. Frame *contents*
stay untrusted after open too: every frame extent and decompressed size is re-checked
when the frame is read, a lookup whose frame fails to decompress reports a miss, and
invalid UTF-8 is replaced lossily rather than erroring.

`verify()` additionally checks the xxh3 checksum, strict key ordering, the arena-order
section (its own digest, that it is a permutation, and that it runs forward), and that
every entry's extent is in bounds and valid UTF-8 in the (decompressed) arena - it is opt-in
so `open` stays lazy (the shared-cache manifest carries a sha256 checked at download
time).
