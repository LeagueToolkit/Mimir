//! An in-memory overlay layered over an ordered list of read-only base tables.

use std::collections::HashMap;
use std::fmt;

use crate::{HashDb, KeyConfigMismatch, PathRef};

/// A writable in-memory overlay on top of ordered read-only [`HashDb`] bases.
///
/// Lookups consult the overlay first, then each base in push order; the first hit
/// wins. Base files are never mutated. One base covers the "table plus my own
/// runtime hashes" case; several cover the one consumers reach for when a workload
/// spans more than one table (e.g. League's `game` and `lcu` under one overlay).
///
/// # Base configuration invariant
///
/// Lookups take a `u64` the caller already computed, and each base binary-searches
/// its own key set with that raw value - there is no per-base re-hashing. So every
/// base (and any path registered via [`insert_path`](Self::insert_path)) must share
/// one [`KeyConfig`](crate::KeyConfig). A base that diverges is unreachable - the caller's probes were
/// hashed for a different scheme, so they can never match it - which is why
/// [`push_base`](Self::push_base) and [`from_bases`](Self::from_bases) refuse one
/// instead of layering it. League's `game`/`lcu` tables are uniform (u64 / xxh64 /
/// ascii-case-insensitive), so the common path always satisfies it.
///
/// A shared key config is necessary, not sufficient: it says two bases *can* answer
/// each other's probes, not that they *should*. `binentries` and `binfields` share
/// one and mean entirely different things, so `HashStore::open_layered` refuses that
/// pairing on top of this check.
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
    /// shadows `bases[1]`, and so on).
    ///
    /// # Errors
    ///
    /// [`KeyConfigMismatch`] if any base hashes its keys differently from `bases[0]`
    /// - see the type-level invariant.
    pub fn from_bases(bases: Vec<HashDb>) -> Result<Self, KeyConfigMismatch> {
        if let Some((first, rest)) = bases.split_first() {
            let expected = first.key_config();
            for (i, db) in rest.iter().enumerate() {
                let found = db.key_config();
                if found != expected {
                    return Err(KeyConfigMismatch {
                        index: i + 1,
                        expected,
                        found,
                    });
                }
            }
        }

        Ok(Self {
            overlay: HashMap::new(),
            bases,
        })
    }

    /// Append a lower-priority base below all existing ones.
    ///
    /// # Errors
    ///
    /// [`KeyConfigMismatch`] if `db` hashes its keys differently from the first base
    /// - see the type-level invariant. The layer is left untouched.
    pub fn push_base(&mut self, db: HashDb) -> Result<(), KeyConfigMismatch> {
        if let Some(first) = self.bases.first() {
            let (expected, found) = (first.key_config(), db.key_config());
            if found != expected {
                return Err(KeyConfigMismatch {
                    index: self.bases.len(),
                    expected,
                    found,
                });
            }
        }

        self.bases.push(db);
        Ok(())
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
    pub fn get(&self, hash: u64) -> Option<PathRef<'_>> {
        if let Some(path) = self.overlay.get(&hash) {
            return Some(PathRef::borrowed(path));
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
        hashes: &[u64],
    ) -> impl Iterator<Item = (u64, Option<PathRef<'a>>)> + 'a {
        let mut results: Vec<Option<PathRef<'a>>> = Vec::new();
        results.resize_with(hashes.len(), || None);

        // Layer 0: overlay, O(1) per hash. Positions still missing stay in
        // `residual`, in input order.
        let mut residual: Vec<usize> = Vec::new();
        for (i, &h) in hashes.iter().enumerate() {
            match self.overlay.get(&h) {
                Some(path) => results[i] = Some(PathRef::borrowed(path)),
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

        let out: Vec<(u64, Option<PathRef<'a>>)> = hashes.iter().copied().zip(results).collect();
        out.into_iter()
    }

    /// Resolve a batch without collecting it, calling `f` per hash as it resolves.
    ///
    /// The streaming counterpart to [`get_batch`](Self::get_batch), with the same
    /// staging: the overlay answers first, then each base takes the residual. Hits
    /// arrive layer by layer and, within a base, in arena order rather than input
    /// order, so the first argument is the hash's position in `hashes`; hashes no
    /// layer answers are reported last.
    pub fn for_each_batch(&self, hashes: &[u64], mut f: impl FnMut(usize, u64, Option<&str>)) {
        let mut residual: Vec<usize> = Vec::new();
        for (i, &hash) in hashes.iter().enumerate() {
            match self.overlay.get(&hash) {
                Some(path) => f(i, hash, Some(path)),
                None => residual.push(i),
            }
        }

        for base in &self.bases {
            if residual.is_empty() {
                break;
            }

            let sub: Vec<u64> = residual.iter().map(|&i| hashes[i]).collect();
            let mut next: Vec<usize> = Vec::new();
            base.for_each_batch(&sub, |p, hash, path| match path {
                Some(path) => f(residual[p], hash, Some(path)),
                None => next.push(residual[p]),
            });
            residual = next;
        }

        for i in residual {
            f(i, hashes[i], None);
        }
    }

    /// Copy a path into `buf`, replacing what was there. `false` for a miss.
    ///
    /// See [`HashDb::get_into`] - the layered form consults the overlay first, then
    /// each base in push order.
    pub fn get_into(&self, hash: u64, buf: &mut String) -> bool {
        if let Some(path) = self.overlay.get(&hash) {
            buf.clear();
            buf.push_str(path);
            return true;
        }

        self.bases.iter().any(|base| base.get_into(hash, buf))
    }

    /// Every entry, overlay first and then each base in priority order.
    ///
    /// Shadowed entries are yielded once, by the layer that answers them - so this
    /// enumerates exactly what [`get`](Self::get) can resolve. Each base is walked in
    /// its own arena order, so its frames decompress once.
    pub fn iter(&self) -> impl Iterator<Item = (u64, PathRef<'_>)> {
        let overlay = self
            .overlay
            .iter()
            .map(|(&hash, path)| (hash, PathRef::borrowed(path)));

        let bases = self
            .bases
            .iter()
            .enumerate()
            .flat_map(move |(layer, base)| {
                base.iter()
                    .filter(move |(hash, _)| !self.shadows(*hash, layer))
            });

        overlay.chain(bases)
    }

    /// Whether a layer above `layer` already answers `hash`.
    fn shadows(&self, hash: u64, layer: usize) -> bool {
        self.overlay.contains_key(&hash)
            || self.bases[..layer].iter().any(|base| base.contains(hash))
    }

    /// Entries across every base, counting a key that appears in several of them once
    /// per base rather than once outright.
    ///
    /// An upper bound on what [`iter`](Self::iter) yields, and O(bases) to compute -
    /// the exact figure would cost a lookup per entry, so it is `iter().count()` when
    /// a caller genuinely needs it.
    pub fn base_len(&self) -> usize {
        self.bases.iter().map(HashDb::len).sum()
    }

    /// Whether every base has read cleanly so far - see [`HashDb::is_healthy`].
    pub fn is_healthy(&self) -> bool {
        self.bases.iter().all(HashDb::is_healthy)
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

impl fmt::Debug for LayeredHashDb {
    /// Shape only - the overlay's size and each base's shape. Never entries.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayeredHashDb")
            .field("overlay", &self.overlay.len())
            .field("bases", &self.bases)
            .finish()
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
        let mut db = LayeredHashDb::from_bases(vec![base0, base1]).expect("uniform bases");
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
        let mut db = LayeredHashDb::from_bases(vec![base0, base1]).expect("uniform bases");
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
        let db = LayeredHashDb::from_bases(vec![base]).expect("uniform bases");

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

    /// A U32 base under a U64 one is unreachable, so it is refused - in release
    /// builds too, which is the point of this not being a `debug_assert!`.
    #[test]
    fn push_base_rejects_divergent_key_config() {
        let u64_base = raw_db_width(KeyWidth::U64, &[(1, "u64/one")]);
        let u32_base = raw_db_width(KeyWidth::U32, &[(2, "u32/two")]);
        let mut db = LayeredHashDb::from_bases(vec![u64_base]).expect("one base");

        let err = db.push_base(u32_base).expect_err("divergent base refused");
        assert_eq!(err.index, 1);
        assert_eq!(err.expected.key_width(), KeyWidth::U64);
        assert_eq!(err.found.key_width(), KeyWidth::U32);
        assert_eq!(db.bases().len(), 1, "the layer is left untouched");
    }

    #[test]
    fn from_bases_rejects_divergent_key_config() {
        let u64_base = raw_db_width(KeyWidth::U64, &[(1, "u64/one")]);
        let u32_base = raw_db_width(KeyWidth::U32, &[(2, "u32/two")]);

        let err = LayeredHashDb::from_bases(vec![u64_base, u32_base]).expect_err("divergent base");
        assert_eq!(err.index, 1);
        let msg = err.to_string();
        assert!(
            msg.contains("u64/") && msg.contains("u32/"),
            "names both configs: {msg}"
        );
    }

    /// `iter` enumerates exactly what `get` can resolve: every shadowed key is
    /// yielded once, by the layer that answers it.
    #[test]
    fn iter_yields_each_key_once_from_its_answering_layer() {
        let base0 = raw_db(&[(1, "base0/one"), (2, "base0/two")]);
        let base1 = raw_db(&[(2, "base1/two"), (3, "base1/three")]);
        let mut db = LayeredHashDb::from_bases(vec![base0, base1]).expect("uniform bases");
        db.insert(1, "overlay/one");

        let mut seen: Vec<(u64, String)> = db
            .iter()
            .map(|(hash, path)| (hash, path.into_owned()))
            .collect();
        seen.sort();

        assert_eq!(
            seen,
            vec![
                (1, "overlay/one".to_owned()),
                (2, "base0/two".to_owned()),
                (3, "base1/three".to_owned()),
            ]
        );

        // Every yielded pair is what `get` answers with.
        for (hash, path) in &seen {
            assert_eq!(db.get(*hash).as_deref(), Some(path.as_str()));
        }

        // `base_len` counts the shadowed key in both bases, as documented.
        assert_eq!(db.base_len(), 4);
    }

    #[test]
    fn for_each_batch_matches_get_batch() {
        let base0 = raw_db(&[(10, "base0/ten"), (20, "shadowed")]);
        let base1 = raw_db(&[(20, "base1/twenty"), (30, "base1/thirty")]);
        let mut db = LayeredHashDb::from_bases(vec![base0, base1]).expect("uniform bases");
        db.insert(5, "overlay/five");

        let probes = [5u64, 10, 20, 30, 999, 10];
        let mut streamed: Vec<(u64, Option<String>)> = vec![(0, None); probes.len()];
        db.for_each_batch(&probes, |i, hash, path| {
            streamed[i] = (hash, path.map(str::to_owned));
        });

        let collected: Vec<(u64, Option<String>)> = db
            .get_batch(&probes)
            .map(|(hash, path)| (hash, path.map(|p| p.into_owned())))
            .collect();
        assert_eq!(streamed, collected);
    }

    /// Debug prints shape, never entries.
    #[test]
    fn debug_prints_shape_only() {
        let mut db =
            LayeredHashDb::from_bases(vec![raw_db(&[(1, "secret/path.bin")])]).expect("one base");
        db.insert(2, "overlay/secret.bin");

        let shown = format!("{db:?}");
        assert!(shown.contains("overlay: 1"), "{shown}");
        assert!(shown.contains("entries: 1"), "{shown}");
        assert!(!shown.contains("secret"), "{shown}");
    }

    #[test]
    fn insert_path_uses_first_base() {
        let base = raw_db(&[(1, "seed")]);
        let mut db = LayeredHashDb::from_bases(vec![base]).expect("uniform bases");
        let path = "assets/characters/aatrox/aatrox.bin";
        let hash = db.insert_path(path).expect("has a base");
        assert_eq!(db.get(hash).as_deref(), Some(path));
    }
}
