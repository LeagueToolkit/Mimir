# ltk_mimir_cache

A shared, versioned cache of League Toolkit hash tables that several processes on one
machine can use at once. It stores tables in the
[`ltk_hashdb`](https://crates.io/crates/ltk_hashdb) format and handles:

- where the cache directory is
- which version of each table is active (`manifest.json`)
- installing new versions atomically, so readers never see a partial file
- a lock so only one process updates the cache at a time
- removing versions that are no longer used
- checking for and downloading updates from a published release

Readers take no locks.

## The cache directory

One directory holds every table, the manifest, and the update lock:

```text
hashes/
  game-2026-07-08.lhdb        # versioned, never modified after it is written
  lcu-2026-07-08.lhdb
  binentries-2026-07-08.lhdb
  ...
  manifest.json               # active version and sha256 per table
  .update.lock                # held by the process that is updating
```

`HashStore::discover` finds the directory without creating it:

- `MIMIR_DIR`, if set and non-empty
- otherwise the platform data directory:
  - Windows: `%LOCALAPPDATA%\LeagueToolkit\hashes\`
  - Linux: `$XDG_DATA_HOME/LeagueToolkit/hashes` (default `~/.local/share/LeagueToolkit/hashes`)
  - macOS: `~/Library/Application Support/LeagueToolkit/hashes`

Only this crate (or the `mimir` CLI) should change files in the directory. Tables are
memory-mapped, which relies on them never being modified.

## Reading

```rust,no_run
use ltk_mimir_cache::{HashStore, Table};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = HashStore::discover()?;

    // One table. `open_shared` reuses a handle this process already has.
    let db = store.open_shared(Table::Game)?;
    if let Some(path) = db.get(0x1234_5678_9abc_def0) {
        println!("{path}");
    }

    // WAD chunk hashes can be in either path table, so look in both; earlier tables win.
    let (paths, unavailable) = store.open_layered(&[Table::Game, Table::Lcu])?;
    for (table, error) in unavailable {
        eprintln!("{table} unavailable: {error}");
    }
    println!("{} base tables", paths.bases().len());
    Ok(())
}
```

Opening checks the file's structure and that its key config matches the table. It does
not re-check the sha256, which was verified when the file was installed. Call
`HashDb::verify` for a full check.

## Updating from a release

`HashStore::update` compares the local manifest with a release, downloads the tables that
changed, checks their sha256, installs them, and removes old versions. The caller supplies
the download function; any closure `Fn(&str) -> Result<Vec<u8>, E>` works. With the `ureq`
feature, `UreqFetch::new(ReleaseSource::github("owner/repo"))` downloads from a GitHub
release. `HashStore::check` reports what an update would do without downloading anything.

```rust,no_run
use std::path::Path;

use ltk_mimir_cache::{HashStore, UpdateOptions, UpdateOutcome};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = HashStore::discover()?;

    // Read release files from a local directory.
    let fetch = |filename: &str| std::fs::read(Path::new("release").join(filename));
    match store.update(&fetch, UpdateOptions::default())? {
        UpdateOutcome::Completed(report) => println!("installed {:?}", report.installed),
        UpdateOutcome::Locked => println!("another process is updating"),
        _ => {}
    }
    Ok(())
}
```

`update_async` does the same with an `AsyncFetch`, such as `ReqwestFetch` from the
`reqwest` feature.

## Committing tables you built

`commit` installs built `.lhdb` files under `<table>-<version>.lhdb` names and then replaces
the manifest. Both `commit` and `gc` require the update lock:

```rust,no_run
use ltk_mimir_cache::{CommitItem, HashStore, Table};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = HashStore::discover()?;

    // `None` means another process is updating.
    if let Some(lock) = store.try_lock_update()? {
        let items = [CommitItem::new(Table::Game, "2026-07-08", "build/game.lhdb")];
        store.commit(&lock, &items, None)?;
        store.gc(&lock)?;
    }
    Ok(())
}
```

`gc` deletes table files the manifest no longer references and leftover `.tmp` files. On
Windows a file that a reader still has mapped cannot be deleted; `gc` lists it in
`GcReport::retained` and tries again next time.

## Features

| Feature | Adds |
|---------|------|
| `ureq` | `UreqFetch`, a blocking HTTP fetcher |
| `reqwest` | `ReqwestFetch`, an async HTTP fetcher |

Both use connect and read timeouts and refuse files larger than
`DEFAULT_MAX_ASSET_SIZE` (512 MiB) unless configured otherwise.

See the [consumer guide](https://github.com/LeagueToolkit/mimir/blob/main/docs/CONSUMERS.md)
for more detail.

## License

Apache-2.0.
