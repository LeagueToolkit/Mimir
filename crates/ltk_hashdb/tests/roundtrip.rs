//! Round-trip and behavioral tests: txt-shaped data → `HashDbWriter` → `HashDb`.

use std::io::Cursor;

use ltk_hashdb::{
    ArenaOrder, BuildError, Casing, Compression, HashDb, HashDbWriter, HashKind, KeyWidth,
    LayeredHashDb, OpenError, VerifyError,
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
        .casing(Casing::AsciiInsensitive);
    w.extend(entries.iter().copied());
    let mut out = Cursor::new(Vec::new());
    let stats = w.build(&mut out).expect("build");
    assert_eq!(stats.file_len, out.get_ref().len() as u64);
    out.into_inner()
}

fn build(key_width: KeyWidth, hash_kind: HashKind, entries: &[(u64, &str)]) -> Vec<u8> {
    build_with(key_width, hash_kind, Compression::None, entries)
}

/// What `build_with` produces, plus the arena-order section.
fn build_ordered(compression: Compression, entries: &[(u64, &str)]) -> Vec<u8> {
    let mut w = HashDbWriter::new(KeyWidth::U64, compression)
        .hash_kind(HashKind::Xxh64)
        .casing(Casing::AsciiInsensitive)
        .arena_order(ArenaOrder::Stored);
    w.extend(entries.iter().copied());

    let mut out = Cursor::new(Vec::new());
    let stats = w.build(&mut out).expect("build");
    assert_eq!(stats.file_len, out.get_ref().len() as u64);

    out.into_inner()
}

/// The two arena-order header fields and the entry count, straight off the wire.
fn arena_order_at(bytes: &[u8]) -> (usize, usize, usize) {
    let offset = u64::from_le_bytes(bytes[72..80].try_into().unwrap()) as usize;
    let count = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;

    (offset, bytes[15] as usize, count)
}

/// Recompute the section's trailing digest, the way a writer that meant to
/// produce these bytes would have.
fn restamp_arena_order(bytes: &mut [u8]) {
    let (offset, width, count) = arena_order_at(bytes);
    let end = offset + count * width;

    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&bytes[offset..end]);
    bytes[end..end + 8].copy_from_slice(&hasher.digest().to_le_bytes());
}

/// Every path, in the order the arena holds them.
fn values(db: &HashDb) -> Vec<String> {
    db.values().map(|p| p.into_owned()).collect()
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

/// `docs/CONSUMERS.md` promises a reader can be shared across threads (all
/// lookups take `&self`); keep this a compile-time guarantee for every public
/// one, `LayeredHashDb` included - it is the type most consumers hold.
#[test]
fn readers_are_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HashDb>();
    assert_send_sync::<LayeredHashDb>();
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

/// A raw arena lends its bytes straight out of the mapping - no copy per lookup.
#[test]
fn get_borrows_from_a_raw_arena() {
    let bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    let db = HashDb::open_bytes(bytes).expect("open");
    assert!(!db.get(1).expect("hit").is_owned());
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
            // A frame larger than the whole arena holds every entry whole, so the
            // hit lends its bytes out of the cached frame rather than copying them.
            // The tiny frame sizes above are the straddling case, which must splice.
            if frame_size == 1 << 20 {
                assert!(!db.get(k).expect("hit").is_owned());
            }
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
    assert_eq!(db.casing(), Casing::AsciiInsensitive);
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

/// The cheap tier still hashes every stored byte, so damage anywhere - arena
/// included - is caught without decompressing a thing.
#[test]
fn verify_index_catches_damage_without_decoding() {
    let good = build_with(
        KeyWidth::U64,
        HashKind::Xxh64,
        Compression::Zeekstd {
            frame_size: 128,
            level: 3,
        },
        GAME_ENTRIES,
    );
    HashDb::open_bytes(good.clone())
        .expect("open")
        .verify_index()
        .expect("a freshly built table passes");

    // A bit flipped inside a compressed frame, the way bit rot arrives. Not the
    // trailing bytes: those are the seek table, and `open` parses that already.
    let arena_offset = u64::from_le_bytes(good[40..48].try_into().unwrap()) as usize;
    let mut rotted = good.clone();
    rotted[arena_offset + 8] ^= 0xff;
    assert!(
        matches!(
            HashDb::open_bytes(rotted).expect("open").verify_index(),
            Err(VerifyError::ChecksumMismatch)
        ),
        "damage inside the arena is caught without decompressing it"
    );

    // A flipped key, which `open` has no reason to look at either.
    let keys_offset = u64::from_le_bytes(good[24..32].try_into().unwrap()) as usize;
    let mut rotted = good;
    rotted[keys_offset] ^= 0xff;
    assert!(matches!(
        HashDb::open_bytes(rotted).expect("open").verify_index(),
        Err(VerifyError::ChecksumMismatch)
    ));
}

/// Where the two tiers part company: an entry whose extent runs off the end of
/// the arena is a well-formedness problem, not damage, so only the full pass
/// finds it.
#[test]
fn verify_index_does_not_prove_entries_are_in_bounds() {
    let mut bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);

    // Point the first entry's length past the end of the arena, then re-stamp
    // the checksum the way a broken writer would have.
    let entry_count = u64::from_le_bytes(bytes[16..24].try_into().unwrap()) as usize;
    let offsets_offset = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
    let offset_width = bytes[13] as usize;
    let lengths_offset = offsets_offset + entry_count * offset_width;
    bytes[lengths_offset..lengths_offset + 2].copy_from_slice(&u16::MAX.to_le_bytes());
    restamp_checksum(&mut bytes);

    let db = HashDb::open_bytes(bytes).expect("the header and bounds still validate");
    db.verify_index()
        .expect("nothing was damaged: the bytes hash to what the header claims");
    assert!(
        matches!(db.verify(), Err(VerifyError::Malformed(_))),
        "the full pass reads the entry and finds it runs off the arena"
    );
}

/// Recompute the header checksum over the four stored sections.
fn restamp_checksum(bytes: &mut [u8]) {
    let u64_at = |i: usize| u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
    let entry_count = u64_at(16) as usize;
    let keys_offset = u64_at(24) as usize;
    let offsets_offset = u64_at(32) as usize;
    let arena_offset = u64_at(40) as usize;
    let arena_len = u64_at(56) as usize;
    let keys_len = entry_count * bytes[12] as usize;

    // keys ‖ offsets ‖ lengths ‖ arena, each as stored, padding excluded.
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&bytes[keys_offset..keys_offset + keys_len]);
    hasher.update(&bytes[offsets_offset..arena_offset]);
    hasher.update(&bytes[arena_offset..arena_offset + arena_len]);
    let digest = hasher.digest();

    bytes[64..72].copy_from_slice(&digest.to_le_bytes());
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

/// The point of splitting the flag byte: a build that predates an optional flag
/// still reads the file, and reads it as if the bit were clear.
#[test]
fn unknown_optional_flags_are_ignored() {
    let mut bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    bytes[14] = 0xff; // `opt_flags`: every bit this build has never heard of.

    let db = HashDb::open_bytes(bytes).expect("an unknown optional flag is not an error");
    let (key, path) = GAME_ENTRIES[0];
    assert_eq!(db.get(key).as_deref(), Some(path));
}

/// The other half: a required flag changes how the file must be read, so an
/// unknown one is still fatal.
#[test]
fn unknown_required_flags_rejected() {
    let mut bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    bytes[11] |= 1 << 7;

    assert!(matches!(
        HashDb::open_bytes(bytes),
        Err(OpenError::MalformedHeader(_))
    ));
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
fn overlay_shadows_a_real_base_table() {
    let bytes = build(KeyWidth::U64, HashKind::Xxh64, GAME_ENTRIES);
    let db = HashDb::open_bytes(bytes).expect("open");
    let mut layered = LayeredHashDb::from_bases(vec![db]).expect("one base");

    // Base entries still resolve.
    assert_eq!(
        layered.get(1).as_deref(),
        Some("assets/characters/aatrox/aatrox.bin")
    );

    // Overlay shadows the base.
    layered.insert(1, "overridden/path.bin");
    assert_eq!(layered.get(1).as_deref(), Some("overridden/path.bin"));

    // insert_path hashes with the first base's algorithm.
    let path = "assets/custom/mod/thing.dds";
    let h = layered.insert_path(path).expect("has a base");
    assert_eq!(h, layered.bases()[0].hash_path(path));
    assert_eq!(layered.get(h).as_deref(), Some(path));
    assert!(layered.contains(h));
    assert_eq!(layered.overlay_len(), 2);
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

/// The whole compatibility argument for adding a section without a version bump:
/// a reader built before it exists must find the file it always found. So the
/// section may only *append*, and the two header fields it fills were reserved
/// and zero in every build that shipped - including byte 64..72, the checksum,
/// which stays defined over keys‖offsets‖lengths‖arena and is covered here by
/// the head comparison.
#[test]
fn the_arena_order_section_only_appends() {
    for compression in [
        Compression::None,
        Compression::Zeekstd {
            frame_size: 128,
            level: 3,
        },
    ] {
        let plain = build_with(KeyWidth::U64, HashKind::Xxh64, compression, GAME_ENTRIES);
        let ordered = build_ordered(compression, GAME_ENTRIES);

        // Four entries fit in one byte each, plus the section's own digest.
        assert_eq!(ordered.len() - plain.len(), GAME_ENTRIES.len() + 8);

        let mut head = ordered[..plain.len()].to_vec();
        head[15] = 0;
        head[72..80].fill(0);
        assert_eq!(
            head, plain,
            "only the two reserved header fields differ before the section"
        );
    }
}

/// A stored permutation and one a reader sorts for are the same permutation, or
/// one of them is wrong. The empty path is the tie that could break differently:
/// it occupies no arena bytes, so it shares its offset with whatever was written
/// next - and `binhashes` really does contain it.
#[test]
fn a_stored_arena_order_matches_the_one_a_reader_sorts_for() {
    let entries: &[(u64, &str)] = &[
        (1, ""),
        (2, "assets/z.bin"),
        (3, "assets/a.bin"),
        (4, "assets/a.bin"),
        (5, "b"),
    ];

    for compression in [
        Compression::None,
        Compression::Zeekstd {
            frame_size: 16,
            level: 3,
        },
    ] {
        let sorted = HashDb::open_bytes(build_with(
            KeyWidth::U64,
            HashKind::Xxh64,
            compression,
            entries,
        ))
        .expect("open");
        let stored = HashDb::open_bytes(build_ordered(compression, entries)).expect("open");

        assert_eq!(sorted.arena_order_size(), None);
        assert_eq!(stored.arena_order_size(), Some(entries.len() as u64 + 8));

        let paths = values(&sorted);
        assert_eq!(paths, values(&stored));
        assert!(
            paths.windows(2).all(|w| w[0] <= w[1]),
            "the arena is in path order: {paths:?}"
        );

        let by_key = |db: &HashDb| -> Vec<(u64, String)> {
            db.iter().map(|(k, p)| (k, p.into_owned())).collect()
        };
        assert_eq!(by_key(&sorted), by_key(&stored));
    }
}

/// A prefix names a contiguous run of the arena, and `prefix` returns exactly it
/// - whether the run is read out of a stored permutation or a sorted one.
#[test]
fn prefix_returns_the_run_under_it() {
    let entries: &[(u64, &str)] = &[
        (1, "assets/characters/ahri/ahri.bin"),
        (2, "assets/characters/ahri/skins/skin01.bin"),
        (3, "assets/characters/ahriX.bin"),
        (4, "assets/characters/aatrox/aatrox.bin"),
        (5, "data/menu/main.bin"),
    ];

    for bytes in [
        build_with(KeyWidth::U64, HashKind::Xxh64, Compression::None, entries),
        build_ordered(Compression::None, entries),
    ] {
        let db = HashDb::open_bytes(bytes).expect("open");
        let hits = |prefix: &str| -> Vec<String> {
            db.prefix(prefix).map(|(_, p)| p.into_owned()).collect()
        };

        assert_eq!(
            hits("assets/characters/ahri/"),
            [
                "assets/characters/ahri/ahri.bin",
                "assets/characters/ahri/skins/skin01.bin"
            ]
        );
        // The trailing slash is what separates a directory from a sibling whose
        // name merely starts the same way.
        assert_eq!(hits("assets/characters/ahri").len(), 3);
        assert_eq!(hits("data/"), ["data/menu/main.bin"]);
        assert_eq!(hits("assets/characters/ahri/ahri.bin").len(), 1);
        assert!(hits("nothing/").is_empty());
        assert!(hits("zzz").is_empty());

        // An empty prefix is the whole table, keyed - `values` with the keys on.
        assert_eq!(hits(""), values(&db));

        // And the keys come back with the paths they belong to.
        for (key, path) in db.prefix("assets/characters/ahri/") {
            assert_eq!(db.get(key).as_deref(), Some(&*path));
        }
    }
}

/// The section is untrusted like every other: a bad one must be reported by
/// `verify_index` without decompressing anything, and must degrade reads to
/// misses rather than panicking or reading out of bounds.
#[test]
fn a_damaged_arena_order_is_caught_and_survived() {
    let good = build_ordered(Compression::None, GAME_ENTRIES);
    HashDb::open_bytes(good.clone())
        .expect("open")
        .verify_index()
        .expect("a freshly built section passes");

    let (at, ..) = arena_order_at(&good);

    // Damage, caught by the section's own digest - the header's does not cover it.
    let mut rotted = good.clone();
    rotted[at] ^= 0xff;
    assert!(matches!(
        HashDb::open_bytes(rotted).expect("open").verify_index(),
        Err(VerifyError::ChecksumMismatch)
    ));

    // A forgery that hashes correctly still has to be a permutation...
    let mut twice = good.clone();
    twice[at + 1] = twice[at];
    restamp_arena_order(&mut twice);
    assert!(matches!(
        HashDb::open_bytes(twice).expect("open").verify_index(),
        Err(VerifyError::Malformed(_))
    ));

    // ...running forward through the arena...
    let mut backwards = good.clone();
    backwards.swap(at, at + 1);
    restamp_arena_order(&mut backwards);
    assert!(matches!(
        HashDb::open_bytes(backwards).expect("open").verify_index(),
        Err(VerifyError::Malformed(_))
    ));

    // ...over entries that exist. This one also has to stay readable: a rank
    // pointing past the table reads as a miss and says the table is unhealthy.
    let mut phantom = good;
    phantom[at] = 200;
    restamp_arena_order(&mut phantom);
    let db = HashDb::open_bytes(phantom).expect("open");
    assert!(matches!(db.verify_index(), Err(VerifyError::Malformed(_))));
    assert_eq!(db.values().count(), GAME_ENTRIES.len() - 1);
    assert!(!db.is_healthy());
    assert!(
        db.get(GAME_ENTRIES[0].0).is_some(),
        "lookups are unaffected"
    );
}

/// A width too narrow to address the table cannot describe a real permutation,
/// so it is a malformed header rather than a section to ignore.
#[test]
fn an_unusable_arena_order_width_is_rejected() {
    let entries: Vec<(u64, String)> = (0..300u64).map(|i| (i, format!("p/{i:04}"))).collect();
    let borrowed: Vec<(u64, &str)> = entries.iter().map(|(k, p)| (*k, p.as_str())).collect();

    let mut bytes = build_ordered(Compression::None, &borrowed);
    assert_eq!(bytes[15], 2, "300 entries need two bytes per rank");

    bytes[15] = 1;
    assert!(matches!(
        HashDb::open_bytes(bytes),
        Err(OpenError::MalformedHeader(_))
    ));
}
