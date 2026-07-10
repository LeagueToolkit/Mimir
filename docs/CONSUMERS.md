# Consumer integration guide

How to use mimir **as a library**. This is the primary interface: the `mimir` CLI is a
thin operational wrapper (CI publishing, quick inspection) and most consumers - WAD
unpackers, `.bin` inspectors, mod loaders, asset browsers - should never shell out to it.
The CLI is covered [at the end](#the-cli).

## Which crate do I need?

| You want to… | Depend on |
|---|---|
| Resolve hashes against the machine-shared League tables (the common case) | `ltk_mimir_cache` (+ `ltk_hashdb` for the `HashDb` type it hands back) |
| Keep that shared cache up to date from your app (no CLI) | `ltk_mimir_cache` - `HashStore::update` + your HTTP client |
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
  `Error::MissingManifest` - see [Getting and updating tables](#getting-and-updating-tables).

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
is `ExtendedHashDb` - an in-memory overlay consulted before the immutable base, so you
don't hand-roll a second map plus fallback:

```rust
use ltk_hashdb::ExtendedHashDb;

let mut ext = ExtendedHashDb::new(store.open(Table::Game)?);

// Hashes with the base table's algorithm and returns the hash:
let h = ext.insert_path("assets/mymod/custom.dds");
ext.insert(precomputed_hash, "assets/mymod/other.bin"); // or bring your own hash
ext.extend(pairs);                                       // or bulk-load

assert!(ext.contains(h));
let path = ext.get(h);        // overlay first, then base
```

The base file is never mutated. `ext.base()` exposes the underlying `HashDb`;
`ext.overlay_len()` counts overlay-only entries. Overlay entries are per-process and
not persisted - if you want them shared or durable, contribute them upstream to the
CommunityDragon txt lists (the canonical source).

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
your app already has:

```rust
use ltk_mimir_cache::{FetchError, HashStore, UpdateOptions, UpdateOutcome};

let store = HashStore::discover()?;
let fetch = |filename: &str| -> Result<Vec<u8>, FetchError> {
    let url = format!(
        "https://github.com/LeagueToolkit/mimir/releases/latest/download/{filename}"
    );
    Ok(my_http_get(&url)?)   // reqwest, ureq, curl - your choice
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

Semantics worth relying on:

- **Immutability.** A published `.lhdb` is never modified; updates are new files under
  new names. Concurrent readers keep their mapping until they reopen.
- **Crash safety.** Both the table copy and the manifest write go through
  temp-file + fsync + atomic rename; a torn update (or a failed download / checksum
  mismatch, which errors before anything installs) leaves the old manifest intact.
- **Single updater, many readers.** The update runs under a cross-process try-lock;
  a `Locked` outcome means someone else is already on it. Readers never take the lock.
- **Forward compatibility.** Tables in the remote manifest this build doesn't know are
  skipped and reported in `report.unknown_tables`, never fatal.

### Custom pipelines: the primitives

`update` is built from public pieces you can drive yourself when your flow differs -
installing tables you built locally instead of downloading, custom retention, etc.:

```rust
use ltk_mimir_cache::{HashStore, PublishItem, Source, Table};

let store = HashStore::discover()?;

// Become the single updater, or leave it to whoever already is.
let Some(_lock) = store.try_lock_update()? else { return Ok(()) };

// Install atomically: files are copied durable first, the manifest pointer
// swaps last, so a concurrent reader never sees a half-written table.
store.publish(
    &[PublishItem::new(Table::Game, "2026-07-10", built_game_path)],
    Some(Source { repo: Some("CommunityDragon/Data".into()), commit, inputs_sha256 }),
)?;

// Clean up superseded versions. Files still mapped by a reader are skipped
// (reported in `retained`) and retried on a later run - never an error.
let report = store.gc()?;
```

### Verifying a table

`open` validates structure only. After downloading from an untrusted channel - or when
debugging - run the full check:

```rust
db.verify()?;   // xxh3 checksum over all sections, keys strictly ascending,
                // every entry in bounds and valid UTF-8
```

## Building your own tables

The format has nothing League-specific: any `u64 → string` map you want mmap-served can
ship as a `.hashdb`. `HashDbWriter` is a streaming builder:

```rust
use ltk_hashdb::{Casing, Compression, HashDbWriter, HashKind, KeyWidth};

let mut w = HashDbWriter::new(KeyWidth::U64, Compression::default()) // 16 KiB frames, level 19
    .hash_kind(HashKind::Xxh64)         // recorded so readers can `hash_path`
    .casing(Casing::Insensitive);       // keys hash the lowercased path (League rule);
                                        // defaults to Sensitive (hash bytes as given)

w.insert(hash, "assets/characters/aatrox/aatrox.bin");
w.extend(pairs);

let mut out = std::fs::File::create("mytable.hashdb")?;
let stats = w.build(&mut out)?;         // sort, dedup, compress, write
println!("{} entries, {} bytes", stats.entries, stats.file_len);
```

- `build` sorts by key and dedups identical pairs; the same key mapped to two
  *different* strings is an `Error::DuplicateKey`.
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
use ltk_hashdb::{Casing, HashKind, KeyWidth};

let mined = mine_wad("Ahri.wad.client".as_ref())?;      // seed strings + chunk hashes

let mut ctx = GuessContext::new(HashKind::Xxh64, Casing::Insensitive, KeyWidth::U64);
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
mimir publish --inputs <dir> --out <dir>     # build all tables + manifest for a release
mimir verify  game.hashdb                    # structure + full checksum
mimir stats   game.hashdb                    # sizes, entry count, compression ratio
```

If you're writing a tool, bind the library; don't spawn the CLI to parse its output.
