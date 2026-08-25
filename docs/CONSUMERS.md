# Consumer integration guide

How to use mimir **as a library**. This is the primary interface: the `mimir` CLI is a
thin operational wrapper (CI publishing, quick inspection) and most consumers - WAD
unpackers, `.bin` inspectors, mod loaders, asset browsers - should never shell out to it.
The CLI is covered [at the end](#the-cli).

## Which crate do I need?

| You want to… | Depend on |
|---|---|
| Resolve hashes against the machine-shared League tables (the common case) | `ltk_mimir_cache` (+ `ltk_hashdb` for the `HashDb` type it hands back) |
| Keep that shared cache up to date from your app (no CLI) | `ltk_mimir_cache` - `HashStore::update` / `update_async` + your HTTP client |
| Open a specific `.hashdb`/`.lhdb` file, or bytes you embedded/downloaded yourself | `ltk_hashdb` |
| Build your own tables (the format is general-purpose: any `u64 → str` map) | `ltk_hashdb` (writer) |
| Brute-force unknown hashes back into paths | `ltk_mimir_gen` |

## Resolving hashes

### From the shared cache (the default path)

The shared cache is one directory of versioned `.lhdb` files plus a `manifest.json`
pointing at the active version of each table. Every mimir-backed tool on the machine
opens the same files, so the OS page cache holds **one** copy for all of them.

```rust
use ltk_mimir_cache::{HashStore, Table};

let store = HashStore::discover()?;   // resolve the cache dir; touches no files
let db = store.open(Table::Game)?;    // manifest → active .lhdb → mmap + validate

if let Some(path) = db.get(0x1234_5678_9abc_def0) {
    println!("{path}");
}
```

- `discover()` uses `MIMIR_DIR` when set, otherwise the platform data dir
  (Windows `%LOCALAPPDATA%\LeagueToolkit\hashes`, Linux `$XDG_DATA_HOME/LeagueToolkit/hashes`,
  macOS `~/Library/Application Support/LeagueToolkit/hashes`). `HashStore::at(dir)` takes
  an explicit directory (tests, `--dir` flags).
- `open` is cheap and lazy: the header and section bounds are validated, but nothing is
  decompressed and no checksum is computed (the manifest's sha256 was checked at download
  time). Pages fault in as lookups touch them.
- If the cache has never been populated, `manifest()`/`open` fail with
  `ManifestError::Missing` - see [Getting and updating tables](#getting-and-updating-tables).

The logical tables are `Table::{Game, Lcu, BinEntries, BinTypes, BinFields, BinHashes,
Rst, RstXxh3}`; `Table::ALL` iterates them and `Table::id()`/`from_id()` map to the stable
string ids used in filenames and the manifest.

### From a file or bytes you manage yourself

If you distribute a table with your app (or in tests), skip the cache layer:

```rust
use ltk_hashdb::HashDb;

let db = HashDb::open("game.lhdb")?;                      // mmap + validate header
let db = HashDb::open_bytes(include_bytes!("t.hashdb").as_slice())?; // embedded/testing
```

Files are treated as untrusted: `open` validates structure, and every read
bounds-checks its own extent. A malformed file errors on open; a corrupted entry makes
`get` return `None`, never UB or a panic.

## Lookup patterns

### Point lookups

```rust
let path: Option<std::borrow::Cow<'_, str>> = db.get(hash);
let present: bool = db.contains(hash);
```

`get` returns a `Cow`: borrowed straight from the mmap for raw arenas, owned for
compressed ones (published `.lhdb` tables are compressed, so expect `Cow::Owned`).

**Misses are cheap by design** - a miss is decided by binary search over the key array
and never touches the string arena, so "probe everything, most won't be there" loops
(the shape of every WAD/bin scanning workload) don't pay decompression costs. This is a
format invariant with a regression test behind it, not an accident.

### "Does this path exist?" - hashing strings with the table's algorithm

Each table records which hash algorithm produced its keys (`HashKind`: XXH64,
FNV-1a-32, or XXH3) and its casing rule (`Casing` - League tables hash the lowercased
path). `hash_path` uses **that** algorithm and casing, so consumers never hard-code
either:

```rust
let hash = db.hash_path("assets/characters/ahri/skins/skin11/ahri_skin11.dds");
if db.contains(hash) { /* the community already knows this path */ }
```

### Batch lookups

Resolving many hashes at once (e.g. every chunk of a WAD archive) should use
`get_batch`, not a `get` loop:

```rust
let chunk_hashes: Vec<u64> = wad.chunks().map(|c| c.path_hash()).collect();
for (hash, path) in db.get_batch(&chunk_hashes) {
    match path {
        Some(p) => println!("{p}"),
        None => println!("{hash:016x} (unknown)"),
    }
}
```

Results come back **in input order**, but internally hits are resolved in arena order so
each compressed frame is decompressed at most once. Paths are stored in lexicographic
order, so a directory's files cluster into the same frames - batch-resolving one
archive's contents touches few frames.

### Enumerating everything

```rust
for (hash, path) in db.iter() { /* streams in path order, one decompress per frame */ }
```

`iter` yields in **arena order** (lexicographic path order, *not* key order), which is
also the natural order for building tree views or prefix scans.

`values()` is the same walk without reading a key - a name list, an autocomplete
corpus, a dump:

```rust
for path in db.values() { /* every path, in path order */ }
```

### Searching by path prefix

Because the arena is sorted by path, the entries under a directory are one
contiguous run, and finding it is a binary search rather than a scan:

```rust
for (hash, path) in db.prefix("assets/characters/ahri/").take(50) {
    println!("{hash:016x} {path}");
}
```

The cost is about `log2(entries)` frames decompressed - single-digit milliseconds
on the 2.3 M-entry game table, whether the prefix matches 13,000 paths or none.
The iterator is lazy, so `take(n)` stops the walk instead of filtering a list that
was already built. An empty prefix is the whole table. `LayeredHashDb::prefix`
applies the same shadowing rule `iter` does, one binary search per base.

This is the query consumers used to download the CommunityDragon txt list for.

Two caveats. It is only meaningful for a table whose arena is in **path order** -
the reference writer's layout, and what every published table does, but the format
permits any layout and the reader cannot check it cheaply. And the first call on a
table that does not carry the arena-order section pays for a sort (~0.5 s on
`game`, once per process, shared by every clone); a table built with
`mimir build --arena-order` reads the order out of the file instead, at ~16 % more
file. See [BENCHMARKS.md](BENCHMARKS.md).

`load_all()` decodes the whole table into an owned `HashMap<u64, Box<str>>`. This is the
opt-in "resident mode" for tools that genuinely need map semantics or maximum lookup
throughput - it forfeits the shared-page-cache benefit and costs the full decompressed
size in private memory, so reach for it last.

### Threads and lifecycle

- All lookups take `&self`, and `HashDb` is `Send + Sync` (guaranteed by a compile-time
  test) - share one handle across threads, e.g. in an `Arc`. Don't open one handle per
  thread; you'd duplicate validation work for nothing.
- **Open lazily, drop freely.** A mod loader that only occasionally resolves a hash
  should open the table at first use, not at startup, and can drop the handle to release
  the mapping - reopening is cheap. Resident memory stays low regardless: pages fault in
  on demand and are reclaimed under pressure.
- An updater can publish a new version while you hold a handle: your mmap stays valid
  (you keep reading the old version) until you reopen via the store.

## Extending a table with custom hashes

Mod tooling often introduces paths the community tables don't know. The sanctioned way
is `LayeredHashDb` - an in-memory overlay consulted before one or more immutable base
tables, so you don't hand-roll a second map plus fallback:

```rust
use ltk_hashdb::LayeredHashDb;

let mut db = LayeredHashDb::from_bases(vec![store.open(Table::Game)?])?;

// Hashes with the first base's algorithm and returns the hash:
let h = db.insert_path("assets/mymod/custom.dds").expect("has a base");
db.insert(precomputed_hash, "assets/mymod/other.bin"); // or bring your own hash
db.extend(pairs);                                      // or bulk-load

assert!(db.contains(h));
let path = db.get(h);         // overlay first, then each base in order
```

Base files are never mutated. `db.bases()` exposes the underlying tables in priority
order; `db.overlay_len()` counts overlay-only entries. Overlay entries are per-process
and not persisted - if you want them shared or durable, contribute them upstream to the
CommunityDragon txt lists (the canonical source).

### Layering several base tables under one overlay

The same type takes N bases, which is what you want when a workload spans **more than
one** table - WAD chunk resolution consults both `Game` and `Lcu`. Lookups consult
the overlay first, then each base in push order, first hit wins.

```rust
use ltk_mimir_cache::{HashStore, Table};

let store = HashStore::discover()?;

// Open the WAD path tables into one layered reader; missing tables are reported,
// not fatal - the tool stays usable and their hashes just miss.
let (mut db, errors) = store.open_layered(&[Table::Game, Table::Lcu])?;
for (table, e) in &errors {
    eprintln!("skipping {table}: {e}");
}

db.insert(precomputed_hash, "assets/mymod/custom.bin"); // overlay writes as before

// Batch-resolve a WAD's chunk hashes: the overlay is checked first, then each base
// handles only the residual misses, so every base's frames decompress at most once.
for (hash, path) in db.get_batch(&chunk_hashes) {
    match path {
        Some(p) => println!("{p}"),
        None => println!("{hash:016x} (unknown)"),
    }
}
```

`open_layered` only fails outright on a set spanning more than one hash universe -
`&[Table::BinEntries, Table::BinFields]`, say, where one table would answer the other's
hashes with an unrelated path. That is checked before anything is opened. Everything
else (a missing table, an unreadable file, a file that isn't the table it's filed under)
comes back in the per-table error list, so a partial cache still gives you a usable
reader.

`open_layered` is the convenience most WAD consumers want; `open_many` is the
lower-level primitive it's built from - it pairs each requested table with its
`Result<HashDb, OpenError>` so you can warn-and-skip instead of aborting on the first
missing one. `db.bases()` exposes the opened tables in priority order, `overlay_len()`
counts overlay entries, and `insert_path` hashes with the **first** base's algorithm
(returning `None` when there are no bases). The League-domain hex fallback for a total
miss stays in your tool - mimir returns `Option`, it never invents a hex string.

## Getting and updating tables

Tables are published as GitHub release assets: each release carries every table as an
immutable `<table>-<version>.lhdb` plus the `manifest.json` describing them
(per-table filename, sha256, entry count, key width, and input provenance).
`releases/latest/download/manifest.json` is the stable URL for the current set.

The whole loop - fetch the remote manifest, keep every table whose sha256 already
matches, download and checksum-verify the rest, install atomically, GC superseded
versions, all under the single-updater lock - is `HashStore::update`. The one thing
you bring is the transport: the cache crate deliberately ships no HTTP client, so you
hand it a `Fetch` (any closure from asset filename to bytes) backed by whatever client
your app already has. The fetcher's error is an associated type (`Fetch::Error`), and
`update` returns `UpdateError<F::Error>` - a failed download surfaces your client's
concrete error (e.g. `reqwest::Error`), not a boxed `dyn Error`:

```rust
use ltk_mimir_cache::{HashStore, UpdateOptions, UpdateOutcome};

let store = HashStore::discover()?;
let fetch = |filename: &str| -> Result<Vec<u8>, MyClientError> {
    let url = format!(
        "https://github.com/LeagueToolkit/mimir/releases/latest/download/{filename}"
    );
    my_http_get(&url)   // reqwest, ureq, curl - your choice; your error type
};

match store.update(&fetch, UpdateOptions::default())? {
    UpdateOutcome::Locked => {}     // another process is updating; leave it to them
    UpdateOutcome::Completed(report) => {
        if report.is_up_to_date() { /* nothing changed */ }
        for table in &report.installed { /* log the refresh */ }
    }
}
```

> `mimir update` is exactly this call plus a reqwest-backed `Fetch` - still the right
> tool for cron jobs and setup scripts. **Readers need none of this** - they just `open`.

#### Streaming, progress, and cancellation

`fetch_to` is the trait's actual primitive; the closure form above buffers a whole
asset because that is all a closure can do. Implementing `fetch_to` instead streams
straight into the sink `update` hands you - which is a file in the cache directory,
hashed as it fills - so a 38 MiB table is never in memory and its bytes are written
once, not copied into place afterwards.

That sink is also the progress and cancellation hook. Wrap a fetcher, pass the inner
one a sink of your own, and you see every chunk:

```rust
use std::io::Write;
use ltk_mimir_cache::{Fetch, FetchError};

struct Reporting<F>(F);

impl<F: Fetch> Fetch for Reporting<F> {
    type Error = F::Error;

    fn fetch_to(
        &self,
        filename: &str,
        sink: &mut (dyn Write + Send),
    ) -> Result<u64, FetchError<Self::Error>> {
        self.0.fetch_to(filename, &mut Counting { sink, done: 0 })
    }
}
```

`Counting::write` forwards to `sink`, adds up what it forwarded, and returns an
`io::Error` to cancel - which arrives at the caller as `FetchError::Sink`, aborts the
run, and removes the partial file. `FetchError::Transport` is the other half: the
fetcher's own failure, kept separate so a dropped connection is never reported as a
full disk.

`mimir update` is this wrapper plus an `indicatif` spinner.

Async apps (tokio + async reqwest, a GUI runtime) use `HashStore::update_async` with an
`AsyncFetch` instead - same loop, same guarantees, awaiting each download. The future
cannot borrow the filename, so build owned state before the `async move` block:

```rust
let fetch = |filename: &str| {
    let url = format!(
        "https://github.com/LeagueToolkit/mimir/releases/latest/download/{filename}"
    );
    async move {
        let response = client.get(&url).send().await?.error_for_status()?;
        Ok::<_, reqwest::Error>(response.bytes().await?.to_vec())
    }
};

match store.update_async(&fetch, UpdateOptions::default()).await? {
    UpdateOutcome::Locked => {}
    UpdateOutcome::Completed(report) => { /* as above */ }
}
```

The future is `Send` (given a `Sync` fetcher) and cancel-safe: dropping it releases the
update lock and removes staged downloads, leaving the cache exactly as it was. Local
work between downloads (checksum verification, the final install) runs inline on the
calling task - up to a few hundred milliseconds per table; if that stalls your executor,
run the blocking `update` on a dedicated thread instead.

Semantics worth relying on (both variants):

- **Immutability.** A published `.lhdb` is never modified; updates are new files under
  new names. Concurrent readers keep their mapping until they reopen.
- **Crash safety.** Both the table copy and the manifest write go through
  temp-file + fsync + atomic rename; a torn update (or a failed download / checksum
  mismatch, which errors before anything installs) leaves the old manifest intact.
- **Single updater, many readers.** The update runs under a cross-process try-lock;
  a `Locked` outcome means someone else is already on it. Readers never take the lock.
  `lock_holder()` names that someone - pid and start time - so a UI can say "syncing
  since 14:02" instead of something that reads like a hang. `lock_update_timeout(d)`
  queues behind the current updater for up to `d` instead of giving up at once.
- **Forward compatibility.** A release published by a newer mimir is never fatal.
  Tables this build has no id for are skipped into `report.unknown_tables`; tables
  published in a `.hashdb` format it cannot open are skipped into
  `report.unsupported_tables`, leaving whatever version the cache already holds in
  place. A higher `schema` and manifest fields this build has never seen are simply
  ignored - only an explicit `min_reader_schema` above this build refuses the
  document outright.
- **Format channels.** The updater asks for `manifest-v<format>.json` and falls back
  to `manifest.json`, so a release that keeps building an older format keeps feeding
  the builds pinned to it.

### Asking without doing: `check`

`update` takes the exclusive lock before it fetches anything, so it is the wrong
call for "are we behind?". `check` fetches the published manifest, diffs it against
the cache, and returns - no download, no install, no lock. Safe on a timer, and safe
while another process is midway through an update.

```rust
use ltk_mimir_cache::{HashStore, ReleaseSource, UreqFetch};

let store = HashStore::discover()?;
let remote = UreqFetch::new(ReleaseSource::github("LeagueToolkit/mimir"));

let report = store.check(&remote)?;
if !report.is_up_to_date() {
    println!("{} table(s) behind", report.behind());
    for diff in &report.tables {
        // `game: 2026-07-03 -> 2026-07-10 (outdated)`
        let have = diff.local.as_ref().map_or("-", |local| &local.version);
        println!("{}: {have} -> {} ({})", diff.table, diff.remote.version, diff.status);
    }
}
```

`TableStatus` distinguishes `Current`, `Absent`, `Stale`, `FileMissing` (the manifest
points at a file that is gone), and `Unsupported` (a `.hashdb` format this build
cannot open - reported, but not something an update could fix, so it does not count
toward `behind()`).

Two version labels can differ while the bytes do not: a release relabels every table
it rebuilds, and `check` compares sha256s, so a table can read `Current` at an older
label. That is the same test `update` makes before skipping a download.

### Custom pipelines: the primitives

`update` is built from public pieces you can drive yourself when your flow differs -
installing tables you built locally instead of downloading, custom retention, etc.:

```rust
use ltk_mimir_cache::{CommitItem, HashStore, Source, Table};

let store = HashStore::discover()?;

// Become the single updater, or leave it to whoever already is.
let Some(_lock) = store.try_lock_update()? else { return Ok(()) };

// Install atomically: files are copied durable first, the manifest pointer
// swaps last, so a concurrent reader never sees a half-written table.
//
// The source is recorded on every table this call installs and on the manifest's
// `last_run`. Tables the call does not mention keep the provenance they were
// installed with; pass `CommitItem::with_source` when one table came from
// somewhere else.
store.commit(
    &[CommitItem::new(Table::Game, "2026-07-10", built_game_path)],
    Some(Source { repo: Some("CommunityDragon/Data".into()), commit, inputs_sha256 }),
)?;

// Clean up superseded versions. Files still mapped by a reader are skipped
// (reported in `retained`) and retried on a later run - never an error.
let report = store.gc()?;
```

### Verifying a table

`open` validates structure only. There are two checks above it:

```rust
db.verify_index()?;  // xxh3 checksum over every stored byte, keys strictly ascending,
                     // and the arena-order section if there is one
db.verify()?;        // the above, plus every entry in bounds and valid UTF-8
```

Both hash the whole file, arena included, so both catch **damage** - bit rot, a
truncating write, a half-finished copy. Only `verify` also decompresses the arena to
prove the file is **well-formed**, which is what a table built by a broken writer
fails. On the 42 MiB `game` table that is ~85 ms against ~940 ms.

So: `verify_index` on a schedule or at startup, `verify` once after installing from
an untrusted channel, or when a table is behaving strangely and you want to know why.
`mimir verify --index-only` is the cheap tier from the command line.

## Building your own tables

The format has nothing League-specific: any `u64 → string` map you want mmap-served can
ship as a `.hashdb`. `HashDbWriter` is a streaming builder:

```rust
use ltk_hashdb::{Casing, Compression, HashDbWriter, HashKind, KeyWidth};

let mut w = HashDbWriter::new(KeyWidth::U64, Compression::default()) // 16 KiB frames, level 19
    .hash_kind(HashKind::Xxh64)         // recorded so readers can `hash_path`
    .casing(Casing::AsciiInsensitive);       // keys hash the ASCII-lowercased path (League rule);
                                        // defaults to Sensitive (hash bytes as given)

w.insert(hash, "assets/characters/aatrox/aatrox.bin");
w.extend(pairs);

let mut out = std::fs::File::create("mytable.hashdb")?;
let stats = w.build(&mut out)?;         // sort, dedup, compress, write
println!("{} entries, {} bytes", stats.entries, stats.file_len);
```

- `build` sorts by key and dedups identical pairs; the same key mapped to two
  *different* strings is a `BuildError::DuplicateKey`.
- `Compression::None` trades disk size for borrowed (`Cow::Borrowed`) zero-copy reads -
  right for small tables or latency-critical embedding. `Compression::Zeekstd` is what
  published tables use; see `docs/BENCHMARKS.md` for the frame-size/level trade-offs.
- Strings are arena-packed in lexicographic order automatically (that ordering is what
  makes the compression ratio and directory-local batch reads good - you don't opt in).

## Hunting unknown hashes

`ltk_mimir_gen` resolves unknown hashes by generating candidate paths from a known
corpus and testing them. The highest-yield input is the WAD archive itself:
`mine_wad` parses its `.bin` chunks for literal strings (and greps everything else
for path-shaped tokens), and the chunk table *is* the unknown set:

```rust
use ltk_mimir_gen::guessers::SeedStrings;
use ltk_mimir_gen::{mine_wad, GuessContext, Hunt};
use ltk_mimir_cache::Table;

let mined = mine_wad("Ahri.wad.client".as_ref())?;      // seed strings + chunk hashes

let mut ctx = GuessContext::new(Table::Game.key_config());
ctx.add_known(db.iter().map(|(_, p)| p.into_owned()));  // corpus to mutate from
ctx.add_unknown(mined.chunk_hashes.into_iter().filter(|&h| !db.contains(h)));

let report = Hunt::default_game()                       // rounds until dry
    .with(SeedStrings::new(mined.strings))
    .run(&mut ctx);
for (hash, path) in &report.resolved {
    println!("{hash:016x} {path}");
}
```

`Hunt::default_game()` / `default_lcu()` bundle the cheap, high-yield guessers
(including the lcu ↔ game cross-referencer); chain `.with(...)` to add more, e.g.
`SeedStrings` for strings you mined yourself, or the opt-in wordlist guessers whose
cost scales with corpus × vocabulary. Fair warning: a hunt saturates every core by
design (rayon) - cap it with `RAYON_NUM_THREADS` when it must coexist with other
work. Newly resolved paths should be contributed upstream to CommunityDragon - the
txt lists stay canonical.

## The CLI

The `mimir` binary wraps the same APIs for CI pipelines (the release workflow), table
maintainers, and one-off inspection:

```sh
mimir build   --input hashes.game.txt --table game --out game.hashdb   # txt → .hashdb
mimir get     0x1234abcd --table game        # one lookup from the shared cache
mimir update  [--force]                      # install the latest release into the shared cache
mimir gen     --known known.txt --wad Ahri.wad.client --table game --out found.txt
mimir merge   a.txt b.txt --out merged.txt   # sorted dedup merge of txt lists
mimir bundle  --inputs <dir> --out <dir>     # build all tables + manifest for a release
mimir verify  game.hashdb                    # structure + full checksum
mimir stats   game.hashdb                    # sizes, entry count, compression ratio
```

If you're writing a tool, bind the library; don't spawn the CLI to parse its output.
