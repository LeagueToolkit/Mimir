//! Shared, versioned, multi-process cache for League Toolkit hash tables
//! (`.lhdb` files - the `.hashdb` format under the League convention).
//!
//! - Resolves the cache directory (env → CDragon → platform dir)
//! - Reads the manifest and opens the active table file read-only
//! - Commits new versions atomically under a single-updater lock with lazy GC
//! - Updates the cache in-process from a published release, through a
//!   caller-supplied fetcher ([`HashStore::update`])

mod dir;
mod error;
mod fsutil;
mod lock;
mod manifest;
mod store;
mod update;

pub use error::{Error, Result};
pub use lock::UpdateLock;
pub use manifest::{Manifest, Source, TableEntry, SCHEMA_VERSION};
pub use store::{CommitItem, GcReport, HashStore};
pub use update::{Fetch, FetchError, UpdateOptions, UpdateOutcome, UpdateReport};

/// The logical hash tables, each stored as its own `.lhdb` file.
///
/// The two RST variants hash the same strings with different algorithms (XXH64
/// vs XXH3 for RST v5+), so they are separate tables (see `docs/FORMAT.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Table {
    Game,
    Lcu,
    BinEntries,
    BinTypes,
    BinFields,
    BinHashes,
    Rst,
    RstXxh3,
}

impl Table {
    /// Every logical table, in a stable order.
    pub const ALL: [Table; 8] = [
        Table::Game,
        Table::Lcu,
        Table::BinEntries,
        Table::BinTypes,
        Table::BinFields,
        Table::BinHashes,
        Table::Rst,
        Table::RstXxh3,
    ];

    /// The stable string id used in filenames and manifest keys.
    pub fn id(self) -> &'static str {
        match self {
            Table::Game => "game",
            Table::Lcu => "lcu",
            Table::BinEntries => "binentries",
            Table::BinTypes => "bintypes",
            Table::BinFields => "binfields",
            Table::BinHashes => "binhashes",
            Table::Rst => "rst",
            Table::RstXxh3 => "rst-xxh3",
        }
    }

    /// Parse a table from its [`id`](Table::id).
    pub fn from_id(id: &str) -> Option<Table> {
        Table::ALL.into_iter().find(|t| t.id() == id)
    }
}
