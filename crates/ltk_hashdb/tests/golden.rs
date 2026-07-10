//! Golden parity against a real CommunityDragon txt snapshot.
//!
//! Set `MIMIR_CDRAGON_DIR` to a directory holding the `hashes.*.txt` files;
//! every table is built (raw and compressed) and checked entry-for-entry
//! against the txt, including recomputing each key from its path to pin our
//! hash implementations to the corpus. Skipped when the env var is unset so
//! plain `cargo test` stays hermetic. Run in release - the game table alone
//! is ~200 MB of txt:
//!
//! ```text
//! MIMIR_CDRAGON_DIR=data/cdragon cargo test --release --test golden -- --nocapture
//! ```

use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;

use ltk_hashdb::{Casing, Compression, HashDb, HashDbWriter, HashKind, KeyWidth};

#[path = "../utils/mod.rs"]
mod utils;
use utils::splitmix64;

const TABLES: &[(&str, KeyWidth, HashKind)] = &[
    ("hashes.game.txt", KeyWidth::U64, HashKind::Xxh64),
    ("hashes.lcu.txt", KeyWidth::U64, HashKind::Xxh64),
    ("hashes.binentries.txt", KeyWidth::U32, HashKind::Fnv1a32),
    ("hashes.binfields.txt", KeyWidth::U32, HashKind::Fnv1a32),
    ("hashes.binhashes.txt", KeyWidth::U32, HashKind::Fnv1a32),
    ("hashes.bintypes.txt", KeyWidth::U32, HashKind::Fnv1a32),
    ("hashes.rst.xxh64.txt", KeyWidth::U64, HashKind::Xxh64),
    ("hashes.rst.xxh3.txt", KeyWidth::U64, HashKind::Xxh3),
];

fn data_dir() -> Option<PathBuf> {
    std::env::var_os("MIMIR_CDRAGON_DIR").map(PathBuf::from)
}

/// Parses a CDragon `<hex-hash> <path>` list. Only the line terminator is
/// stripped - paths can legitimately be empty or end in a space.
fn parse(path: &std::path::Path) -> HashMap<u64, String> {
    let text = std::fs::read_to_string(path).expect("read txt");
    let mut entries = HashMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let (hash, entry_path) = line.split_once(' ').unwrap_or_else(|| {
            panic!("{}:{}: expected `<hex-hash> <path>`", path.display(), i + 1)
        });
        let hash = u64::from_str_radix(hash, 16)
            .unwrap_or_else(|_| panic!("{}:{}: bad hex hash {hash:?}", path.display(), i + 1));
        if let Some(prev) = entries.insert(hash, entry_path.to_owned()) {
            assert_eq!(
                prev,
                entries[&hash],
                "{}: conflicting paths for {hash:#x}",
                path.display()
            );
        }
    }
    entries
}

fn build_bytes(
    key_width: KeyWidth,
    hash_kind: HashKind,
    compression: Compression,
    entries: &HashMap<u64, String>,
) -> Vec<u8> {
    let mut w = HashDbWriter::new(key_width, compression)
        .hash_kind(hash_kind)
        .casing(Casing::Insensitive);
    w.extend(entries.iter().map(|(&k, p)| (k, p.as_str())));
    let mut out = Cursor::new(Vec::new());
    w.build(&mut out).expect("build");
    out.into_inner()
}

fn check_db(db: &HashDb, entries: &HashMap<u64, String>, table: &str) {
    assert_eq!(db.len(), entries.len(), "{table}: entry count");
    db.verify()
        .unwrap_or_else(|e| panic!("{table}: verify failed: {e}"));

    // Every entry resolves - via the batch path (frame-cache friendly) …
    let mut keys: Vec<u64> = entries.keys().copied().collect();
    keys.sort_unstable();
    for (k, got) in db.get_batch(&keys) {
        assert_eq!(
            got.as_deref(),
            Some(entries[&k].as_str()),
            "{table}: get_batch({k:#x})"
        );
    }

    // … and a random sample via point lookups.
    let mut rng = 0xC0FFEEu64;
    for _ in 0..20_000.min(keys.len()) {
        let k = keys[(splitmix64(&mut rng) % keys.len() as u64) as usize];
        let got = db.get(k);
        assert_eq!(
            got.as_deref(),
            Some(entries[&k].as_str()),
            "{table}: get({k:#x})"
        );
    }

    // Random probes agree with the txt on membership (they virtually all miss).
    let key_mask = match db.key_width() {
        KeyWidth::U32 => u32::MAX as u64,
        KeyWidth::U64 => u64::MAX,
    };
    for _ in 0..100_000 {
        let probe = splitmix64(&mut rng) & key_mask;
        assert_eq!(
            db.contains(probe),
            entries.contains_key(&probe),
            "{table}: contains({probe:#x})"
        );
    }
}

#[test]
fn golden_parity() {
    let Some(dir) = data_dir() else {
        eprintln!("MIMIR_CDRAGON_DIR not set - skipping golden parity test");
        return;
    };

    for &(file, key_width, hash_kind) in TABLES {
        let entries = parse(&dir.join(file));
        assert!(!entries.is_empty(), "{file}: no entries parsed");

        // Our hashers must reproduce the corpus keys. A few corpus lines are
        // anomalous - their hash doesn't match their path (e.g. game.txt's
        // 0x10e25123126b83b0 `futures.ps4.market.dds`, confirmed against an
        // independent xxh64) - so tolerate a handful, not zero.
        let mut mismatches = 0usize;
        for (&k, p) in &entries {
            if hash_kind.hash(p, Casing::Insensitive, key_width) != k {
                mismatches += 1;
                if mismatches <= 5 {
                    eprintln!("{file}: upstream hash mismatch: {k:#x} {p:?}");
                }
            }
        }
        assert!(
            mismatches <= 5,
            "{file}: {mismatches} keys our hashers can't reproduce"
        );

        let raw = build_bytes(key_width, hash_kind, Compression::None, &entries);
        let db = HashDb::open_bytes(raw).expect("open raw");
        check_db(&db, &entries, file);

        let compressed = build_bytes(
            key_width,
            hash_kind,
            // Level 3, not the publishing default of 19: the level changes
            // bytes, not behavior, and 19 would multiply the test's runtime.
            Compression::Zeekstd {
                frame_size: 16 * 1024,
                level: 3,
            },
            &entries,
        );
        let db = HashDb::open_bytes(compressed).expect("open compressed");
        assert!(db.is_compressed());
        check_db(&db, &entries, file);

        eprintln!("{file}: {} entries ok (raw + zeekstd)", entries.len());
    }
}
