//! Integration tests for the shared cache: publish/open roundtrip, versioned GC, the
//! single-updater lock, and the concurrency / mapped-file guarantees.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use ltk_hashdb::{Compression, HashDbWriter, KeyWidth};
use ltk_mimir_cache::{HashStore, PublishItem, Table};

/// A self-cleaning unique temp directory (avoids a `tempfile` dependency).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "mimir-cache-test-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build a raw `.lhdb` at `path` from `entries`, returning the path.
fn build_table(path: &Path, entries: &[(u64, &str)]) -> PathBuf {
    let mut writer = HashDbWriter::new(KeyWidth::U64, Compression::None);
    for &(hash, p) in entries {
        writer.insert(hash, p);
    }
    writer.build(File::create(path).unwrap()).unwrap();
    path.to_path_buf()
}

const ENTRIES: &[(u64, &str)] = &[
    (0x1111, "assets/characters/ahri/skins/skin07/ahri.bin"),
    (0x2222, "data/characters/ahri/ahri.bin"),
    (0x3333, "assets/maps/particles/map11/thing.dds"),
];

#[test]
fn publish_open_roundtrip() {
    let tmp = TempDir::new("roundtrip");
    let store = HashStore::at(tmp.path());

    let src = build_table(&tmp.path().join("game-build.lhdb"), ENTRIES);
    let manifest = store
        .publish(&[PublishItem::new(Table::Game, "2026-07-08", &src)], None)
        .unwrap();

    // Manifest records the versioned filename + derived metadata.
    let entry = manifest.entry(Table::Game).expect("game entry");
    assert_eq!(entry.file, "game-2026-07-08.lhdb");
    assert_eq!(entry.entries, ENTRIES.len() as u64);
    assert_eq!(entry.key_width, 8);
    assert_eq!(entry.sha256.len(), 64);
    assert!(tmp.path().join(&entry.file).exists());

    // The active file opens and resolves every published hash.
    let db = store.open(Table::Game).unwrap();
    for &(hash, path) in ENTRIES {
        assert_eq!(db.get(hash).as_deref(), Some(path));
    }
    assert_eq!(db.get(0xdead_beef), None);

    // Re-reading the manifest from disk matches what publish returned.
    assert_eq!(store.manifest().unwrap(), manifest);
}

#[test]
fn publish_new_version_supersedes_and_gc_reclaims() {
    let tmp = TempDir::new("gc");
    let store = HashStore::at(tmp.path());

    let v1 = build_table(&tmp.path().join("g1.lhdb"), ENTRIES);
    store
        .publish(&[PublishItem::new(Table::Game, "v1", &v1)], None)
        .unwrap();
    let v1_file = tmp.path().join("game-v1.lhdb");
    assert!(v1_file.exists());

    // A second publish flips the pointer to a new immutable filename.
    let v2 = build_table(&tmp.path().join("g2.lhdb"), ENTRIES);
    let manifest = store
        .publish(&[PublishItem::new(Table::Game, "v2", &v2)], None)
        .unwrap();
    assert_eq!(manifest.entry(Table::Game).unwrap().file, "game-v2.lhdb");
    // Old version still on disk until GC, new one active.
    assert!(v1_file.exists());
    assert!(tmp.path().join("game-v2.lhdb").exists());

    // GC (no readers hold v1) reclaims the superseded file, keeps the active one.
    let report = store.gc().unwrap();
    assert!(report.deleted.contains(&v1_file), "v1 should be collected");
    assert!(!v1_file.exists());
    assert!(tmp.path().join("game-v2.lhdb").exists());

    // The store still opens fine afterwards.
    assert!(store.open(Table::Game).unwrap().contains(0x1111));
}

#[test]
fn gc_without_manifest_is_a_noop() {
    let tmp = TempDir::new("gc-empty");
    let store = HashStore::at(tmp.path());
    let report = store.gc().unwrap();
    assert!(report.deleted.is_empty());
    assert!(report.retained.is_empty());
}

#[test]
fn open_missing_manifest_and_table_error() {
    let tmp = TempDir::new("missing");
    let store = HashStore::at(tmp.path());

    // No manifest yet.
    assert!(matches!(
        store.open(Table::Game),
        Err(ltk_mimir_cache::Error::MissingManifest(_))
    ));

    // Manifest exists but lacks the requested table.
    let src = build_table(&tmp.path().join("g.lhdb"), ENTRIES);
    store
        .publish(&[PublishItem::new(Table::Game, "v1", &src)], None)
        .unwrap();
    assert!(matches!(
        store.open(Table::Lcu),
        Err(ltk_mimir_cache::Error::TableNotFound(Table::Lcu))
    ));
}

#[test]
fn update_lock_is_exclusive_then_reacquirable() {
    let tmp = TempDir::new("lock");
    let store = HashStore::at(tmp.path());

    let held = store.try_lock_update().unwrap();
    assert!(held.is_some(), "first acquisition succeeds");

    // A second attempt (same process, distinct file handle) is refused while held.
    assert!(
        store.try_lock_update().unwrap().is_none(),
        "lock is exclusive while held"
    );

    drop(held);
    assert!(
        store.try_lock_update().unwrap().is_some(),
        "lock is reacquirable after release"
    );
}

#[test]
fn invalid_version_is_rejected() {
    let tmp = TempDir::new("badver");
    let store = HashStore::at(tmp.path());
    let src = build_table(&tmp.path().join("g.lhdb"), ENTRIES);

    for bad in ["", "a/b", "a\\b"] {
        assert!(matches!(
            store.publish(&[PublishItem::new(Table::Game, bad, &src)], None),
            Err(ltk_mimir_cache::Error::InvalidVersion(_))
        ));
    }
    // No manifest should have been written for a rejected publish.
    assert!(store.manifest().is_err());
}

/// N reader threads hammer `open`/`get` while the main thread republishes new versions;
/// readers must never observe a torn manifest or a missing file.
#[test]
fn readers_never_break_during_publish() {
    let tmp = TempDir::new("concurrency");
    let store = Arc::new(HashStore::at(tmp.path()));

    // Seed an initial version so readers have something to open immediately.
    let seed = build_table(&tmp.path().join("seed.lhdb"), ENTRIES);
    store
        .publish(&[PublishItem::new(Table::Game, "v0", &seed)], None)
        .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));

    let readers: Vec<_> = (0..6)
        .map(|_| {
            let store = Arc::clone(&store);
            let stop = Arc::clone(&stop);
            let reads = Arc::clone(&reads);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let db = store.open(Table::Game).expect("open during publish");
                    assert_eq!(
                        db.get(0x1111).as_deref(),
                        Some("assets/characters/ahri/skins/skin07/ahri.bin"),
                    );
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    // Publish a series of new immutable versions (no GC, so old files linger for any
    // reader mid-open). The atomic manifest swap is the thing under test.
    for i in 1..=40 {
        let src = build_table(&tmp.path().join(format!("pub{i}.lhdb")), ENTRIES);
        store
            .publish(
                &[PublishItem::new(Table::Game, format!("v{i}"), &src)],
                None,
            )
            .unwrap();
    }

    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
    assert!(reads.load(Ordering::Relaxed) > 0, "readers made progress");
}

/// GC of a superseded file that a reader still has mmap'd must never error and must not
/// invalidate the held mapping. Whether the OS deletes it immediately (POSIX
/// unlink; Windows when the handle carries `FILE_SHARE_DELETE`) or refuses until the
/// mapping closes (older Windows behavior → reported as retained), the reader keeps
/// working and the active version opens.
#[test]
fn gc_handles_mapped_superseded_file() {
    let tmp = TempDir::new("mapped");
    let store = HashStore::at(tmp.path());

    let v1 = build_table(&tmp.path().join("g1.lhdb"), ENTRIES);
    store
        .publish(&[PublishItem::new(Table::Game, "v1", &v1)], None)
        .unwrap();

    // Hold a mapping of the v1 file across the publish + GC.
    let mapped = store.open(Table::Game).unwrap();
    assert!(mapped.contains(0x1111));

    let v2 = build_table(&tmp.path().join("g2.lhdb"), ENTRIES);
    store
        .publish(&[PublishItem::new(Table::Game, "v2", &v2)], None)
        .unwrap();

    let v1_file = tmp.path().join("game-v1.lhdb");
    let report = store.gc().unwrap();

    // Every v1 outcome is one of the two graceful paths, and they agree with the fs.
    let deleted = report.deleted.contains(&v1_file);
    let retained = report.retained.contains(&v1_file);
    assert!(
        deleted ^ retained,
        "v1 was either reclaimed or retained, once"
    );
    assert_eq!(v1_file.exists(), retained, "retained ⇔ still on disk");
    // The active v2 file is never touched.
    assert!(tmp.path().join("game-v2.lhdb").exists());

    // The held mapping is still usable after GC, and the active version opens.
    assert!(mapped.contains(0x2222));
    assert!(store.open(Table::Game).unwrap().contains(0x3333));
}
