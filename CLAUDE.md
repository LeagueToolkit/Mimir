# CLAUDE.md

Guidance for working in the `mimir` workspace. Read this first; it points at the
authoritative design docs rather than duplicating them.

## What this is

A Rust toolkit that generates, stores, and serves League of Legends hash → path tables
as a compact, memory-mapped, seekable binary format (`.hashdb`; League tables ship as
`.lhdb`). It replaces/extends
CommunityDragon CDTB's `hashes.py` and the ~348 MB of `hashes.*.txt` artifacts with a
~50 MB binary that is usable as-shipped (no expansion step), `mmap`-friendly (one copy
shared across processes via the OS page cache), and cheap on misses.

See `README.md` for the elevator pitch; the deeper design docs live in `docs/`
(listed below).

## Workspace layout

Cargo workspace (`resolver = "2"`), four crates under `crates/`:

| Crate | Role |
|-------|------|
| `ltk_hashdb`       | The `.hashdb` format: `mmap` reader (`HashDb`) + streaming writer (`HashDbWriter`), `ExtendedHashDb` overlay |
| `ltk_mimir_cache` | Shared cache dir, `manifest.json`, versioned publish, update lock, GC, in-process updater (`HashStore`, `HashStore::update`) |
| `ltk_mimir_gen`   | Hash-discovery ("hunt") engine - guessers that resolve unknown hashes |
| `ltk_mimir_cli`   | The `mimir` binary (`build` / `get` / `verify` / `stats` / `gen` / `update` / `merge` / `bundle`) |

Docs live in `docs/`: `FORMAT.md` (byte-level spec, format version 1), `CONSUMERS.md`
(integration API), `BENCHMARKS.md` (frame-size/compression measurements).

## Conventions

- **Dependencies are workspace-wide.** Add crates to `[workspace.dependencies]` in the root
  `Cargo.toml` and reference them as `foo.workspace = true` in the crate. Same for
  package metadata (`edition.workspace = true`, etc.) and `[lints] workspace = true` —
  except `version`, which is per-crate so release-plz can bump published crates
  independently (see `release-plz.toml`; unpublished crates sit at `0.0.0` with
  `publish = false`).
- **Lints are strict.** CI runs `cargo clippy --all-targets -- -D warnings` and
  `cargo fmt --all -- --check`. Keep both green; warnings fail the build.
- **`league-toolkit` crates** (`ltk_hash`, `ltk_wad`, `ltk_meta`) come from crates.io.
  `ltk_ritobin` deliberately stays out (text-format parsing / hashtable name resolution,
  which nothing here needs).
- Little-endian only; the format targets x86-64 / aarch64.

### Rust code hygiene

- **Blank-line-separate documented fields.** When the fields of a struct (or variants of an
  enum) carry their own doc/comment lines, put a blank line between each one so the field and
  its comment read as a unit instead of a wall of text. Undocumented fields can stay packed.

  ```rust
  pub struct CommitItem {
      /// Which logical table this file is.
      pub table: Table,

      /// Version label used in the immutable filename (`<table>-<version>.lhdb`).
      pub version: String,
  }
  ```

- **Group statements by logical step; blank-line-separate the steps.** Inside a function,
  keep tightly-coupled lines together and put a blank line between distinct steps, so the body
  reads as a few labelled paragraphs rather than one undifferentiated block *or* a line-per-gap
  stutter. Rules of thumb:

  - A run of related bindings (parallel `let`s computing one thing) stays packed - no blanks.
  - A setup line that feeds the block right below it (`let n = …;` before a `for`/`while`/`if`/
    `match` that consumes `n`) stays attached to that block - they are one step.
  - Insert a blank between steps: setup → transform → emit/return, or between two independent
    computations that don't feed each other.
  - Put a blank before a trailing `return`/`Ok(…)` that concludes a multi-step body; a
    one-liner body needs none.
  - Group early-return guard clauses together, then a blank before the main logic.
  - Don't blank-separate *every* statement, and don't wall together steps that do different
    things - both hurt scannability.

  ```rust
  fn build(entry_count: u64, key_width: u64, offset_width: u64, chunks: &[&[u8]]) -> Result<Output> {
      // One step: parallel section-size bindings - packed.
      let keys_len = entry_count * key_width;
      let offsets_len = entry_count * offset_width;
      let lengths_len = entry_count * 2;

      // Next step: the setup line feeds the loop, so it stays attached.
      let mut hasher = Sha256::new();
      for chunk in chunks {
          hasher.update(chunk);
      }

      Ok(Output { keys_len, offsets_len, lengths_len, checksum: hasher.finalize() })
  }
  ```

## Format invariants (don't break these)

- A `.hashdb` file is **immutable once published**; updates ship as new versioned files.
- Files are **untrusted** (downloaded): the header + section bounds are validated on `open`,
  and every read bounds-checks its own extent. `verify()` (checksum + full scan) is opt-in.
- **A lookup miss must never touch the arena** - it's decided by binary search over the keys.
  There is a `decompressions` counter and a unit test asserting misses don't bump it; keep
  that invariant when changing the reader.
- The **txt files stay canonical** (community PRs, git merges). The binary is a generated
  release artifact, never the source of truth.

## Build & test

```sh
cargo build --workspace
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

Some tests/benches need real CDragon data, which is **gitignored** (`/data`, `*.hashdb`,
`*.lhdb`) and
skips cleanly when absent:

- `tests/golden.rs` (parity vs. pinned txt): set `MIMIR_CDRAGON_DIR` (e.g. `data/cdragon`).
- `benches/real.rs` (criterion, prebuilt tables): set `MIMIR_BUILD_DIR` (e.g. `data/build`).
- `examples/bench_real.rs`, `examples/compression_lab.rs`: frame-size / ratio exploration.

Local `data/cdragon/hashes.*.txt` inputs and `data/build/*.hashdb` outputs exist on the dev
machine but are not committed.

## Status & workflow

The toolkit is feature-complete: format, reader/writer, shared cache, update pipeline
(`mimir update` / `merge`), hunt engine incl. WAD mining, and release publishing are all
in place. The main remaining work is the LTK Manager integration, which lives in the
LTK Manager repo, not here.

Primary dev platform is Windows; CI runs the full suite on Linux, Windows, and macOS.
