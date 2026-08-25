//! Shared, versioned, multi-process cache for League Toolkit hash tables.
//!
//! The tables are large and every League tool wants the same ones, so they live
//! in one directory per machine rather than one per application: an immutable
//! `<table>-<version>.lhdb` per [`Table`], a `manifest.json` naming the active
//! version of each, and an update lock. [`ltk_hashdb`] owns the bytes inside a
//! table; this crate owns where they live, which version is current, and how a
//! new one is published while other processes are reading the old one.
//!
//! # Where to start
//!
//! | Goal | Start at |
//! |---|---|
//! | Find the cache | [`HashStore::discover`], or [`HashStore::at`] for an explicit directory |
//! | Resolve WAD chunk hashes | [`HashStore::open_layered`] |
//! | Resolve one table's hashes | [`HashStore::open_shared`] |
//! | Ask whether an update is available | [`HashStore::check`] - lock-free, downloads nothing |
//! | Install the latest release | [`HashStore::update`], or [`HashStore::update_async`] |
//! | Draw a bar while it installs | [`UpdateObserver`] on [`UpdateOptions`] |

//! | Report what the cache holds | [`HashStore::manifest`] |
//! | Publish tables built elsewhere | [`HashStore::commit`], then [`HashStore::gc`] |
//!
//! # Resolving hashes
//!
//! Reading is lock-free: published files are immutable and the manifest is
//! swapped atomically, so a reader maps a table and uses it without coordinating
//! with anyone.
//!
//! A WAD chunk hash may be named by either half of the path table, so resolve
//! against both at once. [`open_layered`](HashStore::open_layered) opens what it
//! can and returns the rest as per-table errors, so a missing table costs its
//! hashes rather than the whole tool:
//!
//! ```no_run
//! use ltk_mimir_cache::{HashStore, Table};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = HashStore::discover()?;
//! let (paths, unavailable) = store.open_layered(&[Table::Game, Table::Lcu])?;
//! for (table, why) in &unavailable {
//!     eprintln!("{table} unavailable: {why}");
//! }
//!
//! match paths.get(0x1e5b_0e0d_5e6e_3f7f) {
//!     Some(path) => println!("{path}"),
//!     None => println!("unknown hash"),
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Only tables sharing a [`HashUniverse`] may be layered: `binfields` and
//! `binentries` are both 32-bit FNV-1a over unrelated strings, so a table asked
//! for the other's hash answers confidently and wrongly. `open_layered` refuses
//! the mix before it opens anything.
//!
//! For a single table prefer [`open_shared`](HashStore::open_shared) over
//! [`open`](HashStore::open): it hands back a handle this process already holds
//! instead of re-mapping the file and re-parsing its seek table - over ten
//! thousand frame records on `game` - and because the register is keyed on the
//! manifest's active filename, it begins serving a newer version by itself once
//! one is installed.
//!
//! # Updating
//!
//! The crate ships no HTTP client. [`update`](HashStore::update) takes a
//! [`Fetch`] - anything that turns a release asset filename into bytes - and
//! runs the whole compare, download, verify, install and GC loop under the
//! single-updater lock, so readers see either the whole old release or the whole
//! new one, and a second updater is told to wait rather than racing:
//!
//! ```no_run
//! use ltk_mimir_cache::{Fetch, HashStore, UpdateOptions, UpdateOutcome};
//!
//! fn sync(store: &HashStore, remote: &impl Fetch) -> Result<(), Box<dyn std::error::Error>> {
//!     match store.update(remote, UpdateOptions::default())? {
//!         UpdateOutcome::Completed(run) if run.is_up_to_date() => println!("already current"),
//!         UpdateOutcome::Completed(run) => println!("installed {} tables", run.installed.len()),
//!         UpdateOutcome::Locked => match store.lock_holder()? {
//!             Some(holder) => println!("pid {} has been updating since {}", holder.pid, holder.since),
//!             None => println!("another process is updating"),
//!         },
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! [`check`](HashStore::check) is that same comparison without the lock and
//! without the downloads, for a UI showing "3 tables behind" - or, via
//! [`download_bytes`](CheckReport::download_bytes), "112 MiB to download" -
//! while some other process is midway through installing them.
//!
//! An [`UpdateObserver`] on [`UpdateOptions`] follows a run as it happens: the
//! plan, with a size per table, before the first connection, then the bytes as
//! each table streams in. That is what a progress bar needs and what wrapping a
//! fetcher cannot supply, since [`fetch_to`](Fetch::fetch_to) is handed one
//! filename at a time.
//!
//! Implementing [`Fetch`] is how a caller adds cancellation, or a directory of
//! files in tests - it streams into a sink, so a 38 MiB table never has to exist
//! in memory. The `ureq` and `reqwest` features ship one ready-made, over a
//! GitHub release or a mirror.
//!
//! # Publishing tables built elsewhere
//!
//! [`commit`](HashStore::commit) is the write primitive underneath `update`, and
//! what a builder calls directly: it installs files under immutable
//! `<table>-<version>.lhdb` names and flips the manifest last, each step a temp
//! file, an fsync and a rename. Hold the update lock across it, then
//! [`gc`](HashStore::gc) to unlink superseded versions nothing still has open:
//!
//! ```no_run
//! use ltk_mimir_cache::{CommitItem, HashStore, Table};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let store = HashStore::discover()?;
//! let Some(_lock) = store.try_lock_update()? else {
//!     return Ok(()); // someone else is publishing
//! };
//!
//! store.commit(&[CommitItem::new(Table::Game, "2026-08-24", "game.lhdb")], None)?;
//! println!("swept {} superseded files", store.gc()?.deleted.len());
//! # Ok(())
//! # }
//! ```
//!
//! # The cache directory
//!
//! [`discover`](HashStore::discover) resolves it without creating it: a
//! non-empty `MIMIR_DIR` overrides everything, otherwise it is the platform data
//! directory - `%LOCALAPPDATA%\LeagueToolkit\hashes` on Windows,
//! `$XDG_DATA_HOME/LeagueToolkit/hashes` on Linux, and
//! `~/Library/Application Support/LeagueToolkit/hashes` on macOS.
//!
//! ```text
//! hashes/
//!   game-2026-08-24.lhdb   # immutable once written
//!   lcu-2026-08-24.lhdb
//!   ...
//!   manifest.json          # active version + sha256 per table
//!   .update.lock           # cross-process single-updater lock
//! ```
//!
//! # Feature flags
//!
//! Off by default - the crate is client-agnostic without them, and both only add
//! a [`Fetch`] implementation over a `ReleaseSource` (a GitHub repo's latest
//! release, or a base URL serving the same assets).
//!
//! - **`ureq`** - `UreqFetch`, a blocking [`Fetch`].
//! - **`reqwest`** - `ReqwestFetch`, an [`AsyncFetch`] for
//!   [`update_async`](HashStore::update_async).

mod dir;
mod error;
#[cfg(any(feature = "ureq", feature = "reqwest"))]
mod fetch;
mod fsutil;
mod lock;
mod manifest;
mod store;
mod table;
mod update;

pub use error::{
    CheckError, CommitError, FetchError, GcError, ManifestError, NoCacheDirError, OpenError,
    ParseTableError, UniverseMismatch, UpdateError,
};
#[cfg(feature = "reqwest")]
pub use fetch::ReqwestFetch;
#[cfg(feature = "ureq")]
pub use fetch::UreqFetch;
#[cfg(any(feature = "ureq", feature = "reqwest"))]
pub use fetch::{HttpFetchError, ReleaseSource};
pub use lock::{LockHolder, UpdateLock};
pub use manifest::{Manifest, Source, TableEntry, SCHEMA_VERSION};
pub use store::{CommitItem, GcReport, HashStore};
pub use table::{HashUniverse, Table};
pub use update::{
    AsyncFetch, CheckReport, Fetch, PlannedTable, TableDiff, TableStatus, UnsupportedTable,
    UpdateObserver, UpdateOptions, UpdateOutcome, UpdateReport,
};
