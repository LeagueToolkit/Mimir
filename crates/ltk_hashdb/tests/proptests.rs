//! Property tests: arbitrary `(key, path)` sets → build → open → every key resolves,
//! every non-inserted key misses, structure validates.

use std::collections::BTreeMap;
use std::io::Cursor;

use ltk_hashdb::{Compression, HashDb, HashDbWriter, KeyWidth};
use proptest::prelude::*;

fn build_bytes(key_width: KeyWidth, entries: &BTreeMap<u64, String>) -> Vec<u8> {
    build_bytes_with(key_width, Compression::None, entries)
}

fn build_bytes_with(
    key_width: KeyWidth,
    compression: Compression,
    entries: &BTreeMap<u64, String>,
) -> Vec<u8> {
    let mut w = HashDbWriter::new(key_width, compression);
    w.extend(entries.iter().map(|(&k, p)| (k, p.as_str())));
    let mut out = Cursor::new(Vec::new());
    w.build(&mut out).expect("build");
    out.into_inner()
}

proptest! {
    #[test]
    fn u64_roundtrip(entries in prop::collection::btree_map(any::<u64>(), ".{0,60}", 0..64)) {
        let db = HashDb::open_bytes(build_bytes(KeyWidth::U64, &entries)).expect("open");
        prop_assert_eq!(db.len(), entries.len());
        for (&k, p) in &entries {
            let got = db.get(k);
            prop_assert_eq!(got.as_deref(), Some(p.as_str()));
        }
        db.verify().expect("verify");
    }

    #[test]
    fn u32_roundtrip(entries in prop::collection::btree_map(0u64..=u32::MAX as u64, ".{0,60}", 0..64)) {
        let db = HashDb::open_bytes(build_bytes(KeyWidth::U32, &entries)).expect("open");
        for (&k, p) in &entries {
            let got = db.get(k);
            prop_assert_eq!(got.as_deref(), Some(p.as_str()));
        }
        db.verify().expect("verify");
    }

    #[test]
    fn misses_miss(
        entries in prop::collection::btree_map(any::<u64>(), ".{0,20}", 0..32),
        probes in prop::collection::vec(any::<u64>(), 0..64),
    ) {
        let db = HashDb::open_bytes(build_bytes(KeyWidth::U64, &entries)).expect("open");
        for probe in probes {
            prop_assert_eq!(db.contains(probe), entries.contains_key(&probe));
            let got = db.get(probe);
            prop_assert_eq!(got.as_deref(), entries.get(&probe).map(String::as_str));
        }
    }

    #[test]
    fn compressed_roundtrip(
        entries in prop::collection::btree_map(any::<u64>(), ".{0,60}", 0..64),
        frame_size in 1u32..512,
    ) {
        let bytes = build_bytes_with(
            KeyWidth::U64,
            Compression::Zeekstd { frame_size, level: 3 },
            &entries,
        );
        let db = HashDb::open_bytes(bytes).expect("open");
        for (&k, p) in &entries {
            let got = db.get(k);
            prop_assert_eq!(got.as_deref(), Some(p.as_str()));
        }
        db.verify().expect("verify");
    }

    /// Malformed/random input must error, never panic.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = HashDb::open_bytes(bytes);
    }

    /// Random corruption of a valid file: open either errors or yields a db whose
    /// verify()/get() don't panic.
    #[test]
    fn corrupted_valid_file_never_panics(
        entries in prop::collection::btree_map(any::<u64>(), ".{0,20}", 1..16),
        flips in prop::collection::vec((any::<prop::sample::Index>(), 1u8..=255), 1..8),
        probe in any::<u64>(),
    ) {
        let mut bytes = build_bytes(KeyWidth::U64, &entries);
        for (idx, mask) in flips {
            let i = idx.index(bytes.len());
            bytes[i] ^= mask;
        }
        if let Ok(db) = HashDb::open_bytes(bytes) {
            let _ = db.get(probe);
            let _ = db.verify();
            let _ = db.iter().count();
        }
    }
}
