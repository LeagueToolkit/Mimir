//! Integration tests for the in-process updater (`HashStore::update`):
//! fresh install, idempotence, redownload + GC, checksum rejection, the
//! single-updater lock, and forward compatibility with unknown tables.
//!
//! Not named `update.rs`: Windows UAC installer detection refuses to run a
//! test binary named `update*.exe` without elevation (os error 740).

use std::fs;
use std::path::PathBuf;

use ltk_mimir_cache::{
    Fetch, HashStore, Table, TableStatus, UpdateError, UpdateOptions, UpdateOutcome,
};
use tempfile::tempdir;

mod common;
use common::{channel_asset, completed, edit_release_manifest, make_release};

/// Serve "release assets" straight from a directory.
struct DirFetch(PathBuf);

impl Fetch for DirFetch {
    type Error = std::io::Error;

    fn fetch(&self, filename: &str) -> Result<Vec<u8>, std::io::Error> {
        fs::read(self.0.join(filename))
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
    // Its error type stays concrete and comes back as `UpdateError<io::Error>`.
    let fetch =
        |filename: &str| -> Result<Vec<u8>, std::io::Error> { fs::read(release.join(filename)) };
    let store = HashStore::at(&cache);
    let report = completed(store.update(&fetch, UpdateOptions::default()).unwrap());

    assert_eq!(report.installed.len(), 2);
    assert!(report.unknown_tables.is_empty());
    let db = store.open(Table::Game).unwrap();
    assert_eq!(db.get(0x11aa).as_deref(), Some("assets/foo.bin"));
    let manifest = store.manifest().unwrap();
    let entry = manifest.entry(Table::Game).unwrap();
    assert_eq!(
        (entry.version.as_str(), entry.format_version),
        ("2026-07-10", ltk_hashdb::FORMAT_VERSION),
        "a consumer can name the version without parsing {:?}",
        entry.file
    );
    assert!(
        manifest.generated_at_time().is_some(),
        "and can tell how stale it is: {:?}",
        manifest.generated_at
    );
    assert_eq!(
        entry.source.as_ref().unwrap().repo.as_deref(),
        Some("test/data"),
        "release provenance travels with the table it describes"
    );
    assert_eq!(
        manifest.last_run.unwrap().repo.as_deref(),
        Some("test/data"),
        "and the run record says what this update drew on"
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
        type Error = std::io::Error;

        fn fetch(&self, filename: &str) -> Result<Vec<u8>, std::io::Error> {
            if filename.ends_with(".lhdb") {
                self.flipped.set(true);
            }
            let dir = if self.flipped.get() {
                &self.new
            } else {
                &self.old
            };
            fs::read(dir.join(filename))
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
    edit_release_manifest(&release, |manifest| {
        manifest.tables.insert(
            "shiny-new".into(),
            ltk_mimir_cache::TableEntry {
                file: "shiny-new-1.lhdb".into(),
                sha256: "0".repeat(64),
                entries: 0,
                key_width: 8,
                version: "1".into(),
                source: None,
                format_version: ltk_hashdb::FORMAT_VERSION,
            },
        );
    });

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
    edit_release_manifest(&release, |manifest| {
        manifest.tables.get_mut("game").unwrap().file = "game-..\\evil.lhdb".into();
    });

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

/// A build asks for the manifest describing the format it can read, so a
/// release that has moved on still hands it a table set it understands.
#[test]
fn the_manifest_comes_from_the_format_channel() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    // Stand in for a release whose unversioned manifest has moved to a format
    // this build knows nothing about: if it were the one read, nothing works.
    fs::write(release.join("manifest.json"), b"not json at all").unwrap();

    let store = HashStore::at(&cache);
    let report = completed(
        store
            .update(&DirFetch(release), UpdateOptions::default())
            .unwrap(),
    );

    assert_eq!(report.installed, [Table::Game]);
}

/// Releases published before channels existed carry only `manifest.json`.
#[test]
fn a_channel_less_release_still_updates() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);
    fs::remove_file(release.join(channel_asset())).unwrap();

    let store = HashStore::at(&cache);
    let report = completed(
        store
            .update(&DirFetch(release), UpdateOptions::default())
            .unwrap(),
    );

    assert_eq!(report.installed, [Table::Game]);
}

/// A release neither file nor channel can supply is still reported against the
/// channel, since that is the request that should have worked.
#[test]
fn a_missing_manifest_names_the_channel() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);
    fs::remove_file(release.join(channel_asset())).unwrap();
    fs::remove_file(release.join("manifest.json")).unwrap();

    let store = HashStore::at(&cache);
    let err = store
        .update(&DirFetch(release), UpdateOptions::default())
        .unwrap_err();

    match err {
        UpdateError::Fetch { file, .. } => assert_eq!(file, channel_asset()),
        other => panic!("expected a fetch error, got {other}"),
    }
}

/// The forward-compatibility contract in one run: a manifest from a newer tool
/// carrying a higher schema, a field this build has never seen, a table it has
/// no id for, and a table in a format it cannot open still installs everything
/// it does understand.
#[test]
fn a_manifest_from_the_future_installs_what_it_can() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(
        &release,
        "1",
        &[
            (Table::Game, &[(0x1, "assets/foo.bin")]),
            (Table::Lcu, &[(0x2, "plugins/thing.json")]),
        ],
    );

    let channel = release.join(channel_asset());
    let mut doc: serde_json::Value = serde_json::from_slice(&fs::read(&channel).unwrap()).unwrap();
    let root = doc.as_object_mut().unwrap();
    root.insert("schema".into(), 99.into());
    root.insert(
        "mirrors".into(),
        serde_json::json!(["https://example.invalid"]),
    );

    let tables = root.get_mut("tables").unwrap().as_object_mut().unwrap();
    tables.insert(
        "shiny-new".into(),
        serde_json::json!({
            "file": "shiny-new-1.lhdb",
            "sha256": "0".repeat(64),
            "entries": 0,
            "key_width": 8,
            "format_version": 1,
        }),
    );
    // `lcu` moved to a format version this build cannot open.
    tables
        .get_mut("lcu")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("format_version".into(), 99.into());
    fs::write(&channel, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();

    let store = HashStore::at(&cache);
    let report = completed(
        store
            .update(&DirFetch(release), UpdateOptions::default())
            .unwrap(),
    );

    assert_eq!(report.installed, [Table::Game], "the readable table lands");
    assert_eq!(report.unknown_tables, ["shiny-new"]);
    assert_eq!(
        report.unsupported_tables,
        [ltk_mimir_cache::UnsupportedTable {
            table: Table::Lcu,
            format_version: 99,
        }]
    );
    assert!(
        store.open(Table::Lcu).is_err(),
        "a table this build cannot read is never installed"
    );
}

/// The unreadable table is skipped, not overwritten: whatever the cache already
/// holds keeps being served.
#[test]
fn an_unreadable_new_format_leaves_the_installed_table_alone() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "old")])]);

    let store = HashStore::at(&cache);
    completed(
        store
            .update(&DirFetch(release.clone()), UpdateOptions::default())
            .unwrap(),
    );

    // The next release ships `game` in a format this build cannot open.
    make_release(&release, "2", &[(Table::Game, &[(0x1, "new")])]);
    edit_release_manifest(&release, |manifest| {
        manifest.tables.get_mut("game").unwrap().format_version = 99;
    });

    let report = completed(
        store
            .update(&DirFetch(release), UpdateOptions::default())
            .unwrap(),
    );

    assert!(report.installed.is_empty());
    assert_eq!(report.unsupported_tables.len(), 1);
    assert_eq!(
        store.open(Table::Game).unwrap().get(0x1).as_deref(),
        Some("old"),
        "the version the cache can read is still the one it serves"
    );
}

/// The status a table has before and after an update, and the count a UI puts
/// in front of a user.
#[test]
fn check_diffs_the_cache_against_the_release() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(
        &release,
        "2026-07-10",
        &[(Table::Game, &[(0x1, "a")]), (Table::Lcu, &[(0x2, "b")])],
    );

    let store = HashStore::at(&cache);
    let remote = DirFetch(release.clone());

    let report = store.check(&remote).unwrap();
    assert_eq!(report.behind(), 2, "an empty cache is behind on everything");
    assert!(report
        .tables
        .iter()
        .all(|diff| diff.status == TableStatus::Absent));
    assert!(
        !cache.join("manifest.json").exists(),
        "check installs nothing"
    );

    completed(store.update(&remote, UpdateOptions::default()).unwrap());
    let report = store.check(&remote).unwrap();
    assert!(report.is_up_to_date());
    assert_eq!(report.behind(), 0);

    // The release moves on for one table only.
    make_release(&release, "2026-07-17", &[(Table::Game, &[(0x1, "a2")])]);
    let report = store.check(&DirFetch(release)).unwrap();
    assert_eq!(report.behind(), 1);

    let game = report
        .tables
        .iter()
        .find(|diff| diff.table == Table::Game)
        .unwrap();
    assert_eq!(game.status, TableStatus::Stale);
    assert_eq!(game.remote.version, "2026-07-17");
    assert_eq!(
        game.local.as_ref().unwrap().version,
        "2026-07-10",
        "both sides are named, so a UI can say what it would replace"
    );
}

/// The point of the lock-free path: a status read works while someone else is
/// midway through an update.
#[test]
fn check_does_not_take_the_update_lock() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    let store = HashStore::at(&cache);
    let _held = store
        .try_lock_update()
        .unwrap()
        .expect("nobody else holds it");

    let report = store.check(&DirFetch(release)).unwrap();
    assert_eq!(report.behind(), 1);
}

/// A file that vanished under the manifest reads as reinstallable, not as
/// current - the same test `update` makes before skipping a table.
#[test]
fn check_notices_a_missing_file() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    let store = HashStore::at(&cache);
    let remote = DirFetch(release);
    completed(store.update(&remote, UpdateOptions::default()).unwrap());
    fs::remove_file(cache.join("game-1.lhdb")).unwrap();

    let report = store.check(&remote).unwrap();
    assert_eq!(report.tables[0].status, TableStatus::FileMissing);
    assert_eq!(report.behind(), 1);
}

/// A table this build cannot open is reported, but it is not something an
/// update could fix - so it does not count as being behind.
#[test]
fn check_reports_an_unreadable_format_without_counting_it() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);
    edit_release_manifest(&release, |manifest| {
        manifest.tables.get_mut("game").unwrap().format_version = 99;
    });

    let report = HashStore::at(&cache).check(&DirFetch(release)).unwrap();
    assert_eq!(report.tables[0].status, TableStatus::Unsupported);
    assert!(report.is_up_to_date(), "no update would change this");
}
