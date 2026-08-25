//! Integration tests for the async in-process updater
//! (`HashStore::update_async`). The sync suite (`refresh.rs`) covers the
//! shared plan/verify/install/GC logic in depth; this one proves the async
//! driver wiring plus the async-only contracts - the future is `Send` and
//! cancel-safe.
//!
//! Not named `update_async.rs`: Windows UAC installer detection refuses to run
//! a test binary named `update*.exe` without elevation (os error 740).

use std::fs;
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Waker};

use ltk_mimir_cache::{
    AsyncFetch, FetchError, HashStore, PlannedTable, Table, UpdateError, UpdateObserver,
    UpdateOptions, UpdateOutcome,
};
use pollster::block_on;
use tempfile::tempdir;

mod common;
use common::{channel_asset, completed, edit_release_manifest, make_release, serve_asset};

/// Serve "release assets" straight from a directory.
struct DirFetch(PathBuf);

impl AsyncFetch for DirFetch {
    type Error = std::io::Error;

    async fn fetch_to(
        &self,
        filename: &str,
        sink: &mut (dyn Write + Send),
    ) -> Result<u64, FetchError<std::io::Error>> {
        serve_asset(&self.0.join(filename), sink)
    }
}

/// Compile-time check: the update future is `Send` (given a `Sync` fetcher),
/// so callers can drive it from multi-threaded executors - including while an
/// observer is watching, which the future holds across every await.
#[allow(dead_code)]
fn update_future_is_send(store: &HashStore, remote: &DirFetch) {
    struct Silent;
    impl UpdateObserver for Silent {}

    fn assert_send<T: Send>(_: T) {}
    assert_send(store.update_async(remote, UpdateOptions::default()));
    assert_send(store.update_async(remote, UpdateOptions::default().observed_by(&Silent)));
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

    // A closure is an `AsyncFetch` too - it builds owned state before the
    // `async move` block since the future cannot borrow the filename. Its
    // error type stays concrete and comes back as `UpdateError<io::Error>`.
    let fetch = |filename: &str| {
        let path = release.join(filename);
        async move { fs::read(path) }
    };
    let store = HashStore::at(&cache);
    let report = completed(block_on(store.update_async(&fetch, UpdateOptions::default())).unwrap());

    assert_eq!(report.installed.len(), 2);
    assert!(report.unknown_tables.is_empty());
    let db = store.open(Table::Game).unwrap();
    assert_eq!(db.get(0x11aa).as_deref(), Some("assets/foo.bin"));
}

/// The async driver resolves the remote manifest the same way the blocking one
/// does: this format's channel first, then the unversioned asset.
#[test]
fn the_manifest_comes_from_the_format_channel() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);
    fs::write(release.join("manifest.json"), b"not json at all").unwrap();

    let store = HashStore::at(&cache);
    let report = completed(
        block_on(store.update_async(&DirFetch(release), UpdateOptions::default())).unwrap(),
    );

    assert_eq!(report.installed, [Table::Game]);
}

#[test]
fn a_channel_less_release_still_updates() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);
    fs::remove_file(release.join(channel_asset())).unwrap();

    let store = HashStore::at(&cache);
    let report = completed(
        block_on(store.update_async(&DirFetch(release), UpdateOptions::default())).unwrap(),
    );

    assert_eq!(report.installed, [Table::Game]);
}

/// A table in a format this build cannot open is skipped here too - the async
/// driver shares `plan`, so this guards the wiring, not the decision.
#[test]
fn an_unreadable_format_is_skipped() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);
    edit_release_manifest(&release, |manifest| {
        manifest.tables.get_mut("game").unwrap().format_version = 99;
    });

    let store = HashStore::at(&cache);
    let report = completed(
        block_on(store.update_async(&DirFetch(release), UpdateOptions::default())).unwrap(),
    );

    assert!(report.installed.is_empty());
    assert_eq!(report.unsupported_tables.len(), 1);
}

/// `check_async` answers the same question `check` does, without the lock.
#[test]
fn check_async_diffs_without_installing() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    let store = HashStore::at(&cache);
    let remote = DirFetch(release);

    let report = block_on(store.check_async(&remote)).unwrap();
    assert_eq!(report.behind(), 1);
    assert!(!cache.join("manifest.json").exists());

    completed(block_on(store.update_async(&remote, UpdateOptions::default())).unwrap());
    assert!(block_on(store.check_async(&remote))
        .unwrap()
        .is_up_to_date());
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
        completed(block_on(store.update_async(&remote, UpdateOptions::default())).unwrap())
            .installed
            .len(),
        1
    );

    let rerun = completed(block_on(store.update_async(&remote, UpdateOptions::default())).unwrap());
    assert!(rerun.is_up_to_date());

    let forced = completed(
        block_on(store.update_async(&remote, UpdateOptions::default().forced())).unwrap(),
    );
    assert_eq!(forced.installed, [Table::Game], "force reinstalls a match");
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
    let err =
        block_on(store.update_async(&DirFetch(release), UpdateOptions::default())).unwrap_err();

    assert!(
        matches!(err, UpdateError::ChecksumMismatch { ref file, .. } if file == "game-1.lhdb"),
        "{err}"
    );
    assert!(
        store.manifest().is_err(),
        "nothing was installed into the cache"
    );
    assert!(no_tmp_litter(&cache), "staged downloads were cleaned up");
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
        block_on(store.update_async(&DirFetch(release), UpdateOptions::default())).unwrap(),
        UpdateOutcome::Locked
    ));
}

/// Serve assets from a directory, except the `lcu` download never resolves -
/// the update future stalls there so the test can cancel it mid-run, after
/// `game` (which sorts first in the manifest) has already been staged.
struct StallOnLcu(PathBuf);

impl AsyncFetch for StallOnLcu {
    type Error = std::io::Error;

    async fn fetch_to(
        &self,
        filename: &str,
        sink: &mut (dyn Write + Send),
    ) -> Result<u64, FetchError<std::io::Error>> {
        if filename.starts_with("lcu-") {
            std::future::pending::<()>().await;
        }

        serve_asset(&self.0.join(filename), sink)
    }
}

/// The documented cancel-safety contract: dropping the future mid-run releases
/// the update lock, removes staged downloads, and installs nothing.
#[test]
fn cancelled_update_releases_the_lock_and_cleans_staging() {
    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(
        &release,
        "1",
        &[(Table::Game, &[(0x1, "a")]), (Table::Lcu, &[(0x2, "b")])],
    );

    let store = HashStore::at(&cache);
    let remote = StallOnLcu(release);
    {
        let mut update = pin!(store.update_async(&remote, UpdateOptions::default()));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            update.as_mut().poll(&mut cx).is_pending(),
            "the run stalls on the lcu download"
        );

        // Mid-run: `game` is already staged and the single-updater lock is held.
        assert!(cache.join("game-1.lhdb.download.tmp").exists());
        assert!(store.try_lock_update().unwrap().is_none());
    } // dropping the future cancels the run

    assert!(
        store.try_lock_update().unwrap().is_some(),
        "cancellation released the update lock"
    );
    assert!(store.manifest().is_err(), "nothing was installed");
    assert!(no_tmp_litter(&cache), "staged downloads were cleaned up");
}

/// True when the cache dir holds no leftover `.tmp` files.
fn no_tmp_litter(cache: &std::path::Path) -> bool {
    fs::read_dir(cache)
        .unwrap()
        .filter_map(|e| e.ok())
        .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp"))
}

/// The async driver reports a run the same way the blocking one does: the plan
/// first, then bytes, then the table.
#[test]
fn an_observer_follows_an_async_run() {
    #[derive(Default)]
    struct Counting {
        planned: AtomicUsize,

        /// The last byte count seen, which must land on the file's size.
        done: AtomicU64,

        downloaded: AtomicUsize,
    }

    impl UpdateObserver for Counting {
        fn planned(&self, tables: &[PlannedTable]) {
            self.planned.store(tables.len(), Ordering::Relaxed);
        }

        fn progressed(&self, _table: Table, done: u64, _total: Option<u64>) {
            self.done.store(done, Ordering::Relaxed);
        }

        fn downloaded(&self, _table: Table) {
            self.downloaded.fetch_add(1, Ordering::Relaxed);
        }
    }

    let tmp = tempdir().unwrap();
    let release = tmp.path().join("release");
    let cache = tmp.path().join("cache");
    make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

    let store = HashStore::at(&cache);
    let observer = Counting::default();
    let report = completed(
        block_on(store.update_async(
            &DirFetch(release),
            UpdateOptions::default().observed_by(&observer),
        ))
        .unwrap(),
    );

    assert_eq!(report.installed, [Table::Game]);
    assert_eq!(observer.planned.load(Ordering::Relaxed), 1);
    assert_eq!(observer.downloaded.load(Ordering::Relaxed), 1);
    assert_eq!(
        observer.done.load(Ordering::Relaxed),
        fs::metadata(cache.join("game-1.lhdb")).unwrap().len(),
        "progress ends on the installed file's size"
    );
}
