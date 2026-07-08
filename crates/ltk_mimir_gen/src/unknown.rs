//! Membership set for the still-unknown hashes a hunt is trying to resolve.

/// A sorted key set with a bitmap prefilter.
///
/// Candidate checks are overwhelmingly misses (billions of generated strings
/// against thousands of unknown hashes), so the prefilter is sized at ~8 bits
/// per key: a random miss usually costs a single memory access instead of a
/// binary search.
pub struct UnknownSet {
    /// Sorted, deduped.
    keys: Vec<u64>,

    bitmap: Vec<u64>,

    /// `bitmap` bit count minus one (bit count is a power of two).
    mask: u64,
}

impl UnknownSet {
    pub fn new(mut keys: Vec<u64>) -> Self {
        keys.sort_unstable();
        keys.dedup();

        let bits = (keys.len().max(128) * 8).next_power_of_two();
        let mut bitmap = vec![0u64; bits / 64];
        let mask = bits as u64 - 1;
        for &key in &keys {
            let bit = key & mask;
            bitmap[(bit / 64) as usize] |= 1 << (bit % 64);
        }

        Self { keys, bitmap, mask }
    }

    pub fn contains(&self, key: u64) -> bool {
        let bit = key & self.mask;
        if self.bitmap[(bit / 64) as usize] & (1 << (bit % 64)) == 0 {
            return false;
        }

        self.keys.binary_search(&key).is_ok()
    }

    pub fn keys(&self) -> &[u64] {
        &self.keys
    }

    /// Consume the set, returning its (sorted, deduped) key vector without
    /// copying - used to append more keys and rebuild.
    pub fn into_keys(self) -> Vec<u64> {
        self.keys
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

impl Default for UnknownSet {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership() {
        let set = UnknownSet::new(vec![3, 1, 2, 2, u64::MAX, 0]);
        assert_eq!(set.len(), 5);
        for k in [0, 1, 2, 3, u64::MAX] {
            assert!(set.contains(k));
        }
        for k in [4, 5, 1000, u64::MAX - 1] {
            assert!(!set.contains(k));
        }
    }

    #[test]
    fn empty() {
        let set = UnknownSet::new(Vec::new());
        assert!(set.is_empty());
        assert!(!set.contains(0));
    }
}
