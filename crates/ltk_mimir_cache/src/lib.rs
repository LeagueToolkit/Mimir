//! Shared, versioned, multi-process cache for League Toolkit hash tables
//! (`.lhdb` files - the `.hashdb` format under the League convention).
//!
//! - Resolves the cache directory (env → CDragon → platform dir)
//! - Reads the manifest and opens the active table file read-only
//! - Commits new versions atomically under a single-updater lock with lazy GC
//! - Updates the cache in-process from a published release, through a
//!   caller-supplied fetcher ([`HashStore::update`] /
//!   [`HashStore::update_async`])

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
    CommitError, GcError, ManifestError, NoCacheDirError, OpenError, UniverseMismatch, UpdateError,
};
#[cfg(feature = "reqwest")]
pub use fetch::ReqwestFetch;
#[cfg(feature = "ureq")]
pub use fetch::UreqFetch;
#[cfg(any(feature = "ureq", feature = "reqwest"))]
pub use fetch::{HttpFetchError, ReleaseSource};
pub use lock::UpdateLock;
pub use manifest::{Manifest, Source, TableEntry, SCHEMA_VERSION};
pub use store::{CommitItem, GcReport, HashStore};
pub use table::{HashUniverse, Table};
pub use update::{AsyncFetch, Fetch, UpdateOptions, UpdateOutcome, UpdateReport};
