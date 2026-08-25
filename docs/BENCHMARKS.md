# Benchmarks

Real-data measurements behind the format's size/latency claims, the **path-ordered
arena**, the **16 KiB default frame size**, and the **level 19** publishing default.

- Data: CommunityDragon `hashes.*.txt` snapshot of **2026-07-07** (~272 MB of txt,
  ~2.97 M entries across 8 tables).
- Machine: Windows 11, x86-64, NVMe; single thread; release build. Page cache warm -
  numbers measure the format, not the disk.
- Reproduce:

  ```text
  cargo run --release -p ltk_hashdb --example bench_real -- data/cdragon data/build
  cargo run --release -p ltk_hashdb --example compression_lab   # ordering/level/dict study
  cargo run --release -p ltk_hashdb --example arena_order -- game.lhdb out.lhdb
  cargo run --release -p ltk_hashdb --example path_coding -- game.lhdb
  cargo bench -p ltk_hashdb            # criterion, uses the files built above
  MIMIR_CDRAGON_DIR=data/cdragon cargo test --release --test golden
  ```

  `bench_real` also writes its measurements to `<out_dir>/bench_real.json`; the
  performance charts in `DESIGN.md` (`docs/assets/bench-*.svg`) regenerate from that report
  via `cargo run -p ltk_hashdb --example gen_charts`. The arena-layout chart is the
  exception - its numbers come from `compression_lab` and live in `gen_charts.rs`.

## Table sizes (path-ordered arena, 16 KiB frames, level 19)

| table | entries | txt | raw `.hashdb` | zstd `.hashdb` | vs txt | build |
|-------|--------:|----:|-------------:|--------------:|-------:|------:|
| game | 2,086,643 | 198.3 MiB | 190.4 MiB | 38.3 MiB | 19.3% | 71.8s |
| lcu | 150,538 | 16.1 MiB | 15.5 MiB | 2.7 MiB | 16.9% | 5.8s |
| binentries | 377,224 | 27.9 MiB | 27.9 MiB | 5.5 MiB | 19.6% | 9.3s |
| binfields | 9,400 | 237 KiB | 237 KiB | 143 KiB | 60.3% | 0.0s |
| binhashes | 109,515 | 4.8 MiB | 4.8 MiB | 1.6 MiB | 33.4% | 1.2s |
| bintypes | 3,587 | 115 KiB | 115 KiB | 55 KiB | 48.0% | 0.0s |
| rst.xxh64 | 108,709 | 5.7 MiB | 5.3 MiB | 1.8 MiB | 31.2% | 1.0s |
| rst.xxh3 | 129,647 | 6.8 MiB | 6.4 MiB | 2.1 MiB | 31.3% | 1.2s |

**Everything: ~253 MiB of txt → ~52 MiB of ready-to-mmap `.hashdb`** (~21 %), with no
parse/expand step on the consumer - `open` is a header validation plus an mmap, and
open + first lookup is sub-millisecond. Of the 38.3 MiB game file, ~29 MiB is the
uncompressed keys + offsets + lengths sections (the binary-searchable part); the
162 MiB of path strings compress to ~9 MiB.

Build time is a publisher cost only (level 19; decompression speed is
level-independent). `--level 3` builds the game table in ~1 s and costs about
2 MiB of arena (12.3 vs 10.4 MiB in the lab table below).

## Why the arena is sorted by path (`examples/compression_lab.rs`)

Measured on the game arena (162.5 MiB of strings, 16 KiB frames):

| variant | level | arena size | ratio | frame decompress |
|---------|------:|-----------:|------:|-----------------:|
| key-order arena | 3 | 50.8 MiB | 31.3% | 17.5 µs |
| key-order arena | 19 | 45.7 MiB | 28.1% | 15.3 µs |
| key-order + trained 112 KiB dictionary | 19 | 30.0 MiB | 18.4% | 13.8 µs |
| **path-order arena** | **3** | **12.3 MiB** | **7.6%** | **6.6 µs** |
| path-order arena | 19 | 10.4 MiB | 6.4% | 6.4 µs |
| path-order + trained dictionary | 3 | 12.0 MiB | 7.4% | 6.6 µs |
| solid stream (no random access) | 19 | 17.5 MiB | 10.8% | - |

Keys are hashes, so a key-ordered arena scatters similar paths across frames; sorting
lexicographically packs a directory into the same frames and compresses ~4× better -
beating even a solid level-19 stream - while making hits faster (smaller compressed
frames) and directory-local batches frame-coherent. A trained zstd dictionary is worth
~40 % on a key-ordered arena but nothing on top of path order, so the format has no
dictionary machinery. This is why v1 stores per-entry offsets + u16 lengths instead of
monotonic offsets.

## Frame-size sweep (level 19)

Per-lookup figures are averages over 20 k random existing keys (`hit`), the same
sample resolved through `get_batch` (`batch hit`), and 1 M random probes (`miss`).
`full iter` walks every entry in arena order.

### `game` (2,086,643 entries)

| frame | file | hit | batch hit | miss | full iter |
|-------|-----:|----:|----------:|-----:|----------:|
| raw | 190.4 MiB | 1.7 µs | 417 ns | 143 ns | 397 ms |
| 4 KiB | 42.3 MiB | 4.9 µs | 3.2 µs | 146 ns | 753 ms |
| 8 KiB | 39.8 MiB | 6.0 µs | 3.5 µs | 139 ns | 625 ms |
| **16 KiB** | **38.3 MiB** | **8.5 µs** | **3.6 µs** | **143 ns** | **610 ms** |
| 32 KiB | 37.4 MiB | 13.6 µs | 3.6 µs | 140 ns | 580 ms |
| 64 KiB | 36.8 MiB | 27.0 µs | 3.5 µs | 150 ns | 1576 ms |
| 128 KiB | 36.4 MiB | 47.7 µs | 3.4 µs | 153 ns | 1534 ms |
| 256 KiB | 36.0 MiB | 94.7 µs | 3.7 µs | 147 ns | 605 ms |

### `binentries` (377,224 entries)

| frame | file | hit | batch hit | miss | full iter |
|-------|-----:|----:|----------:|-----:|----------:|
| raw | 27.9 MiB | 674 ns | 233 ns | 86 ns | 24 ms |
| 4 KiB | 6.3 MiB | 5.3 µs | 1.7 µs | 88 ns | 100 ms |
| 8 KiB | 5.8 MiB | 6.8 µs | 1.2 µs | 86 ns | 80 ms |
| **16 KiB** | **5.5 MiB** | **10.5 µs** | **1.2 µs** | **89 ns** | **72 ms** |
| 32 KiB | 5.3 MiB | 16.3 µs | 1.0 µs | 87 ns | 60 ms |
| 64 KiB | 5.1 MiB | 31.4 µs | 844 ns | 93 ns | 59 ms |
| 128 KiB | 5.1 MiB | 55.2 µs | 879 ns | 83 ns | 59 ms |
| 256 KiB | 5.0 MiB | 109.8 µs | 914 ns | 91 ns | 61 ms |

### Reading the curve

- **Misses never touch the arena**: ~90–150 ns at every frame size, raw or
  compressed. This is the format's core promise - hash hunting hammers misses.
- **Point-hit latency is pure frame decompression** and scales with frame size,
  while the file shrinks only ~2–4 % per doubling past 16 KiB.
- **Batched lookups and iteration are insensitive to frame size** (the reader's
  frame-run cache decompresses each frame once), so bulk consumers don't care.
- **16 KiB stays the knee** after path ordering: past it the size gain collapses
  (38.3 → 36.0 MiB for 16× larger frames) while hits keep doubling.

The defaults are writer-side choices only - the format records the seek table, so
any frame size and level stay readable by every reader.

## Arena order: storing the permutation vs sorting for it

The arena is laid out by path and the offsets are stored by key, so anything that
walks the arena forward - `iter`, `values`, `prefix`, `verify` - needs the
permutation between the two. It can be sorted for, or read from the optional
arena-order section. Measured against the published `game` table (2,291,347
entries, 44,114,614 B) with `--example arena_order`, each phase against a freshly
opened table:

| | sorted for | stored in the file |
|---|---:|---:|
| file size | 44,114,614 B | 50,988,663 B (**+15.6 %**) |
| first arena-order call | 516 ms | 11 ms |
| `prefix`, 13,511 hits, warm | 1.4 ms | 1.6 ms |
| `prefix`, 499 hits, warm | 0.3 ms | 0.4 ms |
| `prefix`, no match, warm | 0.1 ms | 0.2 ms |
| `values()`, whole table | 776 ms | 331 ms |

So the section buys one thing: it removes a **~0.5 s sort** the first time a
process walks the arena, plus the ~18 MB of scratch that sort needs and the
6.9 MB it leaves behind per process. Warm, the two are the same code on the same
bytes and measure the same. Across all eight published tables the section would
cost **+16.5 %** (61.1 MiB → 71.3 MiB), paid by every consumer including the ones
that only ever call `get`.

That is why [`ArenaOrder::Omitted`] is the default and the published tables do not
carry it: a prefix search is already single-digit milliseconds without it, and a
half-second once per process is a smaller price than a sixth of the download for
everyone. `mimir build --arena-order` is there for consumers who would rather pay
the bytes - a long-running asset browser opening the table on every launch, say.

[`ArenaOrder::Omitted`]: https://docs.rs/ltk_hashdb/latest/ltk_hashdb/enum.ArenaOrder.html

## Would factoring the paths out be smaller?

The 187 MB of paths in the `game` arena are enormously redundant - 2.29 M paths
share only 78,792 directories - so it is worth asking whether storing each
directory once beats leaving the redundancy to zstd. Measured with
`--example path_coding`, all at level 19 against the same 44,114,614 B file:

| arena encoding | on disk | paths per frame |
|---|---:|---:|
| **raw** (what ships) | 12,035,676 B | 199 |
| front-coded | 7,274,750 B | 1,045 |
| front-coded, at raw's frame granularity | 9,380,187 B | 199 |
| raw, at front coding's frame granularity | 10,083,790 B | 1,045 |
| interned: directory table + `(dir_id, filename)` | **16,728,132 B** | 199 |

Two results.

**Interning - each directory stored once, an id per entry - makes the file
bigger**, by 39 % of the arena. The directory strings do compress away, but the
`dir_id` array is 3 bytes per entry of *index*: read at random, so never
compressed, and it costs more than the redundancy it removes. That is the general
shape of the trap. The arena is only **27 % of the file** and compresses 15.6x;
the other 73 % - keys, offsets, lengths - is random-access and stored raw. Any
scheme that moves bytes from the arena into the index trades compressible bytes
for incompressible ones.

**Front coding is worth about 2.7 MB, not the 4.8 MB it first appears.** Comparing
the top two rows is unfair: the same 16 KiB frame holds 5x more front-coded paths,
so half the gain is a coarser lookup granularity that raw can also buy - and does,
in row four - by raising `--frame-size` at the cost of hit latency. At equal paths
per frame it is a real ~22 % off the arena, i.e. ~6 % off the file.

That 6 % is not additive, though. A front-coded arena cannot be addressed by
`(offset, length)`, so it needs a restart per frame, a replay to read any entry,
and a `flags` bit - which is the one kind of change [FORMAT.md](FORMAT.md)'s
compatibility rule rules out, because every reader already shipped would reject
the file.

## Memory profile

- `open` maps the file: no allocation proportional to table size; the OS pages in
  what lookups touch (keys for misses, plus one decompressed frame per hit -
  the frame buffer is the only per-lookup allocation).
- `iter`/`values`/`prefix` need the arena order: a table carrying the section
  reads it out of the mapping and allocates nothing, and one without it sorts
  once and keeps the result (1-8 bytes/entry, 3 on `game`) for as long as a handle
  to the table is open - shared by every clone, not rebuilt per call.
- `load_all` (opt-in) then owns everything (~arena size + `HashMap` overhead) for
  the expand-once profile.

## Correctness vs the txt corpus (golden test)

`tests/golden.rs` (gated on `MIMIR_CDRAGON_DIR`) builds every table raw **and**
compressed from the snapshot and checks entry-for-entry parity: `len`, `verify`,
every key via `get_batch`, 20 k random point `get`s, and 100 k membership probes
per table. It also recomputes every key from its path - our xxh64 / fnv1a32 / xxh3
implementations reproduce the entire corpus except **3 anomalous upstream lines**
(1 in game, 2 in lcu) whose stored hash doesn't match their own path, confirmed
against an independent xxhash implementation.

Paths in the corpus are not always "clean": binhashes contains the empty string
(`811c9dc5` = FNV-1a offset basis) and strings with trailing spaces (`Seed `).
Parsers must strip only the line terminator - never `trim_end()`.
