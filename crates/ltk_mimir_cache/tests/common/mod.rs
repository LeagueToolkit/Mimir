//! Helpers shared by the sync (`refresh.rs`) and async (`refresh_async.rs`)
//! updater suites: building tiny tables, staging fake releases, and unwrapping
//! completed runs.

use std::fs;
use std::path::{Path, PathBuf};

use ltk_hashdb::{Compression, HashDbWriter, HashKind, KeyWidth};
use ltk_mimir_cache::{CommitItem, HashStore, Source, Table, UpdateOutcome, UpdateReport};

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

    fs::remove_dir_all(&build).unwrap();
}

/// Unwrap a completed run's report.
pub fn completed(outcome: UpdateOutcome) -> UpdateReport {
    match outcome {
        UpdateOutcome::Completed(report) => report,
        UpdateOutcome::Locked => panic!("expected a completed run, got Locked"),
    }
}
