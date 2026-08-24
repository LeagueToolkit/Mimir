//! Helpers shared by the sync (`refresh.rs`) and async (`refresh_async.rs`)
//! updater suites: building tiny tables, staging fake releases, and unwrapping
//! completed runs.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ltk_hashdb::{Compression, HashDbWriter, HashKind, KeyWidth};
use ltk_mimir_cache::{
    CommitItem, FetchError, HashStore, Manifest, Source, Table, UpdateOutcome, UpdateReport,
};

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

/// Stage a fake release (versioned `.lhdb` files + `manifest.json` + this
/// format's channel copy) in `dir`, reusing the real commit path so the layout
/// matches what the bundler uploads.
pub fn make_release(dir: &Path, version: &str, tables: &[(Table, &[(u64, &str)])]) {
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
    fs::copy(dir.join("manifest.json"), dir.join(channel_asset())).unwrap();

    fs::remove_dir_all(&build).unwrap();
}

/// The manifest asset name for the format this build reads.
pub fn channel_asset() -> String {
    Manifest::asset_for_format(ltk_hashdb::FORMAT_VERSION)
}

/// Rewrite a staged release's manifest, keeping the unversioned file and the
/// format channel in step the way the bundler does.
pub fn edit_release_manifest(dir: &Path, edit: impl FnOnce(&mut Manifest)) {
    let path = dir.join("manifest.json");
    let mut manifest = Manifest::from_slice(&fs::read(&path).unwrap()).unwrap();
    edit(&mut manifest);

    manifest.write_atomic(&path).unwrap();
    fs::copy(&path, dir.join(channel_asset())).unwrap();
}

/// Unwrap a completed run's report.
pub fn completed(outcome: UpdateOutcome) -> UpdateReport {
    match outcome {
        UpdateOutcome::Completed(report) => report,
        UpdateOutcome::Locked => panic!("expected a completed run, got Locked"),
    }
}

/// Stream one "release asset" out of a directory, the way a real fetcher must:
/// a read failure is the transport's, a write failure is the sink's.
pub fn serve_asset(
    path: &Path,
    sink: &mut (dyn Write + Send),
) -> Result<u64, FetchError<std::io::Error>> {
    let mut file = File::open(path).map_err(FetchError::Transport)?;
    // Deliberately tiny, so even the smallest fixture takes several chunks.
    let mut buf = [0u8; 1024];
    let mut total = 0;

    loop {
        let read = file.read(&mut buf).map_err(FetchError::Transport)?;
        if read == 0 {
            return Ok(total);
        }

        sink.write_all(&buf[..read]).map_err(FetchError::Sink)?;
        total += read as u64;
    }
}
