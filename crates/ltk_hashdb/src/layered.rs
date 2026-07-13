//! An in-memory overlay layered over an ordered list of read-only base tables.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::{Casing, HashDb, HashKind, KeyWidth};

/// The key configuration a base hashes under. Every base in a [`LayeredHashDb`]
/// must agree on this triple; a base that diverges can never be hit by a caller's
/// precomputed probe (see the type-level invariant).
fn base_config(db: &HashDb) -> (KeyWidth, HashKind, Casing) {
    (db.key_width(), db.hash_kind(), db.casing())
}

/// A writable in-memory overlay on top of ordered read-only [`HashDb`] bases.
///
/// Lookups consult the overlay first, then each base in push order; the first hit
/// wins. Base files are never mutated. This generalises [`ExtendedHashDb`] (one
/// base) to the N-base case consumers need when several tables (e.g. League's
/// `game` and `lcu`) sit under one overlay.
///
/// # Base configuration invariant
///
/// Lookups take a `u64` the caller already computed, and each base binary-searches
/// its own key set with that raw value - there is no per-base re-hashing. So every
/// base (and any path registered via [`insert_path`](Self::insert_path)) must share
/// the same key configuration: [`key_width`](HashDb::key_width),
/// [`hash_kind`](HashDb::hash_kind), and [`casing`](HashDb::casing). A base that
/// diverges is silently unreachable - the caller's probes were hashed for a
/// different scheme, so they can never match it. [`push_base`](Self::push_base) and
/// [`from_bases`](Self::from_bases) `debug_assert!` this; release builds skip the
/// check. League's `game`/`lcu` tables are uniform (XXH64 / U64 / case-insensitive),
/// so the common path always satisfies it.
///
/// [`ExtendedHashDb`]: crate::ExtendedHashDb
#[derive(Default)]
pub struct LayeredHashDb {
    overlay: HashMap<u64, Box<str>>,

    /// Bases in priority order: earlier ones shadow later ones.
    bases: Vec<HashDb>,
}

impl LayeredHashDb {
    /// An empty layered db: no overlay, no bases. Everything resolves to `None`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Layer an empty overlay over `bases`, in the given priority order (`bases[0]`
    /// shadows `bases[1]`, and so on). In debug builds, asserts every base shares
    /// the first one's key configuration (see the type-level invariant).
    pub fn from_bases(bases: Vec<HashDb>) -> Self {
        if let Some((first, rest)) = bases.split_first() {
            let cfg = base_config(first);
            for db in rest {
                debug_assert_eq!(
                    base_config(db),
                    cfg,
                    "LayeredHashDb bases must share key config (key_width/hash_kind/casing); \
                     a divergent base is unreachable by a caller's precomputed probe"
                );
            }
        }

        Self {
            overlay: HashMap::new(),
            bases,
        }
    }

    /// Append a lower-priority base below all existing ones.
    pub fn push_base(&mut self, db: HashDb) {
        if let Some(first) = self.bases.first() {
            debug_assert_eq!(
                base_config(&db),
                base_config(first),
                "LayeredHashDb bases must share key config (key_width/hash_kind/casing); \
                 a divergent base is unreachable by a caller's precomputed probe"
            );
        }

        self.bases.push(db);
    }

    /// Insert an overlay entry (e.g. a runtime mod hash). Shadows every base.
    pub fn insert(&mut self, hash: u64, path: impl Into<Box<str>>) {
        self.overlay.insert(hash, path.into());
    }

    /// Bulk-insert overlay entries.
    pub fn extend<'a>(&mut self, it: impl IntoIterator<Item = (u64, &'a str)>) {
        self.overlay
            .extend(it.into_iter().map(|(k, p)| (k, Box::from(p))));
    }

    /// Hash `path` with the **first** base's algorithm/casing/width, insert it into
    /// the overlay, and return the hash - "register this path" without knowing the
    /// algorithm. Returns `None` when there are no bases (no algorithm to hash with).
    ///
    /// Bases with differing key widths make this first-base-wins; callers mixing
    /// widths should precompute the hash and use [`insert`](Self::insert) instead.
    pub fn insert_path(&mut self, path: &str) -> Option<u64> {
        let hash = self.bases.first()?.hash_path(path);
        self.insert(hash, path);
        Some(hash)
    }

    /// Overlay first, then each base in push order; the first hit wins.
    pub fn get(&self, hash: u64) -> Option<Cow<'_, str>> {
        if let Some(path) = self.overlay.get(&hash) {
            return Some(Cow::Borrowed(&**path));
        }
        self.bases.iter().find_map(|base| base.get(hash))
    }

    /// Membership test across the overlay and every base; never touches an arena.
    pub fn contains(&self, hash: u64) -> bool {
        self.overlay.contains_key(&hash) || self.bases.iter().any(|base| base.contains(hash))
    }

    /// Staged bulk resolve. The overlay is consulted first, then each base's
    /// [`get_batch`](HashDb::get_batch) handles only the residual misses, so every
    /// base's frames decompress at most once per call. Results are yielded in input
    /// order. This is the payoff over calling [`get`](Self::get) N times.
    pub fn get_batch<'a>(
        &'a self,
        hashes: &'a [u64],
    ) -> impl Iterator<Item = (u64, Option<Cow<'a, str>>)> + 'a {
        let mut results: Vec<Option<Cow<'a, str>>> = Vec::new();
        results.resize_with(hashes.len(), || None);

        // Layer 0: overlay, O(1) per hash. Positions still missing stay in
        // `residual`, in input order.
        let mut residual: Vec<usize> = Vec::new();
        for (i, &h) in hashes.iter().enumerate() {
            match self.overlay.get(&h) {
                Some(path) => results[i] = Some(Cow::Borrowed(&**path)),
                None => residual.push(i),
            }
        }

        // Layers 1..: each base in push order, on the shrinking residual set. `sub`
        // is built in residual order, and `base.get_batch` yields in input order, so
        // the zip lines up positionally while the base re-sorts by arena offset
        // internally (preserving frame coalescing).
        for base in &self.bases {
            if residual.is_empty() {
                break;
            }
            let sub: Vec<u64> = residual.iter().map(|&i| hashes[i]).collect();
            let mut next: Vec<usize> = Vec::new();
            for ((_, opt), &pos) in base.get_batch(&sub).zip(&residual) {
                match opt {
                    Some(path) => results[pos] = Some(path),
                    None => next.push(pos),
                }
            }
            residual = next;
        }

        hashes.iter().copied().zip(results)
    }

    /// The base tables, in priority order.
    pub fn bases(&self) -> &[HashDb] {
        &self.bases
    }

    /// Number of overlay entries.
    pub fn overlay_len(&self) -> usize {
        self.overlay.len()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::LayeredHashDb;
    use crate::{Compression, HashDb, HashDbWriter, KeyWidth};

    /// Build a raw (uncompressed) U64 table from `(hash, path)` pairs.
    fn raw_db(entries: &[(u64, &str)]) -> HashDb {
        let mut w = HashDbWriter::new(KeyWidth::U64, Compression::None);
        for &(h, p) in entries {
            w.insert(h, p);
        }
        let mut out = Cursor::new(Vec::new());
        w.build(&mut out).expect("build");
        HashDb::open_bytes(out.into_inner()).expect("open")
    }

    /// Build a raw table with an explicit key width, for config-mismatch tests.
    fn raw_db_width(width: KeyWidth, entries: &[(u64, &str)]) -> HashDb {
        let mut w = HashDbWriter::new(width, Compression::None);
        for &(h, p) in entries {
            w.insert(h, p);
        }
        let mut out = Cursor::new(Vec::new());
        w.build(&mut out).expect("build");
        HashDb::open_bytes(out.into_inner()).expect("open")
    }

    /// Build a compressed U64 table whose 100 clustered paths span several frames.
    fn compressed_db(frame_size: u32) -> HashDb {
        let mut w = HashDbWriter::new(
            KeyWidth::U64,
            Compression::Zeekstd {
                frame_size,
                level: 3,
            },
        );
        for i in 0..100u64 {
            w.insert(
                i * 3,
                &format!("assets/characters/champ{i}/skins/skin{i}.bin"),
            );
        }
        let mut out = Cursor::new(Vec::new());
        w.build(&mut out).expect("build");
        HashDb::open_bytes(out.into_inner()).expect("open")
    }

    #[test]
    fn layering_order_shadows_lower_layers() {
        let base0 = raw_db(&[(1, "base0/one"), (2, "base0/two")]);
        let base1 = raw_db(&[(2, "base1/two"), (3, "base1/three")]);
        let mut db = LayeredHashDb::from_bases(vec![base0, base1]);
        db.insert(1, "overlay/one");

        // Overlay shadows base 0.
        assert_eq!(db.get(1).as_deref(), Some("overlay/one"));
        // Base 0 shadows base 1 on a shared key.
        assert_eq!(db.get(2).as_deref(), Some("base0/two"));
        // Falls through to base 1.
        assert_eq!(db.get(3).as_deref(), Some("base1/three"));
        // Total miss.
        assert_eq!(db.get(999), None);
        assert!(!db.contains(999));
        assert!(db.contains(3));
    }

    #[test]
    fn get_and_get_batch_agree_in_input_order() {
        let base0 = raw_db(&[(10, "base0/ten"), (20, "shadowed")]);
        let base1 = raw_db(&[(20, "base1/twenty"), (30, "base1/thirty")]);
        let mut db = LayeredHashDb::from_bases(vec![base0, base1]);
        db.insert(5, "overlay/five");

        // Mixed set: overlay hit, base-0 hit, base-1 hit, miss, duplicate.
        let probes = [5u64, 10, 20, 30, 999, 10];
        let batch: Vec<_> = db
            .get_batch(&probes)
            .map(|(h, o)| (h, o.map(|c| c.into_owned())))
            .collect();
        let expected: Vec<_> = probes
            .iter()
            .map(|&h| (h, db.get(h).map(|c| c.into_owned())))
            .collect();
        assert_eq!(batch, expected);
    }

    #[test]
    fn get_batch_preserves_frame_coalescing() {
        // Small frames so the 100 clustered paths span several frames.
        let base = compressed_db(256);
        let frames = base.decompressions(); // 0 before any read
        assert_eq!(frames, 0);
        let db = LayeredHashDb::from_bases(vec![base]);

        // Batch every real key (i*3 for i in 0..100) plus some misses.
        let mut probes: Vec<u64> = (0..100u64).map(|i| i * 3).collect();
        probes.extend([1, 2, 4, u64::MAX]);
        let hits = db.get_batch(&probes).filter(|(_, o)| o.is_some()).count();
        assert_eq!(hits, 100);

        // Coalesced: decompressions track #frames, well below the 100 hits (the
        // fixture spans ~20 frames at this size). Per-hit resolution would be ~100.
        // No public frame-count accessor here, so assert against the hit count.
        let decomps = db.bases()[0].decompressions();
        assert!(decomps > 0, "hits must decompress");
        assert!(
            decomps < hits as u64,
            "batch decompressed {decomps} times for {hits} clustered hits - coalescing defeated"
        );
    }

    #[test]
    fn empty_and_no_base() {
        let mut db = LayeredHashDb::new();
        assert_eq!(db.get(1), None);
        assert_eq!(db.insert_path("some/path"), None);

        // Overlay still works with no bases.
        db.insert(42, "manual");
        assert_eq!(db.get(42).as_deref(), Some("manual"));
        assert_eq!(db.overlay_len(), 1);
        assert!(db.bases().is_empty());
    }

    #[test]
    #[should_panic(expected = "must share key config")]
    fn push_base_rejects_divergent_key_config() {
        let u64_base = raw_db_width(KeyWidth::U64, &[(1, "u64/one")]);
        let u32_base = raw_db_width(KeyWidth::U32, &[(2, "u32/two")]);
        let mut db = LayeredHashDb::from_bases(vec![u64_base]);

        // Debug-only guard: layering a U32 base under a U64 base is unreachable.
        db.push_base(u32_base);
    }

    #[test]
    #[should_panic(expected = "must share key config")]
    fn from_bases_rejects_divergent_key_config() {
        let u64_base = raw_db_width(KeyWidth::U64, &[(1, "u64/one")]);
        let u32_base = raw_db_width(KeyWidth::U32, &[(2, "u32/two")]);

        let _ = LayeredHashDb::from_bases(vec![u64_base, u32_base]);
    }

    #[test]
    fn insert_path_uses_first_base() {
        let base = raw_db(&[(1, "seed")]);
        let mut db = LayeredHashDb::from_bases(vec![base]);
        let path = "assets/characters/aatrox/aatrox.bin";
        let hash = db.insert_path(path).expect("has a base");
        assert_eq!(db.get(hash).as_deref(), Some(path));
    }
}
