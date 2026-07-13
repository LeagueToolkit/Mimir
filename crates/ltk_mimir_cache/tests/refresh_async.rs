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
use std::path::PathBuf;
use std::pin::pin;
use std::task::{Context, Waker};

use ltk_mimir_cache::{
    AsyncFetch, FetchError, HashStore, Table, UpdateError, UpdateOptions, UpdateOutcome,
};
use pollster::block_on;
use tempfile::tempdir;

mod common;
use common::{completed, make_release};

/// Serve "release assets" straight from a directory.
struct DirFetch(PathBuf);

impl AsyncFetch for DirFetch {
    async fn fetch(&self, filename: &str) -> Result<Vec<u8>, FetchError> {
        Ok(fs::read(self.0.join(filename))?)
    }
}

/// Compile-time check: the update future is `Send` (given a `Sync` fetcher),
/// so callers can drive it from multi-threaded executors.
#[allow(dead_code)]
fn update_future_is_send(store: &HashStore, remote: &DirFetch) {
    fn assert_send<T: Send>(_: T) {}
    assert_send(store.update_async(remote, UpdateOptions::default()));
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
    // `async move` block since the future cannot borrow the filename.
    let fetch = |filename: &str| {
        let path = release.join(filename);
        async move { Ok::<_, FetchError>(fs::read(path)?) }
    };
    let store = HashStore::at(&cache);
    let report = completed(block_on(store.update_async(&fetch, UpdateOptions::default())).unwrap());

    assert_eq!(report.installed.len(), 2);
    assert!(report.unknown_tables.is_empty());
    let db = store.open(Table::Game).unwrap();
    assert_eq!(db.get(0x11aa).as_deref(), Some("assets/foo.bin"));
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

    let forced =
        completed(block_on(store.update_async(&remote, UpdateOptions { force: true })).unwrap());
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
    async fn fetch(&self, filename: &str) -> Result<Vec<u8>, FetchError> {
        if filename.starts_with("lcu-") {
            std::future::pending::<()>().await;
        }
        Ok(fs::read(self.0.join(filename))?)
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
