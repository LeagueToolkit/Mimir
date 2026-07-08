//! A [`HashDb`] plus an in-memory overlay of extra entries.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::HashDb;

/// A `HashDb` with a mutable in-memory overlay (e.g. runtime mod hashes). The base
/// file is never mutated; lookups consult the overlay first, then the table.
pub struct ExtendedHashDb {
    base: HashDb,
    overlay: HashMap<u64, Box<str>>,
}

impl ExtendedHashDb {
    pub fn new(base: HashDb) -> Self {
        Self {
            base,
            overlay: HashMap::new(),
        }
    }

    pub fn insert(&mut self, hash: u64, path: impl Into<Box<str>>) {
        self.overlay.insert(hash, path.into());
    }

    /// Hash `path` with the base table's algorithm, insert it, and return the hash -
    /// "register this path" without knowing the algorithm.
    pub fn insert_path(&mut self, path: &str) -> u64 {
        let hash = self.base.hash_path(path);
        self.insert(hash, path);
        hash
    }

    pub fn extend<'a>(&mut self, it: impl IntoIterator<Item = (u64, &'a str)>) {
        self.overlay
            .extend(it.into_iter().map(|(k, p)| (k, Box::from(p))));
    }

    /// Overlay first, then the base table.
    pub fn get(&self, hash: u64) -> Option<Cow<'_, str>> {
        match self.overlay.get(&hash) {
            Some(path) => Some(Cow::Borrowed(&**path)),
            None => self.base.get(hash),
        }
    }

    pub fn contains(&self, hash: u64) -> bool {
        self.overlay.contains_key(&hash) || self.base.contains(hash)
    }

    pub fn base(&self) -> &HashDb {
        &self.base
    }

    pub fn overlay_len(&self) -> usize {
        self.overlay.len()
    }
}
