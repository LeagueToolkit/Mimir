//! Integration tests for the in-process updater (`HashStore::update`):
//! fresh install, idempotence, redownload + GC, checksum rejection, the
//! single-updater lock, and forward compatibility with unknown tables.
//!
//! Not named `update.rs`: Windows UAC installer detection refuses to run a
//! test binary named `update*.exe` without elevation (os error 740).

use std::fs;
use std::path::{Path, PathBuf};

use ltk_hashdb::{Compression, HashDbWriter, HashKind, KeyWidth};
use ltk_mimir_cache::{
    CommitItem, Fetch, FetchError, HashStore, Manifest, Source, Table, UpdateError, UpdateOptions,
    UpdateOutcome, UpdateReport,
};
use tempfile::tempdir;

/// Serve "release assets" straight from a directory.
struct DirFetch(PathBuf);

impl Fetch for DirFetch {
    fn fetch(&self, filename: &str) -> Result<Vec<u8>, FetchError> {
        Ok(fs::read(self.0.join(filename))?)
    }
}

/// Build a tiny raw `.lhdb` and return its path.
fn build_table(dir: &Path, name: &str, entries: &[(u64, &str)]) -> PathBuf {
    let mut writer = HashDbWriter::new(KeyWidth::U64, Compression::None).hash_kind(HashKind::Xxh64);
    for (hash, path) in entries {
        writer.insert(*hash, path);
    }

    let path = dir.join(name);
    writer.build(fs::File::create(&path).unwrap()).unwrap();
    path
}

/// Stage a fake release (versioned `.lhdb` files + `manifest.json`) in `dir`,
/// reusing the real commit path so the layout matches CI's output.
fn make_release(dir: &Path, version: &str, tables: &[(Table, &[(u64, &str)])]) {
    let build = dir.join(".release-build");
    fs::create_dir_all(&build).unwrap();

    let items: Vec<CommitItem> = tables
        .iter()
        .map(|(table, entries)| {
            let built = build_table(&build, &format!("{}.lhdb", table.id()), entries);
            CommitItem::new(*table, version, built)
        })
        .collect();
    let source = Source {
        repo: Some("test/data".into()),
        commit: Some("deadbeef".into()),
        inputs_sha256: None,
    };
    HashStore::at(dir).commit(&items, Some(source)).unwrap();

    fs::remove_dir_all(&build).unwrap();
}

/// Unwrap a completed run's report.
fn completed(outcome: UpdateOutcome) -> UpdateReport {
    match outcome {
        UpdateOutcome::Completed(report) => report,
        UpdateOutcome::Locked => panic!("expected a completed run, got Locked"),
    }
}

#[test]
fn fresh_install_downloads_everything() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(
        &release,
        "2026-07-10",
        &[
            (Table::Game, &[(0x11aa, "assets/foo.bin")]),
            (Table::Lcu, &[(0x33cc, "plugins/thing.json")]),
        ],
    );

    // A closure is a `Fetch` too - this is the shape most consumers will use.
    let fetch =
        |filename: &str| -> Result<Vec<u8>, FetchError> { Ok(fs::read(release.join(filename))?) };
    let store = HashStore::at(&cache);
    let report = completed(store.update(&fetch, UpdateOptions::default()).unwrap());

    assert_eq!(report.installed.len(), 2);
    assert!(report.unknown_tables.is_empty());
    let db = store.open(Table::Game).unwrap();
    assert_eq!(db.get(0x11aa).as_deref(), Some("assets/foo.bin"));
    let manifest = store.manifest().unwrap();
    assert_eq!(
        manifest.source.unwrap().repo.as_deref(),
        Some("test/data"),
        "release provenance carries over into the local manifest"
    );
}

#[test]
fn second_run_is_up_to_date() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    let store = HashStore::at(&cache);
    let remote = DirFetch(release);
    assert_eq!(
        completed(store.update(&remote, UpdateOptions::default()).unwrap())
            .installed
            .len(),
        1
    );

    let rerun = completed(store.update(&remote, UpdateOptions::default()).unwrap());
    assert!(rerun.is_up_to_date());
    assert!(rerun.gc.deleted.is_empty(), "no install, no gc");

    let forced = completed(
        store
            .update(&remote, UpdateOptions { force: true })
            .unwrap(),
    );
    assert_eq!(forced.installed, [Table::Game], "force reinstalls a match");
}

#[test]
fn changed_table_redownloads_and_gc_sweeps_the_old_version() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(
        &release,
        "1",
        &[(Table::Game, &[(0x1, "a")]), (Table::Lcu, &[(0x2, "b")])],
    );

    let store = HashStore::at(&cache);
    store
        .update(&DirFetch(release.clone()), UpdateOptions::default())
        .unwrap();

    // A new release changes only the game table; lcu keeps its entry.
    make_release(&release, "2", &[(Table::Game, &[(0x1, "a"), (0x3, "c")])]);
    let report = completed(
        store
            .update(&DirFetch(release), UpdateOptions::default())
            .unwrap(),
    );

    assert_eq!(report.installed, [Table::Game]);
    let db = store.open(Table::Game).unwrap();
    assert_eq!(db.get(0x3).as_deref(), Some("c"));
    assert!(
        !cache.join("game-1.lhdb").exists(),
        "superseded version was gc'd"
    );
    assert!(cache.join("lcu-1.lhdb").exists(), "unchanged table kept");
}

#[test]
fn missing_local_file_is_reinstalled() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    let store = HashStore::at(&cache);
    store
        .update(&DirFetch(release.clone()), UpdateOptions::default())
        .unwrap();
    fs::remove_file(cache.join("game-1.lhdb")).unwrap();

    let report = completed(
        store
            .update(&DirFetch(release), UpdateOptions::default())
            .unwrap(),
    );
    assert_eq!(report.installed, [Table::Game]);
    assert!(store.open(Table::Game).is_ok());
}

#[test]
fn corrupted_download_fails_without_installing() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    // Tamper with the asset after the manifest recorded its sha256.
    let asset = release.join("game-1.lhdb");
    let mut bytes = fs::read(&asset).unwrap();
    *bytes.last_mut().unwrap() ^= 0xff;
    fs::write(&asset, bytes).unwrap();

    let store = HashStore::at(&cache);
    let err = store
        .update(&DirFetch(release), UpdateOptions::default())
        .unwrap_err();

    assert!(
        matches!(err, UpdateError::ChecksumMismatch { ref file, .. } if file == "game-1.lhdb"),
        "{err}"
    );
    assert!(
        store.manifest().is_err(),
        "nothing was installed into the cache"
    );
    let litter: Vec<_> = fs::read_dir(&cache)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(litter.is_empty(), "staged downloads were cleaned up");
}

#[test]
fn up_to_date_run_still_gcs_stray_files() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    let store = HashStore::at(&cache);
    let remote = DirFetch(release);
    store.update(&remote, UpdateOptions::default()).unwrap();

    // An unreferenced version a previous run couldn't sweep still lingers on disk.
    let stray = cache.join("game-old.lhdb");
    fs::write(&stray, b"stale").unwrap();

    // The second run installs nothing, but GC still runs and reclaims the stray - it is
    // no longer gated behind an install.
    let rerun = completed(store.update(&remote, UpdateOptions::default()).unwrap());
    assert!(rerun.is_up_to_date());
    assert!(
        rerun.gc.deleted.contains(&stray),
        "an up-to-date run still sweeps strays"
    );
    assert!(!stray.exists());
}

/// A release cut between the manifest fetch and the asset fetch: the asset returns the
/// newer bytes, which don't match the manifest we verified against. `update` errors out
/// without installing anything, and a re-run against the now-consistent release
/// converges - the documented contract for mid-publish failures.
#[test]
fn release_race_errors_and_rerun_converges() {
    let tmp = tempdir().unwrap();
    let old = tmp.path().join("old");
    let new = tmp.path().join("new");
    let cache = tmp.path().join("cache");
    // Same version label, different content - the newer release replaced the asset.
    make_release(&old, "1", &[(Table::Game, &[(0x1, "a")])]);
    make_release(&new, "1", &[(Table::Game, &[(0x2, "b")])]);

    /// Serves the old release until the first asset request, then flips to the new one -
    /// modelling `latest` advancing mid-run.
    struct RacingFetch {
        old: PathBuf,
        new: PathBuf,
        flipped: std::cell::Cell<bool>,
    }
    impl Fetch for RacingFetch {
        fn fetch(&self, filename: &str) -> Result<Vec<u8>, FetchError> {
            if filename.ends_with(".lhdb") {
                self.flipped.set(true);
            }
            let dir = if self.flipped.get() {
                &self.new
            } else {
                &self.old
            };
            Ok(fs::read(dir.join(filename))?)
        }
    }

    let store = HashStore::at(&cache);
    let remote = RacingFetch {
        old,
        new,
        flipped: std::cell::Cell::new(false),
    };

    let err = store.update(&remote, UpdateOptions::default()).unwrap_err();
    assert!(matches!(err, UpdateError::ChecksumMismatch { .. }), "{err}");
    assert!(store.manifest().is_err(), "nothing was installed");

    // `latest` has settled on the new release; a re-run sees a consistent
    // manifest + assets and succeeds.
    let report = completed(store.update(&remote, UpdateOptions::default()).unwrap());
    assert_eq!(report.installed, [Table::Game]);
    let db = store.open(Table::Game).unwrap();
    assert_eq!(
        db.get(0x2).as_deref(),
        Some("b"),
        "the re-run installed the newer release's content"
    );
}

#[test]
fn locked_cache_is_skipped() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    let store = HashStore::at(&cache);
    let _held = store.try_lock_update().unwrap().unwrap();

    assert!(matches!(
        store
            .update(&DirFetch(release), UpdateOptions::default())
            .unwrap(),
        UpdateOutcome::Locked
    ));
}

#[test]
fn unknown_remote_table_is_skipped() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    // A future mimir publishes a ninth table this build doesn't know.
    let manifest_path = release.join("manifest.json");
    let mut manifest = Manifest::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.tables.insert(
        "shiny-new".into(),
        ltk_mimir_cache::TableEntry {
            file: "shiny-new-1.lhdb".into(),
            sha256: "0".repeat(64),
            entries: 0,
            key_width: 8,
        },
    );
    manifest.write_atomic(&manifest_path).unwrap();

    let store = HashStore::at(&cache);
    let report = completed(
        store
            .update(&DirFetch(release), UpdateOptions::default())
            .unwrap(),
    );

    assert_eq!(
        report.installed,
        [Table::Game],
        "known tables install; the unknown one is skipped, not fatal"
    );
    assert_eq!(report.unknown_tables, ["shiny-new"]);
}

#[test]
fn malformed_remote_filename_is_rejected() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    // Point the game entry at a filename whose version would escape the cache dir.
    let manifest_path = release.join("manifest.json");
    let mut manifest = Manifest::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.tables.get_mut("game").unwrap().file = "game-..\\evil.lhdb".into();
    manifest.write_atomic(&manifest_path).unwrap();

    let store = HashStore::at(&cache);
    let err = store
        .update(&DirFetch(release), UpdateOptions::default())
        .unwrap_err();

    assert!(
        matches!(err, UpdateError::BadRemoteFilename { .. }),
        "{err}"
    );
    assert!(store.manifest().is_err(), "nothing was installed");
}
