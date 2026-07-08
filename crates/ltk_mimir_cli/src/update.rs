//! `mimir update`: bring the shared cache up to date with the latest published
//! tables.
//!
//! Tables are built by CI from the canonical txt lists and shipped as GitHub
//! release assets, so updating a machine is a download, not a rebuild: fetch
//! the release `manifest.json`, keep every table whose sha256 already matches
//! the local manifest, download the rest, verify each checksum, and install
//! atomically via [`HashStore::publish`] + [`HashStore::gc`] under the
//! single-updater lock. Readers keep resolving throughout - they see either
//! the whole old version or the whole new one.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use ltk_mimir_cache::{Error as CacheError, HashStore, Manifest, PublishItem, Table};
use sha2::{Digest, Sha256};

pub struct Options {
    /// GitHub `owner/repo` whose latest release ships the tables.
    pub repo: String,

    /// Explicit base URL serving `manifest.json` + the `.lhdb` assets (a
    /// mirror); overrides `repo`.
    pub url: Option<String>,

    /// Explicit cache directory; `None` resolves the shared cache.
    pub dir: Option<PathBuf>,

    /// Reinstall every table even when the local copy already matches.
    pub force: bool,
}

/// What an update run did.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Another process holds the update lock; leave the work to it.
    Locked,

    /// Every remote table already matches the local cache.
    UpToDate,

    /// This many tables were downloaded and installed.
    Updated(usize),
}

pub fn run(opts: &Options) -> Result<()> {
    let base = match &opts.url {
        Some(url) => url.trim_end_matches('/').to_owned(),
        None => format!("https://github.com/{}/releases/latest/download", opts.repo),
    };
    let store = match &opts.dir {
        Some(dir) => HashStore::at(dir),
        None => HashStore::discover()?,
    };
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("mimir/", env!("CARGO_PKG_VERSION")))
        .build()?;

    match update(&store, &HttpFetch { base, client }, opts.force)? {
        Outcome::Locked => println!(
            "another process is already updating {} - nothing to do",
            store.dir().display()
        ),
        Outcome::UpToDate => println!("up to date"),
        Outcome::Updated(n) => println!("updated {n} table(s) in {}", store.dir().display()),
    }
    Ok(())
}

/// Fetch one release asset by filename. The single indirection between the
/// update flow and GitHub, so tests can serve a release from a directory.
trait Fetch {
    fn fetch(&self, filename: &str) -> Result<Vec<u8>>;
}

struct HttpFetch {
    base: String,
    client: reqwest::blocking::Client,
}

impl Fetch for HttpFetch {
    fn fetch(&self, filename: &str) -> Result<Vec<u8>> {
        let url = format!("{}/{filename}", self.base);
        let response = self
            .client
            .get(&url)
            .send()
            .with_context(|| format!("GET {url}"))?;
        if !response.status().is_success() {
            bail!("GET {url}: HTTP {}", response.status());
        }

        Ok(response
            .bytes()
            .with_context(|| format!("reading {url}"))?
            .to_vec())
    }
}

/// Downloaded-but-not-yet-installed files, removed on drop so neither success
/// nor failure litters the cache dir (`gc` would sweep the `.tmp` suffix
/// eventually anyway).
struct Staged(Vec<PathBuf>);

impl Drop for Staged {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

fn update(store: &HashStore, remote: &dyn Fetch, force: bool) -> Result<Outcome> {
    let Some(_lock) = store.try_lock_update()? else {
        return Ok(Outcome::Locked);
    };

    let remote_manifest = Manifest::from_slice(
        &remote
            .fetch("manifest.json")
            .context("fetching the release manifest")?,
    )?;
    let local = match store.manifest() {
        Ok(manifest) => Some(manifest),
        Err(CacheError::MissingManifest(_)) => None,
        Err(e) => return Err(e.into()),
    };

    // Stage a verified download for every table whose content differs from what
    // the local manifest points at (or whose file has gone missing on disk).
    let mut items = Vec::new();
    let mut staged = Staged(Vec::new());
    for (id, entry) in &remote_manifest.tables {
        let Some(table) = Table::from_id(id) else {
            eprintln!("{id}: unknown table - skipped (newer mimir release?)");
            continue;
        };
        let version = version_of(table, &entry.file).with_context(|| {
            format!(
                "{id}: malformed table filename {:?} in the release manifest",
                entry.file
            )
        })?;

        let current = local.as_ref().and_then(|m| m.entry(table));
        let fresh = current
            .is_some_and(|c| c.sha256 == entry.sha256 && store.dir().join(&c.file).is_file());
        if fresh && !force {
            continue;
        }

        println!("{id}: downloading {}", entry.file);
        let bytes = remote
            .fetch(&entry.file)
            .with_context(|| format!("downloading {}", entry.file))?;
        let sha256 = hex_sha256(&bytes);
        if sha256 != entry.sha256 {
            bail!(
                "{}: sha256 mismatch (manifest {}, downloaded {sha256})",
                entry.file,
                entry.sha256
            );
        }

        let tmp = store.dir().join(format!("{}.download.tmp", entry.file));
        fs::write(&tmp, &bytes).with_context(|| format!("staging {}", tmp.display()))?;
        items.push(PublishItem::new(table, version, &tmp));
        staged.0.push(tmp);
    }
    if items.is_empty() {
        return Ok(Outcome::UpToDate);
    }

    // Install atomically - table files first, manifest pointer last - then
    // sweep the versions nothing references anymore.
    store.publish(&items, remote_manifest.source.clone())?;
    let gc = store.gc()?;
    if !gc.deleted.is_empty() {
        println!("gc: removed {} superseded file(s)", gc.deleted.len());
    }
    if !gc.retained.is_empty() {
        println!(
            "gc: {} superseded file(s) still in use - will retry next update",
            gc.retained.len()
        );
    }

    Ok(Outcome::Updated(items.len()))
}

/// The version label embedded in a release filename (`<id>-<version>.lhdb`).
/// Rejects anything that is not a single clean path component: the manifest is
/// remote input, and the filename is reused locally.
fn version_of(table: Table, file: &str) -> Option<&str> {
    let version = file
        .strip_prefix(table.id())?
        .strip_prefix('-')?
        .strip_suffix(".lhdb")?;

    (!version.is_empty() && !version.contains(['/', '\\'])).then_some(version)
}

fn hex_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use ltk_hashdb::{Compression, HashDbWriter, HashKind, KeyWidth};
    use ltk_mimir_cache::Source;

    use crate::testutil::TempDir;

    /// Serve "release assets" straight from a directory.
    struct DirFetch(PathBuf);

    impl Fetch for DirFetch {
        fn fetch(&self, filename: &str) -> Result<Vec<u8>> {
            Ok(fs::read(self.0.join(filename))?)
        }
    }

    /// Build a tiny raw `.lhdb` and return its path.
    fn build_table(dir: &Path, name: &str, entries: &[(u64, &str)]) -> PathBuf {
        let mut writer =
            HashDbWriter::new(KeyWidth::U64, Compression::None).hash_kind(HashKind::Xxh64Lower);
        for (hash, path) in entries {
            writer.insert(*hash, path);
        }

        let path = dir.join(name);
        writer.build(fs::File::create(&path).unwrap()).unwrap();
        path
    }

    /// Stage a fake release (versioned `.lhdb` files + `manifest.json`) in `dir`,
    /// reusing the real publish path so the layout matches CI's output.
    fn make_release(dir: &Path, version: &str, tables: &[(Table, &[(u64, &str)])]) {
        let build = dir.join(".release-build");
        fs::create_dir_all(&build).unwrap();

        let items: Vec<PublishItem> = tables
            .iter()
            .map(|(table, entries)| {
                let built = build_table(&build, &format!("{}.lhdb", table.id()), entries);
                PublishItem::new(*table, version, built)
            })
            .collect();
        let source = Source {
            repo: Some("test/data".into()),
            commit: Some("deadbeef".into()),
            inputs_sha256: None,
        };
        HashStore::at(dir).publish(&items, Some(source)).unwrap();

        fs::remove_dir_all(&build).unwrap();
    }

    #[test]
    fn fresh_install_downloads_everything() {
        let tmp = TempDir::new("update-fresh");
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

        let store = HashStore::at(&cache);
        let outcome = update(&store, &DirFetch(release), false).unwrap();

        assert_eq!(outcome, Outcome::Updated(2));
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
        let tmp = TempDir::new("update-idempotent");
        let release = tmp.path().join("release");
        let cache = tmp.path().join("cache");
        make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

        let store = HashStore::at(&cache);
        assert_eq!(
            update(&store, &DirFetch(release.clone()), false).unwrap(),
            Outcome::Updated(1)
        );
        assert_eq!(
            update(&store, &DirFetch(release.clone()), false).unwrap(),
            Outcome::UpToDate
        );
        assert_eq!(
            update(&store, &DirFetch(release), true).unwrap(),
            Outcome::Updated(1),
            "--force reinstalls a matching table"
        );
    }

    #[test]
    fn changed_table_redownloads_and_gc_sweeps_the_old_version() {
        let tmp = TempDir::new("update-changed");
        let release = tmp.path().join("release");
        let cache = tmp.path().join("cache");
        make_release(
            &release,
            "1",
            &[(Table::Game, &[(0x1, "a")]), (Table::Lcu, &[(0x2, "b")])],
        );

        let store = HashStore::at(&cache);
        update(&store, &DirFetch(release.clone()), false).unwrap();

        // A new release changes only the game table; lcu keeps its entry.
        make_release(&release, "2", &[(Table::Game, &[(0x1, "a"), (0x3, "c")])]);
        let outcome = update(&store, &DirFetch(release), false).unwrap();

        assert_eq!(outcome, Outcome::Updated(1));
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
        let tmp = TempDir::new("update-missing-file");
        let release = tmp.path().join("release");
        let cache = tmp.path().join("cache");
        make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

        let store = HashStore::at(&cache);
        update(&store, &DirFetch(release.clone()), false).unwrap();
        fs::remove_file(cache.join("game-1.lhdb")).unwrap();

        assert_eq!(
            update(&store, &DirFetch(release), false).unwrap(),
            Outcome::Updated(1)
        );
        assert!(store.open(Table::Game).is_ok());
    }

    #[test]
    fn corrupted_download_fails_without_installing() {
        let tmp = TempDir::new("update-corrupt");
        let release = tmp.path().join("release");
        let cache = tmp.path().join("cache");
        make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

        // Tamper with the asset after the manifest recorded its sha256.
        let asset = release.join("game-1.lhdb");
        let mut bytes = fs::read(&asset).unwrap();
        *bytes.last_mut().unwrap() ^= 0xff;
        fs::write(&asset, bytes).unwrap();

        let store = HashStore::at(&cache);
        let err = update(&store, &DirFetch(release), false)
            .unwrap_err()
            .to_string();

        assert!(err.contains("sha256 mismatch"), "{err}");
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
    fn locked_cache_is_skipped() {
        let tmp = TempDir::new("update-locked");
        let release = tmp.path().join("release");
        let cache = tmp.path().join("cache");
        make_release(&release, "1", &[(Table::Game, &[(0x1, "a")])]);

        let store = HashStore::at(&cache);
        let _held = store.try_lock_update().unwrap().unwrap();

        assert_eq!(
            update(&store, &DirFetch(release), false).unwrap(),
            Outcome::Locked
        );
    }

    #[test]
    fn unknown_remote_table_is_skipped() {
        let tmp = TempDir::new("update-unknown-table");
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
        assert_eq!(
            update(&store, &DirFetch(release), false).unwrap(),
            Outcome::Updated(1),
            "known tables install; the unknown one is skipped, not fatal"
        );
    }

    #[test]
    fn version_of_rejects_escapes() {
        assert_eq!(
            version_of(Table::Game, "game-2026-07-10.lhdb"),
            Some("2026-07-10")
        );
        assert_eq!(
            version_of(Table::RstXxh3, "rst-xxh3-2026-07-10.lhdb"),
            Some("2026-07-10")
        );
        assert_eq!(version_of(Table::Game, "lcu-1.lhdb"), None);
        assert_eq!(version_of(Table::Game, "game-.lhdb"), None);
        assert_eq!(version_of(Table::Game, "game-..\\evil.lhdb"), None);
        assert_eq!(version_of(Table::Game, "game-a/b.lhdb"), None);
    }
}
