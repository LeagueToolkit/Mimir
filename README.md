<div align="center">
  <a href="https://github.com/LeagueToolkit">
    <img src="https://avatars.githubusercontent.com/u/28510182?s=200&v=4" alt="LeagueToolkit logo" width="96" height="96">
  </a>
  <h1>mimir</h1>
</div>

Hash → path tables for League of Legends tooling, stored as a compact, memory-mapped,
seekable binary format (`.hashdb`). It replaces CommunityDragon's ~348 MB of
`hashes.*.txt` with a **~52 MiB binary that is usable as shipped** - no parse step before
the first lookup, and **one copy in the page cache that every tool on the machine
shares**.

The format itself is general-purpose: a read-only map from integer keys to string values,
with nothing League-specific in its layout. League Toolkit distributes its own tables
under the `.lhdb` extension (identical bytes).

<div align="center">

**[Install](#install)** · **[Quick start](#quick-start)** · **[Library API](#library-api)** ·
**[CLI](#cli)** · **[Design](docs/DESIGN.md)**

</div>

## Install

Nothing is on crates.io yet, so depend on the repository directly:

```toml
[dependencies]
ltk_hashdb = { git = "https://github.com/LeagueToolkit/mimir" }

# Only if you want the shared cache and the download-driven updater.
# `ureq` gives you a blocking fetcher, `reqwest` an async one; both are optional.
ltk_mimir_cache = { git = "https://github.com/LeagueToolkit/mimir", features = ["ureq"] }
```

The CLI:

```sh
cargo install --git https://github.com/LeagueToolkit/mimir ltk_mimir_cli
```

## Quick start

Pull the published tables into the shared cache, then resolve a hash out of it:

```sh
mimir update
mimir get 0x1234abcd --table game
```

The same thing from Rust:

```rust
use ltk_mimir_cache::{HashStore, Table};

let store = HashStore::discover()?;
let db = store.open_shared(Table::Game)?;   // mmap + validate header; no parse step

if let Some(path) = db.get(0x1234_5678_9abc_def0) {
    println!("{path}");
}
```

The cache lives in the platform data directory - `%LOCALAPPDATA%\LeagueToolkit\hashes` on
Windows, `$XDG_DATA_HOME/LeagueToolkit/hashes` on Linux, `~/Library/Application
Support/LeagueToolkit/hashes` on macOS - and `MIMIR_DIR` overrides it.

## Library API

Two crates matter to consumers. `ltk_hashdb` is the format: open a file, resolve hashes.
`ltk_mimir_cache` is everything around it: where tables live on disk, which version is
active, and how they get updated. A tool that ships its own tables needs only the first.

### Resolving a hash

`get` returns a **`PathRef`**, which borrows its bytes instead of copying them - out of
the mapping for a raw arena, out of the cached decompressed frame for a compressed one.
It derefs to `str`, so it behaves like one:

```rust
use ltk_hashdb::HashDb;

let db = HashDb::open("game.lhdb")?;

if let Some(path) = db.get(hash) {
    println!("{path}");                     // Display
    if path.ends_with(".dds") { /* … */ }   // Deref<Target = str>
    let owned: String = path.into_owned();  // copy only when you keep it
}

// A miss is decided by binary search over the key array and never touches the arena.
assert!(!db.contains(0xdead_beef));
```

Hashing a path with the table's own algorithm, so you never have to know which one it is:

```rust
let hash = db.hash_path("assets/characters/ahri/ahri.bin");
assert!(db.contains(hash));
```

### Resolving many hashes

Both forms resolve hits in **arena order** so each compressed frame is decompressed once.
Use `get_batch` when you want results back in input order, and `for_each_batch` when you
want them streamed with no intermediate `Vec`:

```rust
// Collected, in input order.
for (hash, path) in db.get_batch(&chunk_hashes) {
    match path {
        Some(path) => println!("{path}"),
        None => println!("{hash:016x} (unknown)"),
    }
}

// Streamed. Calls arrive in arena order, so the first argument is the input position.
db.for_each_batch(&chunk_hashes, |i, hash, path| match path {
    Some(path) => println!("{i}: {path}"),
    None => println!("{i}: {hash:016x}"),
});
```

### Layering tables and adding your own hashes

`LayeredHashDb` puts a writable in-memory overlay over one or more read-only bases.
Lookups consult the overlay first, then each base in push order; the first hit wins, and
no base file is ever mutated. This is what WAD consumers want, since chunk resolution
spans both `game` and `lcu`:

```rust
use ltk_mimir_cache::{HashStore, Table};

let store = HashStore::discover()?;

// Missing tables are reported, not fatal - the tool stays usable and their hashes miss.
let (mut db, errors) = store.open_layered(&[Table::Game, Table::Lcu]);
for (table, e) in &errors {
    eprintln!("skipping {table:?}: {e}");
}

// Register a path your mod introduced; it is hashed with the first base's algorithm.
let hash = db.insert_path("assets/mymod/custom.dds").expect("has a base");
assert_eq!(db.get(hash).as_deref(), Some("assets/mymod/custom.dds"));
```

> [!NOTE]
> Every base must agree on key width, hash algorithm, and casing, because lookups take a
> hash the caller already computed and no base re-hashes it. `game` and `lcu` do; the four
> 32-bit `bin*` tables are separate hash universes and must not be layered together.

### Enumerating a table

```rust
// Streams in arena order (lexicographic path order), one decompress per frame.
for (hash, path) in db.iter() {
    println!("{hash:016x} {path}");
}

// Opt-in resident mode: the whole table as an owned map. Costs the full decompressed
// size in private memory and forfeits the shared page cache, so reach for it last.
let map = db.load_all();
```

### Updating the cache

The crate ships no HTTP client of its own - you hand it a fetcher. `UreqFetch` (feature
`ureq`) and `ReqwestFetch` (feature `reqwest`, async) cover the common case:

```rust
use ltk_mimir_cache::{HashStore, ReleaseSource, UpdateOptions, UpdateOutcome, UreqFetch};

let store = HashStore::discover()?;
let remote = UreqFetch::new(ReleaseSource::github("LeagueToolkit/mimir"));

match store.update(&remote, UpdateOptions::default())? {
    UpdateOutcome::Completed(report) => println!("installed {:?}", report.installed),
    UpdateOutcome::Locked => println!("another process is already updating"),
}
```

Only tables whose sha256 differs are downloaded. Installs are atomic - versioned files
land first, the manifest pointer flips last - so a reader sees either the whole old
version or the whole new one, and readers never take a lock.

### Building your own table

```rust
use std::fs::File;
use ltk_hashdb::{Casing, Compression, HashDbWriter, HashKind, KeyWidth};

let mut writer = HashDbWriter::new(KeyWidth::U64, Compression::default())
    .hash_kind(HashKind::Xxh64)     // recorded, so readers can hash new paths
    .casing(Casing::Insensitive);   // League tables hash the lowercased path

writer.insert(hash, "assets/characters/ahri/ahri.bin");
writer.extend(pairs);

let stats = writer.build(File::create("mine.hashdb")?)?;
println!("{} entries, {} bytes", stats.entries, stats.file_len);
```

`Compression::default()` is the publishing configuration: 16 KiB frames at level 19, the
measured size/latency knee. `Compression::None` writes a raw arena that lookups borrow
straight out of the mapping.

### API surface

**`HashDb`** - one `.hashdb` file. Cheap to clone; every clone shares the mapping and the
frame cache. `Send + Sync`.

| Method | |
|---|---|
| `open` · `open_bytes` | mmap a file, or open an in-memory image |
| `options()` | open-time knobs: `frame_cache_bytes(n)`, `0` disables |
| `get` | resolve a hash → `Option<PathRef>` |
| `try_get` | as `get`, but a corrupt arena errors instead of reading as a miss |
| `get_into` | copy into a reusable `String`; holds no frame afterwards |
| `contains` | membership, never touches the arena |
| `get_batch` · `for_each_batch` | bulk resolve, collected or streamed |
| `iter` · `load_all` | enumerate in arena order, or decode into an owned map |
| `hash_path` | hash a string with this table's algorithm and casing |
| `verify` · `is_healthy` | full integrity pass; sticky flag set by a failed read |
| `len` · `key_width` · `hash_kind` · `casing` · `is_compressed` | shape |
| `downgrade` | a `WeakHashDb` for registries that must not pin the table |

**`LayeredHashDb`** - an overlay over N ordered bases.

| Method | |
|---|---|
| `from_bases` · `push_base` | layer read-only tables, highest priority first |
| `insert` · `insert_path` · `extend` | write to the overlay, shadowing every base |
| `get` · `contains` · `get_into` | overlay first, then each base in order |
| `get_batch` · `for_each_batch` | staged bulk resolve; each base sees only the residual |
| `iter` | every entry, each shadowed key yielded once by the layer that answers it |
| `bases` · `overlay_len` · `base_len` · `is_healthy` | shape |

**`HashStore`** - the shared cache directory.

| Method | |
|---|---|
| `discover` · `at` | resolve the platform cache dir, or point at your own |
| `open_shared` | open the active version, reusing a handle this store already has |
| `open` · `open_many` | open a fresh mapping, one table or several |
| `open_layered` | open several into one `LayeredHashDb`, reporting per-table errors |
| `manifest` · `path_for` | what is installed, and where |
| `update` · `update_async` | compare → download → verify → install → GC |
| `commit` · `gc` · `try_lock_update` | publish versions, sweep old ones, take the lock |

**`PathRef`** - a resolved path. `Deref<Target = str>`, plus `as_str`, `is_owned`
(whether the bytes were copied rather than borrowed), and `into_owned`.

**`HashDbWriter`** - `new` → `hash_kind` / `casing` → `insert` / `extend` → `build`.

## CLI

```
mimir <COMMAND>

build    Build a .hashdb table from a txt hash list (lines of `<hex-hash> <path>`)
get      Resolve one hash from a .hashdb file or the shared cache
update   Download the latest published tables into the shared cache
gen      Run the hunt engine: discover paths for still-unknown hashes
merge    Sorted dedup merge of CDragon txt hash lists
bundle   Build all tables + manifest from CDragon txt inputs, staged for a GH release
verify   Structural + checksum validation of a .hashdb file
stats    Sizes, entry counts, compression ratio of a .hashdb file
```

```sh
# Build a table from a CDragon txt list
mimir build --input hashes.game.txt --table game --out game.hashdb

# Resolve a hash, from a file or from the shared cache
mimir get 0x1234abcd --file game.hashdb
mimir get 0x1234abcd --table game

# Keep the shared cache current (--url for a mirror, --dir for a private cache)
mimir update
mimir update --force

# Inspect and validate
mimir stats game.hashdb
mimir verify game.hashdb
```

## Crates

| Crate | Role |
|-------|------|
| `ltk_hashdb` | The `.hashdb` format: `mmap` reader (`HashDb`) + streaming writer |
| `ltk_mimir_cache` | Shared cache dir, manifest, versioned publish, update lock, GC, in-process updater |
| `ltk_mimir_gen` | Hash-discovery ("hunt") engine for still-unknown hashes |
| `ltk_mimir_cli` | The `mimir` binary |

## Documentation

| | |
|---|---|
| [`docs/DESIGN.md`](docs/DESIGN.md) | Why this exists, how the format works, what it measures |
| [`docs/FORMAT.md`](docs/FORMAT.md) | Byte-level specification of `.hashdb`, format version 1 |
| [`docs/CONSUMERS.md`](docs/CONSUMERS.md) | Integration guide: lookup patterns, threading, custom pipelines |
| [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) | Frame-size and compression measurements, with reproduction steps |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | Planned work, in dependency order |

## Status

Early development. The format, reader/writer, shared cache, release publishing
(`mimir bundle` plus a scheduled CI job that ships every table as versioned `.lhdb`
release assets, rebuilt from the canonical CommunityDragon txt lists), the
download-driven `mimir update` flow, and the hunt engine - including WAD string mining
(`mimir gen --wad`) - are all in place.

The txt lists stay canonical; the binaries are generated release artifacts, never the
source of truth.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
