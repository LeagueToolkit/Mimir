//! Round-trip and behavioral tests: txt-shaped data → `HashDbWriter` → `HashDb`.

use std::borrow::Cow;
use std::io::Cursor;

use ltk_hashdb::{
    BuildError, Casing, Compression, ExtendedHashDb, HashDb, HashDbWriter, HashKind, KeyWidth,
    OpenError, VerifyError,
};

fn build_with(
    key_width: KeyWidth,
    hash_kind: HashKind,
    compression: Compression,
    entries: &[(u64, &str)],
) -> Vec<u8> {
    // The fixtures are League-shaped, so record the League casing rule.
    let mut w = HashDbWriter::new(key_width, compression)
        .hash_kind(hash_kind)
        .casing(Casing::Insensitive);
    w.extend(entries.iter().copied());
    let mut out = Cursor::new(Vec::new());
    let stats = w.build(&mut out).expect("build");
    assert_eq!(stats.file_len, out.get_ref().len() as u64);
    out.into_inner()
}

fn build(key_width: KeyWidth, hash_kind: HashKind, entries: &[(u64, &str)]) -> Vec<u8> {
    build_with(key_width, hash_kind, Compression::None, entries)
}

const GAME_ENTRIES: &[(u64, &str)] = &[
    (0x0000_0000_0000_0001, "assets/characters/aatrox/aatrox.bin"),
    (
        0xdead_beef_dead_beef,
        "assets/characters/ahri/skins/skin11/ahri_skin11.dds",
    ),
    (0x1234_5678_9abc_def0, "data/final/champions/zed.wad.client"),
    (
        0xffff_ffff_ffff_ffff,
        "plugins/rcp-be-lol-game-data/global/default/x.png",
    ),
];

/// `docs/CONSUMERS.md` promises a `HashDb` can be shared across threads
/// (all lookups take `&self`); keep this a compile-time guarantee.
#[test]
fn hashdb_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HashDb>();
    assert_send_sync::<ExtendedHashDb>();
}

#[test]
fn roundtrip_u64() {
    let bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    let db = HashDb::open_bytes(bytes).expect("open");

    assert_eq!(db.len(), GAME_ENTRIES.len());
    assert_eq!(db.key_width(), KeyWidth::U64);
    assert_eq!(db.hash_kind(), HashKind::Xxh64);
    for &(k, p) in GAME_ENTRIES {
        assert_eq!(db.get(k).as_deref(), Some(p));
        assert!(db.contains(k));
    }
    db.verify().expect("verify");
}

#[test]
fn roundtrip_u32() {
    let entries: &[(u64, &str)] = &[
        (0x0000_0001, "mSpellName"),
        (0xafd0_71e5, "test"),
        (0xffff_ffff, "SkinCharacterDataProperties"),
    ];
    let bytes = build(KeyWidth::U32, HashKind::Fnv1a32, entries);
    let db = HashDb::open_bytes(bytes).expect("open");
    for &(k, p) in entries {
        assert_eq!(db.get(k).as_deref(), Some(p));
    }
    // A key above u32::MAX can never be present in a u32 table.
    assert_eq!(db.get(u64::MAX), None);
    db.verify().expect("verify");
}

#[test]
fn misses_return_none() {
    let bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    let db = HashDb::open_bytes(bytes).expect("open");
    for miss in [0u64, 2, 0xdead_beef_dead_bee0, 0xfeed_face_feed_face] {
        assert_eq!(db.get(miss), None);
        assert!(!db.contains(miss));
    }
}

#[test]
fn empty_table() {
    let bytes = build(KeyWidth::U64, HashKind::Unspecified, &[]);
    let db = HashDb::open_bytes(bytes).expect("open");
    assert!(db.is_empty());
    assert_eq!(db.get(42), None);
    assert_eq!(db.iter().count(), 0);
    db.verify().expect("verify");
}

#[test]
fn get_returns_borrowed_for_raw_arena() {
    let bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    let db = HashDb::open_bytes(bytes).expect("open");
    assert!(matches!(db.get(1), Some(Cow::Borrowed(_))));
}

#[test]
fn iter_and_load_all_match() {
    let bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    let db = HashDb::open_bytes(bytes).expect("open");

    let mut from_iter: Vec<(u64, String)> = db.iter().map(|(k, s)| (k, s.into_owned())).collect();
    from_iter.sort();
    let mut expected: Vec<(u64, String)> = GAME_ENTRIES
        .iter()
        .map(|&(k, p)| (k, p.to_owned()))
        .collect();
    expected.sort();
    assert_eq!(from_iter, expected);

    let all = db.load_all();
    assert_eq!(all.len(), GAME_ENTRIES.len());
    for &(k, p) in GAME_ENTRIES {
        assert_eq!(all[&k].as_ref(), p);
    }
}

#[test]
fn get_batch_preserves_input_order() {
    let bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    let db = HashDb::open_bytes(bytes).expect("open");
    let queries = [0xdead_beef_dead_beefu64, 999, 1];
    let results: Vec<_> = db.get_batch(&queries).collect();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].0, queries[0]);
    assert!(results[0].1.is_some());
    assert_eq!(results[1], (999, None));
    assert_eq!(
        results[2].1.as_deref(),
        Some("assets/characters/aatrox/aatrox.bin")
    );
}

#[test]
fn duplicate_identical_pairs_dedup() {
    let mut w = HashDbWriter::new(KeyWidth::U64, Compression::None);
    w.insert(7, "same/path.bin");
    w.insert(7, "same/path.bin");
    let mut out = Cursor::new(Vec::new());
    let stats = w.build(&mut out).expect("build");
    assert_eq!(stats.entries, 1);
}

#[test]
fn conflicting_duplicate_key_errors() {
    let mut w = HashDbWriter::new(KeyWidth::U64, Compression::None);
    w.insert(7, "a.bin");
    w.insert(7, "b.bin");
    let err = w.build(Cursor::new(Vec::new())).unwrap_err();
    assert!(matches!(err, BuildError::DuplicateKey { key: 7 }));
}

#[test]
fn u32_table_rejects_wide_keys() {
    let mut w = HashDbWriter::new(KeyWidth::U32, Compression::None);
    w.insert(0x1_0000_0000, "too/wide.bin");
    let err = w.build(Cursor::new(Vec::new())).unwrap_err();
    assert!(matches!(err, BuildError::KeyOutOfRange { .. }));
}

#[test]
fn compressed_roundtrip() {
    // A tiny frame size forces multiple frames and entries that straddle
    // frame boundaries.
    for frame_size in [16u32, 64, 1 << 20] {
        let bytes = build_with(
            KeyWidth::U64,
            HashKind::Xxh64,
            Compression::Zeekstd {
                frame_size,
                level: 3,
            },
            GAME_ENTRIES,
        );
        let db = HashDb::open_bytes(bytes).expect("open");
        assert!(db.is_compressed());
        for &(k, p) in GAME_ENTRIES {
            assert_eq!(db.get(k).as_deref(), Some(p), "frame_size {frame_size}");
            assert!(matches!(db.get(k), Some(Cow::Owned(_))));
        }
        assert_eq!(db.get(2), None);
        db.verify().expect("verify");

        let mut collected: Vec<(u64, String)> =
            db.iter().map(|(k, s)| (k, s.into_owned())).collect();
        collected.sort();
        let mut expected: Vec<(u64, String)> = GAME_ENTRIES
            .iter()
            .map(|&(k, p)| (k, p.to_owned()))
            .collect();
        expected.sort();
        assert_eq!(collected, expected);
    }
}

#[test]
fn compressed_empty_table() {
    let bytes = build_with(
        KeyWidth::U64,
        HashKind::Unspecified,
        Compression::Zeekstd {
            frame_size: 65536,
            level: 3,
        },
        &[],
    );
    let db = HashDb::open_bytes(bytes).expect("open");
    assert!(db.is_empty());
    assert_eq!(db.get(42), None);
    db.verify().expect("verify");
}

#[test]
fn compressed_get_batch() {
    let bytes = build_with(
        KeyWidth::U64,
        HashKind::Xxh64,
        Compression::Zeekstd {
            frame_size: 32,
            level: 3,
        },
        GAME_ENTRIES,
    );
    let db = HashDb::open_bytes(bytes).expect("open");
    let queries = [0xffff_ffff_ffff_ffffu64, 5, 1, 0xdead_beef_dead_beef];
    let results: Vec<_> = db.get_batch(&queries).collect();
    assert_eq!(results.len(), 4);
    for (q, (h, _)) in queries.iter().zip(&results) {
        assert_eq!(q, h, "input order preserved");
    }
    assert!(results[0].1.is_some());
    assert!(results[1].1.is_none());
    assert_eq!(
        results[2].1.as_deref(),
        Some("assets/characters/aatrox/aatrox.bin")
    );
    assert!(results[3].1.is_some());
}

#[test]
fn compressed_corruption_detected_by_verify() {
    let mut bytes = build_with(
        KeyWidth::U64,
        HashKind::Xxh64,
        Compression::Zeekstd {
            frame_size: 32,
            level: 3,
        },
        GAME_ENTRIES,
    );
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    // Depending on where the flip lands (frame data vs seek table), open itself
    // may reject the file; if it opens, verify must catch it.
    if let Ok(db) = HashDb::open_bytes(bytes) {
        assert!(db.verify().is_err());
    }
}

#[test]
fn zero_frame_size_rejected() {
    let mut w = HashDbWriter::new(
        KeyWidth::U64,
        Compression::Zeekstd {
            frame_size: 0,
            level: 3,
        },
    );
    w.insert(1, "a");
    assert!(w.build(Cursor::new(Vec::new())).is_err());
}

#[test]
fn hash_path_uses_table_algorithm() {
    let bytes = build(KeyWidth::U32, HashKind::Fnv1a32, &[]);
    let db = HashDb::open_bytes(bytes).expect("open");
    assert_eq!(db.casing(), Casing::Insensitive);
    assert_eq!(db.hash_path("TEST"), 0xafd071e5);
}

/// The casing rule roundtrips through the header flag and drives `hash_path`.
#[test]
fn hash_path_respects_recorded_casing() {
    let w = HashDbWriter::new(KeyWidth::U32, Compression::None).hash_kind(HashKind::Fnv1a32);
    let mut out = Cursor::new(Vec::new());
    w.build(&mut out).expect("build");

    let db = HashDb::open_bytes(out.into_inner()).expect("open");
    assert_eq!(db.casing(), Casing::Sensitive);
    assert_ne!(db.hash_path("TEST"), 0xafd071e5);
    assert_eq!(db.hash_path("test"), 0xafd071e5);
}

#[test]
fn corruption_is_detected_by_verify() {
    let mut bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff; // flip a bit in the arena
    let db = HashDb::open_bytes(bytes).expect("open still succeeds (lazy)");
    assert!(matches!(db.verify(), Err(VerifyError::ChecksumMismatch)));
}

#[test]
fn truncated_file_rejected_on_open() {
    let bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    for cut in [0, 10, 79, bytes.len() - 1] {
        assert!(HashDb::open_bytes(bytes[..cut].to_vec()).is_err());
    }
}

#[test]
fn bad_magic_rejected() {
    let mut bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    bytes[0] = b'X';
    assert!(matches!(
        HashDb::open_bytes(bytes),
        Err(OpenError::BadMagic)
    ));
}

#[test]
fn extended_overlay_first_then_base() {
    let bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    let db = HashDb::open_bytes(bytes).expect("open");
    let mut ext = ExtendedHashDb::new(db);

    // Base entries still resolve.
    assert_eq!(
        ext.get(1).as_deref(),
        Some("assets/characters/aatrox/aatrox.bin")
    );

    // Overlay shadows the base.
    ext.insert(1, "overridden/path.bin");
    assert_eq!(ext.get(1).as_deref(), Some("overridden/path.bin"));

    // insert_path hashes with the base table's algorithm.
    let path = "assets/custom/mod/thing.dds";
    let h = ext.insert_path(path);
    assert_eq!(h, ext.base().hash_path(path));
    assert_eq!(ext.get(h).as_deref(), Some(path));
    assert!(ext.contains(h));
    assert_eq!(ext.overlay_len(), 2);
}

#[test]
fn identical_paths_share_arena_bytes() {
    let mut w = HashDbWriter::new(KeyWidth::U64, Compression::None);
    // Two keys mapping to the same path (as different hash algorithms of one
    // path would) are stored once in the arena.
    w.insert(1, "assets/shared/path.bin");
    w.insert(2, "assets/shared/path.bin");
    w.insert(3, "assets/other.bin");
    let mut out = Cursor::new(Vec::new());
    let stats = w.build(&mut out).expect("build");
    assert_eq!(
        stats.arena_decompressed_size,
        ("assets/shared/path.bin".len() + "assets/other.bin".len()) as u64
    );

    let db = HashDb::open_bytes(out.into_inner()).expect("open");
    assert_eq!(db.get(1).as_deref(), Some("assets/shared/path.bin"));
    assert_eq!(db.get(2).as_deref(), Some("assets/shared/path.bin"));
    assert_eq!(db.get(3).as_deref(), Some("assets/other.bin"));
    db.verify().expect("verify");
}

#[test]
fn path_longer_than_u16_rejected() {
    let mut w = HashDbWriter::new(KeyWidth::U64, Compression::None);
    let long = "x".repeat(u16::MAX as usize + 1);
    w.insert(7, &long);
    let err = w.build(&mut Cursor::new(Vec::new())).unwrap_err();
    assert!(matches!(err, BuildError::PathTooLong { key: 7, len } if len == u16::MAX as usize + 1));
}

#[test]
fn iter_yields_paths_in_lexicographic_order() {
    let bytes = build(
        KeyWidth::U64,
        HashKind::Unspecified,
        &[(50, "b/2"), (10, "c/3"), (30, "a/1")],
    );
    let db = HashDb::open_bytes(bytes).expect("open");
    let paths: Vec<String> = db.iter().map(|(_, p)| p.into_owned()).collect();
    assert_eq!(paths, ["a/1", "b/2", "c/3"]);
}
