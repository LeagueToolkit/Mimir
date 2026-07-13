# ltk_mimir_cache

The shared, versioned, multi-process **cache** for League Toolkit hash tables - the I/O
policy layer that sits on top of the [`ltk_hashdb`](../ltk_hashdb) format. It decides
*where* tables live on disk, *which* version is active, and how new versions are published
so that many tools on one machine can read the same files while a single updater swaps in a
new release underneath them - without locks on the read path and without ever showing a
reader a half-written file.

`ltk_hashdb` owns the byte format; this crate owns everything around it: directory
resolution, the `manifest.json` pointer, atomic versioned publishing, the single-updater
lock, and lazy garbage collection.

## The cache directory

One directory holds every table plus a manifest and an update lock:

```
hashes/
  game-2026-07-08.lhdb        # versioned, immutable once written
  lcu-2026-07-08.lhdb
  binentries-2026-07-08.lhdb
  ...
  manifest.json               # pointer: active version + sha256 per table
  .update.lock                # cross-process single-updater lock
```

Its location is resolved (without being created) from the platform data directory by
default, which `MIMIR_DIR` overrides when set:

- `MIMIR_DIR` - points directly at the tables directory; overrides everything.
- Otherwise, the platform data dir:
  - Windows: `%LOCALAPPDATA%\LeagueToolkit\hashes\`
  - Linux: `$XDG_DATA_HOME/LeagueToolkit/hashes` (fallback `~/.local/share/...`)
  - macOS: `~/Library/Application Support/LeagueToolkit/hashes`

## Reading (lazy, lock-free)

Tables are immutable and the manifest is swapped atomically, so readers never coordinate:
read the manifest, `mmap` the active file, use it, drop it.

```rust
use ltk_mimir_cache::{HashStore, Table};

let store = HashStore::discover()?;             // MIMIR_DIR override, else platform dir
let db = store.open(Table::Game)?;              // manifest → active file → mmap
if let Some(path) = db.get(0x1234_5678_9abc_def0) {
    println!("{path}");
}
```

`open` validates structure only: it trusts the manifest's download-time sha256 and
stays cheap and lazy. Use [`HashDb::verify`](../ltk_hashdb) for a full checksum pass.

## Committing (single updater, atomic)

`commit` is the cache-side primitive: it installs one or more freshly built `.lhdb` files
under immutable `<table>-<version>.lhdb` names, then flips the manifest to point at them
**last**, so a reader mid-lookup sees either the whole old version or the whole new one. The
table copies and the manifest write are each a temp file + `fsync` + rename.

```rust
use ltk_mimir_cache::{CommitItem, HashStore, Table};

let store = HashStore::discover()?;

// Serialize updaters (readers need no lock). `None` means another process is updating.
if let Some(_lock) = store.try_lock_update()? {
    store.commit(
        &[CommitItem::new(Table::Game, "2026-07-08", "build/game.lhdb")],
        None, // optional provenance (source repo + commit / inputs sha256)
    )?;
    store.gc()?; // reclaim superseded versions no reader still maps
}
```

`gc` deletes versioned files no longer referenced by the manifest (plus stray `.tmp`
leftovers). A file the OS refuses to unlink - still mapped by a reader, the classic Windows
case - is left in place and reported in `GcReport::retained` to retry later, never surfaced
as an error.

> The download-driven `mimir update` flow is built on exactly these primitives: fetch the
> release manifest, fingerprint-skip per table by sha256, download + verify what changed,
> then `commit` + `gc` under `try_lock_update`.

## API surface

| Item | Role |
|------|------|
| `HashStore::discover` / `at` | Resolve the cache dir, or use an explicit one |
| `HashStore::manifest` / `open` / `path_for` | Read the manifest; open / locate the active table |
| `HashStore::try_lock_update` → `UpdateLock` | Non-blocking cross-process single-updater lock |
| `HashStore::commit` | Install versioned files + atomically swap the manifest |
| `HashStore::gc` → `GcReport` | Reclaim unreferenced, unmapped versions |
| `HashStore::update` / `update_async` | The download-driven update loop (blocking or async) over a caller-supplied `Fetch` / `AsyncFetch` |
| `Table` | The eight logical tables (`id` / `from_id` / `ALL`) |
| `Manifest` / `Source` / `TableEntry` | The `manifest.json` schema (serde) |

See [`docs/CONSUMERS.md`](../../docs/CONSUMERS.md) for the consumer-facing integration guide.

## License

MIT OR Apache-2.0.
