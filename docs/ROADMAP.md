# Roadmap

> Planned work on the format, the reader, and the shared cache, in dependency order.
> Findings are lettered (A/B/C) from a consumer-integration review; tasks are numbered by wave.

Everything here is `ltk_hashdb`, `ltk_mimir_cache`, or `ltk_mimir_cli`. The waves are
ordered by what each unblocks and by three constraints that are not negotiable - read
those first, because two of them decide what may ship at all.

## Ordering constraints

### 1. `version` and `schema` are equality gates, so everything is additive from here

`Header::decode` rejects any `version != FORMAT_VERSION` (`header.rs:114`), and
`Manifest::from_slice` rejects any `schema != SCHEMA_VERSION` (`manifest.rs:76`). Both
compare for equality, not for a floor.

The shared cache exists so that several independently-versioned tools point at one
directory. That premise breaks the moment a version is bumped: the first tool to sync
publishes files and a manifest that every not-yet-upgraded tool on the machine refuses.
The manifest gate is the sharper of the two - it is not confined to reading a table, so a
consumer that merely reports cache status fails outright rather than degrading.

**`schema` stays at `1` and `FORMAT_VERSION` stays at `1`.** Task 3.1 relaxes the gates
for future readers; it cannot help the ones already shipped, which is the argument for
never bumping rather than for bumping once more first.

### 2. The additive room is in the reserved fields, not in the flag bits

| Room | Where | Status |
|------|-------|--------|
| Header reserved `[u8;8]` | bytes 72..80 (`header.rs:20`) | **Safe.** Written as zero, ignored on read - wide enough for a section offset. |
| Header reserved `[u8;2]` | bytes 14..16 (`header.rs:12`) | **Safe**, but only 2 bytes - a discriminant or a small count, not an offset. |
| New manifest fields | `manifest.rs` | **Safe.** serde ignores unknown fields; existing optional fields already use `#[serde(default)]`. |
| New `flags` bits | `header.rs:38`, checked at `header.rs:120` | **Not safe.** `KNOWN_FLAGS` rejects any unknown bit, and `FORMAT.md` specifies "other bits must be 0". |

So a new *feature* can be announced through a reserved header field without a version
bump, but a new *flag* cannot. Task 3.1 opens the flag byte; task 4.1 is written to need
only the reserved `u64`, so it does not depend on that half landing first.

### 3. Breaking Rust API changes are free until the first crates.io release

`ltk_hashdb` and `ltk_mimir_cache` sit at `0.1.0` with `release = true, publish = true` in
`release-plz.toml`, but no crate tag exists yet (`git tag -l` shows only the
`hashes-*` table-data namespace) and neither crate has a changelog. There are no external
consumers pinned to a published version.

Every breaking signature change is therefore one rev bump in one downstream repo today,
and a semver event forever after. **Wave 2 exists to spend that window before the first
`cargo publish`.**

## Wave 1 - Additive reader work

Purely additive: no signature changes, so this wave can ship to existing git-pinned
consumers without touching a call site.

### 1.1 Move `HashDb`'s state behind an `Arc` and add a frame cache

> Findings **A1** and **B4**. One refactor, not two - the shared read-only state and the
> shared frame cache want the same `Arc<Inner>`.

`FrameCache` (`reader.rs:52`) is a local `&mut` threaded through one call. It lives for
the duration of a `get_batch` (`reader.rs:154`), an `iter` (`reader.rs:230`), or a
`verify` (`reader.rs:249`), and dies when that call returns. `get` (`reader.rs:143`)
passes `&mut None`. Nothing caches a decompressed frame across calls.

The cost is the gap the benchmarks already record: on `game` a point hit is 8.5 µs against
3.6 µs batched and 143 ns for a miss; on `binentries` it is 10.5 µs against 1.2 µs. That
whole difference is frame decompression, and it is re-paid on every lookup even when the
previous one decompressed the very frame the answer is in. Because the arena is
path-ordered, consecutive lookups in a real workload are usually in the same frame - so a
consumer resolving an archive's chunks one at a time decompresses a handful of frames tens
of thousands of times.

Point lookups are not a niche shape. `ltk_wad`'s `PathResolver` is point-shaped by
construction, so every WAD extractor resolves one hash per chunk whether it wants to or
not.

- `HashDb { inner: Arc<Inner> }`, deriving `Clone`; the mmap, header, section ranges, and
  seek table move into `Inner`.
- A byte-capped LRU of decompressed frames beside them - a few MiB by default,
  `with_frame_cache(bytes)` to tune, `0` to disable. Published files are immutable, so this
  is pure memoisation with no invalidation.
- `HashStore::open_shared(table)` keeping a weak cache keyed on the manifest's active
  filename, so reopening after an update happens by itself. `open` (`store.rs:106`) today
  re-mmaps and re-parses the seek table on every call - ~10 400 frame records for `game`.

Design constraints: a frame held behind a `Mutex` cannot be borrowed out of the guard, so
`get` keeps returning `Cow::Owned` and the allocation only goes away with task 1.2.
Measure lock contention before choosing between one mutex, sharding by frame index, and a
thread-local cache.

**Done when** a bench resolving one archive's chunks point-wise lands within a small factor
of the same set batched; the `misses_never_decompress` invariant still holds; and
`Clone + Send + Sync` is asserted for `HashDb` *and* `LayeredHashDb`.

### 1.2 Closure and buffer accessors for allocation-free reads

> Finding **A3**. Depends on 1.1.

For a compressed arena - every published `.lhdb` - `str_at_cached` (`reader.rs:348`) always
takes the owned branch, because there is no borrow to be had from a decompression buffer
that dies with the call. `BENCHMARKS.md` already names the frame buffer as the only
per-lookup allocation. A consumer building a whole-install index therefore allocates a
`String` per chunk and keeps a fraction of them.

- `with_str<R>(&self, hash, f: impl FnOnce(&str) -> R) -> Option<R>`, reading straight out
  of the cached frame.
- `get_into(&self, hash, &mut String) -> bool` for callers that own a reusable buffer.
- Document that `with_str`'s closure runs under the cache lock and must stay short.

**Done when** a filter-only pass over a WAD's chunks - resolve, test the extension, discard
- allocates nothing per hit.

### 1.3 `Debug` on every public reader

> Finding **B3**.

None of `HashDb`, `LayeredHashDb` (`layered.rs:36` derives `Default` only), or
`ExtendedHashDb` (`extended.rs:10`) implements `Debug`. Consumers wrapping them end up
hand-writing `Debug` for their own types to compensate. Print counts, widths, and flags -
never entries.

`tests/roundtrip.rs:48` asserts `Send + Sync` for `HashDb` and `ExtendedHashDb` but not for
`LayeredHashDb`, which is the type most consumers actually hold. Add it.

**Done when** all three print their shape and the assertion covers all three.

### 1.4 Tell a corrupt table apart from a table of unknown hashes

> Finding **A4**.

`str_at_cached` (`reader.rs:348`) swallows every `frame_bytes` (`reader.rs:366`) error into
`None`. That is right for a lookup path - `FORMAT.md` specifies a failed frame as a miss -
but it means a table whose arena no longer decompresses degrades silently into "this build
knows nothing". A consumer renders an entire install under hex names with no signal to
distinguish that from an incomplete table.

Nothing re-verifies an installed table after its download checksum (`store.rs:106`: the
manifest sha256 is trusted), so bit rot and truncating writes both land here.

- A sticky `AtomicBool` set on the first swallowed error, plus `HashDb::is_healthy()`.
- Optionally `try_get -> Result<Option<Cow>, VerifyError>` for callers that want the error
  at the call site.

**Done when** a table with a deliberately corrupted frame reports unhealthy after the first
affected lookup, and `get` still never panics.

### 1.5 Write down why the mmap is sound

> Finding **A5**.

`unsafe { memmap2::Mmap::map(&file)? }` (`reader.rs:58`) carries no safety comment. What
discharges it - published `.lhdb` files are immutable, `commit` never renames over an
existing name, and `gc` only unlinks - is documented on `HashStore` in a different crate.
`CONSUMERS.md` explicitly invites using `ltk_hashdb` alone for embedded or self-built
tables, and such a consumer gets no warning that truncating a mapped file is undefined
behaviour rather than an error.

Add a `// SAFETY:` at the block and a paragraph in `HashDb::open`'s docs stating the
caller's obligation.

## Wave 2 - Breaking API cleanup

Every breaking signature change worth making, batched into one release. See constraint 3.

### 2.1 Give `Table` its own metadata

> Finding **B2**. Blocks 2.2.

`Table` (`lib.rs:38`) exposes `id()` (`lib.rs:63`) and `from_id()` (`lib.rs:77`) and nothing
else. It cannot report its key width, hash algorithm, or casing, though those are fixed per
table and the workspace knows them - so they are written out three times: the CLI's private
`Table` enum (`ltk_mimir_cli/src/main.rs:32`, methods at `:43`-`:64`), `bundle.rs`'s
`SPECS` (`ltk_mimir_cli/src/bundle.rs:76`), and prose in consumers explaining which tables
share a hash universe.

- `key_width()`, `hash_kind()`, `casing()`, and a `universe()` discriminator on the library
  enum.
- Delete the CLI's duplicate methods and fold `TableSpec` (`bundle.rs:49`) down to input
  filename plus split flag.

**Done when** each table's width, algorithm, and casing is stated once in the workspace.

### 2.2 Make the layered key-config invariant a hard error

> Finding **B1**. Depends on 2.1.

`LayeredHashDb` requires every base to agree on `(key_width, hash_kind, casing)`, because
lookups take a pre-hashed `u64` and no base re-hashes. It documents this at length and then
enforces it with `debug_assert_eq!` (`layered.rs:57` in `from_bases`, `layered.rs:75` in
`push_base`) - which compiles out of release builds.

In release, layering `binfields` under `binentries` does not fail. It answers a property
hash with an object's path, across four unrelated 32-bit FNV-1a universes totalling
~500 000 rows, often enough to be a certainty rather than a risk. A wrong name is worse
than a number.

- `push_base` and `from_bases` return `Result<_, KeyConfigMismatch>` naming both configs.
- `open_layered` (`store.rs:124`) refuses a mismatched set using `Table::universe()` before
  it opens anything.

**Done when** layering `binfields` under `binentries` fails in a release build.

### 2.3 `#[non_exhaustive]`, `Display`, and `FromStr` on `Table`

> Finding **B7**.

The table set will grow, and without `#[non_exhaustive]` each addition is a breaking
change. `Display`/`FromStr` over `id()` remove `table.id().to_owned()` from every consumer
boundary. `OpenError::TableNotFound` formats with `{0:?}`, so it prints `BinEntries` where
every other surface says `binentries`. An optional serde feature belongs here too.

### 2.4 Split `Casing` so the League rule has its own name

> Finding **A6**. Narrows a documented decision; does not reverse it.

`FORMAT.md` already specifies Unicode-aware lowercasing deliberately, and already carries
the stability note that only the ASCII part of the mapping is bit-stable across toolchains,
recommending that non-ASCII publishers pre-lowercase and hash case-sensitively. The gap is
not that the decision is wrong - it is that `Casing::Insensitive` (`hash.rs:18`, variant at
`hash.rs:27`) is one name covering two rules, and the League tables, which are the
overwhelmingly common case, get the Unicode one by default.

Introduce `Casing::AsciiInsensitive` as what League tables mean and what
`FLAG_CASE_INSENSITIVE` maps to for them; keep the Unicode variant as a deliberate,
separately-named choice or drop it (see *Open questions*). No data migration - every
published table is ASCII, so the bytes on disk do not change, only which variant names
them. `FORMAT.md`'s `case_insensitive` section needs updating either way.

### 2.5 Signature parity and a streaming batch

> Finding **B5**.

`HashDb::get_batch` (`reader.rs:154`) collects eagerly so the returned iterator does not
borrow the input. `LayeredHashDb::get_batch` (`layered.rs:126`) keeps the borrow, because
its tail zips over the input slice - so the same method on the two types has two contracts,
and the layered one refuses a temporary.

- Drop the `'a` from the layered signature's `hashes`.
- Add `for_each_batch(&self, hashes, f: impl FnMut(usize, u64, Option<&str>))` to both,
  which inherits task 1.2's allocation-free path. Both current forms materialise a `Vec` of
  every result before yielding the first.

### 2.6 Retire `ExtendedHashDb`, grow `LayeredHashDb`

> Finding **B6**.

`layered.rs` already describes `LayeredHashDb` as `ExtendedHashDb` generalised to N bases,
and it has everything the narrow type has plus `get_batch`. Deprecate `ExtendedHashDb`,
lead `CONSUMERS.md` with the layered type rather than teaching the narrower one first, and
add `iter()` and `len()` to `LayeredHashDb` - today it cannot be enumerated at all.

## Wave 3 - Distribution

Task 3.1 gates the rest of this wave *and* the flag-byte half of wave 4. Nothing here may
add a manifest field or a header flag until the version gates stop being fatal.

### 3.1 Make the version gates survivable

> Finding **A7**. Blocks 3.2, 3.3, and the flag-byte option in 4.1.

- **Manifest.** Accept `schema >= SCHEMA_VERSION` (`manifest.rs:76`) and rely on serde
  ignoring unknown fields. Add an optional `min_reader_schema` for the day something
  genuinely incompatible is needed. Policy: `schema` stays at `1`.
- **Header.** Split `flags` into a required byte - unknown bit rejects, today's behaviour
  at `header.rs:120` - and an optional byte whose unknown bits are ignored. Update
  `FORMAT.md`, which currently specifies "other bits must be 0".
- **Manifest.** `format_version` per `TableEntry` (`manifest.rs:50`), so a reader skips a
  table it cannot open the way `UpdateReport::unknown_tables` already skips an unknown id.
- **Release.** Publish a per-format channel filename alongside `latest`, so an old build
  keeps updating within the format it can read. `ReleaseSource::github` hardcodes
  `/releases/latest/download`.

**Done when** a manifest carrying an unknown table, an unknown field, and a higher `schema`
still installs the tables this build understands, and a `.hashdb` with an unknown
*optional* flag bit opens normally.

### 3.2 Version and time on `TableEntry`

> Finding **C6**. Depends on 3.1.

`TableEntry` (`manifest.rs:50`) carries `file`, `sha256`, `entries`, and `key_width` - no
version and no timestamp. The version exists only inside the filename, extracted by
`version_of`, which is private (`update.rs:345`). `generated_at` (`manifest.rs:23`) is a
`String`, so "how stale is this cache" is every consumer's parsing problem despite the
crate already depending on `time`.

Add `version`; expose `generated_at` parsed beside the raw string.

**Done when** a consumer can show `game · 2026-07-10` instead of `game-2026-07-10.lhdb`.

### 3.3 Move provenance onto the table

> Finding **C5**. Depends on 3.1.

`commit` (`store.rs:155`) assigns `manifest.source = source` wholesale (`store.rs:169`), so
a run installing only `game` restamps `repo`, `commit`, and `inputs_sha256` for the seven
tables it did not touch - the manifest then claims a CommunityDragon commit for tables
built from a different one.

Move `commit` and `inputs_sha256` onto `TableEntry`; keep a manifest-level record named for
what it actually is, the last run.

### 3.4 Add `HashStore::check`

> Finding **C1**.

`update` (`update.rs:161`) is the only path to the remote manifest, and it takes the
exclusive lock before fetching anything. There is no lock-free "what would change?", so a
UI cannot show an updates-available state and a startup check cannot stay out of the CLI's
way.

Expose the first half of the private `plan` (`update.rs:226`) as
`check(&remote) -> Result<Vec<TableDiff>, _>`: fetch, compare sha256s, install nothing,
take no lock.

**Done when** a consumer can render "3 tables behind" without touching the update lock.

### 3.5 Stream the download and stop copying the result

> Findings **C3** and **C2**. One change - the copy only exists because the fetcher hands
> back a whole buffer.

`Fetch` (`update.rs:29`) and `AsyncFetch` (`update.rs:66`) return `Vec<u8>`. For each table
the bytes then move four times: `verify_and_stage` (`update.rs:269`) runs `sha256_bytes`
(`fsutil.rs:46`) over the buffer and writes a `.download.tmp`; `commit` opens that file,
`atomic_copy` (`fsutil.rs:36`) reads and writes it again into a second temp, renames, and
`sha256_file` (`fsutil.rs:51`) reads the destination a third time to recompute a digest it
was just handed.

A ~52 MiB corpus therefore costs several hundred MiB of I/O and peaks at the size of the
largest table in memory (38.3 MiB for `game`). The buffer return also makes byte-level
progress and mid-download cancellation impossible, which is why the bundled fetchers are
documented as silent.

- `Fetch::fetch_to(&self, filename, sink: &mut dyn Write)` as the primitive, with the
  `Vec` form kept as a default method over it. Same for `AsyncFetch`.
- Stage straight into the cache directory while hashing the stream.
- A `CommitItem` constructor that *takes* an already-staged sibling - rename, reuse the
  digest - alongside the existing copy path for files built elsewhere.

**Done when** installing the full corpus reads and writes each table's bytes once, peak
memory does not track the largest table, and a caller's sink can report bytes and cancel.

### 3.6 Say who holds the update lock

> Finding **C4**.

`UpdateLock::try_acquire` (`lock.rs:20`) is non-blocking and reports only presence - no
holder, no start time, no bounded wait. Consumers can only surface "another process is
already syncing", which reads the same as a crashed updater.

Write pid and an RFC-3339 start time into the lock file's body, and expose `lock_holder()`
plus `lock_update_timeout(Duration)`. The OS lock stays the source of truth; the body
exists only so a message can name who and since when.

### 3.7 A cheap integrity tier between `open` and `verify`

> Finding **C7**. Pairs with 1.4.

`open` validates structure only; `verify` (`reader.rs:249`) reads the whole file. Add
`verify_index()` - checksum the keys, offsets, and lengths sections and skip the arena.
Milliseconds rather than a full pass, and it catches most real post-install damage.
Together with 1.4's health flag a consumer gets both a proactive check and a reactive
signal.

## Wave 4 - Format capability

### 4.1 Record arena order in the file

> Finding **A2**. Depends on 3.1 only if the flag-byte variant is chosen; the reserved-`u64`
> variant below needs nothing.

The writer sorts the arena lexicographically by path (`writer.rs:84`-`93`) - that ordering
is what earns the ~4× compression win. But offsets and lengths are stored in *key* order,
so nothing can walk the arena forward without first reconstructing the permutation.
`arena_order` (`reader.rs:301`) does that with an `O(n log n)` sort over an `n`-word
allocation, on every `iter` and every `verify`: on `game` that is a 2 086 643-element sort
and ~16 MiB of scratch, recomputed per call. `BENCHMARKS.md` already lists the vector under
*Memory profile*.

The larger cost is what it forecloses. A table that is already a sorted list of strings
cannot answer "which paths start with `assets/characters/ahri/`" - the query an asset
browser and a name-autocomplete both want. Consumers work around it by downloading the
CommunityDragon txt list separately just to enumerate names.

Two candidate layouts:

| Layout | Content | Size on `game` |
|--------|---------|---------------:|
| **Sparse per-frame index** | Per zstd frame: entry index and arena offset of the first entry starting in it | ~10 400 frames × 12 B ≈ **122 KiB** |
| Dense rank array | `arena_rank → entry_index`, `u32 × entry_count` | ~8 MiB (+21 % on a 38.3 MiB file) |

Point the reader at the section from the reserved header `u64` at bytes 72..80
(`header.rs:20`), which current readers already ignore - so no version bump, no flag bit,
and a reader built before this change still opens the new file.

Then: `values()` streaming with no sort and no allocation, `prefix(&str, limit)` as a
binary search over the arena, and `iter`/`verify` stop rebuilding the permutation.

`FORMAT.md` needs a new section; its "readers must treat the arena as opaque bytes
addressed by (offset, length)" invariant is unaffected, since this adds an index rather
than changing how an entry is addressed.

**Done when** `iter` on the game table allocates no index vector,
`prefix("assets/characters/ahri/")` returns in single-digit milliseconds, and a reader
built before the change still opens the file. Decide between the two layouts against a
measured prefix benchmark, not on paper.

## Open questions

- **Does `ExtendedHashDb` get deleted or deprecated forever?** Deprecation costs nothing
  but keeps two overlapping types in the docs; deletion is free right now (constraint 3)
  and a semver event later.
- **Does the Unicode `Casing` variant survive task 2.4?** Nothing in the League corpus
  needs it, `FORMAT.md` already tells non-ASCII publishers to pre-lowercase instead, and
  keeping it means keeping a way to hash a path to a key no published table holds. Dropping
  it would simplify `hash.rs` and remove the Unicode-version stability caveat from the spec.
- **Should mimir ship a `PathResolver` impl for `ltk_wad`?** Every WAD consumer writes the
  same ~40-line adapter. The dependency direction is the awkward part: `ltk_wad`
  implementing the trait for `LayeredHashDb` is cleaner than mimir depending on `ltk_wad`,
  but it puts the impl in a crate that would then need an `ltk_hashdb` dependency. A
  feature-gated impl here is the third option.
