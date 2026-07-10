//! `mimir publish`: build every table from a directory of CDragon
//! `hashes.*.txt` inputs into a staging directory of immutable
//! `<table>-<version>.lhdb` files plus a `manifest.json`, ready to upload as
//! GitHub release assets.
//!
//! The txt lists stay the canonical, mergeable source of truth; this
//! output is a derived artifact, rebuilt from scratch each run. Manifest,
//! sha256, and atomic installation are delegated to [`HashStore::publish`] so
//! the release staging dir and the local cache share one publish code path.

use std::fs::{self, File};
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ltk_hashdb::{Casing, Compression, HashDbWriter, HashKind, KeyWidth};
use ltk_mimir_cache::{HashStore, PublishItem, Source, Table};
use sha2::{Digest, Sha256};

use crate::read_hash_lines;

pub struct Options {
    /// Directory holding the CDragon `hashes.*.txt` inputs.
    pub inputs: PathBuf,

    /// Staging directory for the built tables + manifest.
    pub out: PathBuf,

    /// Version label for the immutable filenames; `None` → today's UTC date.
    pub version: Option<String>,

    /// Source repo of the inputs (git URL or GitHub `owner/repo`), recorded in
    /// the manifest's `source`.
    pub source_repo: String,

    /// Commit of the source repo the inputs were taken at, recorded in the
    /// manifest's `source`.
    pub source_commit: Option<String>,

    /// Skip tables whose inputs are absent instead of failing.
    pub allow_missing: bool,

    pub compression: Compression,
}

/// Which CDragon txt file feeds each table. `split` also gathers numbered
/// `<input>.<n>` parts - the data repo splits `hashes.game.txt` purely to dodge
/// GitHub's file-size limit, so parts and the unsplit file are the same list.
struct TableSpec {
    table: Table,
    key_width: KeyWidth,
    hash_kind: HashKind,
    input: &'static str,
    split: bool,
}

const fn spec(
    table: Table,
    key_width: KeyWidth,
    hash_kind: HashKind,
    input: &'static str,
    split: bool,
) -> TableSpec {
    TableSpec {
        table,
        key_width,
        hash_kind,
        input,
        split,
    }
}

/// The published table set. The legacy truncated `hashes.rst.txt` is deliberately
/// not consumed: tables store full-width hashes only; the
/// full-width `.xxh64` / `.xxh3` lists cover RST.
const SPECS: [TableSpec; 8] = [
    spec(
        Table::Game,
        KeyWidth::U64,
        HashKind::Xxh64,
        "hashes.game.txt",
        true,
    ),
    spec(
        Table::Lcu,
        KeyWidth::U64,
        HashKind::Xxh64,
        "hashes.lcu.txt",
        false,
    ),
    spec(
        Table::BinEntries,
        KeyWidth::U32,
        HashKind::Fnv1a32,
        "hashes.binentries.txt",
        false,
    ),
    spec(
        Table::BinTypes,
        KeyWidth::U32,
        HashKind::Fnv1a32,
        "hashes.bintypes.txt",
        false,
    ),
    spec(
        Table::BinFields,
        KeyWidth::U32,
        HashKind::Fnv1a32,
        "hashes.binfields.txt",
        false,
    ),
    spec(
        Table::BinHashes,
        KeyWidth::U32,
        HashKind::Fnv1a32,
        "hashes.binhashes.txt",
        false,
    ),
    spec(
        Table::Rst,
        KeyWidth::U64,
        HashKind::Xxh64,
        "hashes.rst.xxh64.txt",
        false,
    ),
    spec(
        Table::RstXxh3,
        KeyWidth::U64,
        HashKind::Xxh3,
        "hashes.rst.xxh3.txt",
        false,
    ),
];

pub fn run(opts: &Options) -> Result<()> {
    let version = opts.version.clone().unwrap_or_else(today_utc);

    // Resolve every table's input files up front so a missing input fails the run
    // before any expensive build work.
    let mut resolved: Vec<(&TableSpec, Vec<PathBuf>)> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for spec in &SPECS {
        let files = gather_inputs(&opts.inputs, spec);
        if files.is_empty() {
            missing.push(spec.input);
        } else {
            resolved.push((spec, files));
        }
    }
    if resolved.is_empty() {
        bail!("no hash lists found in {}", opts.inputs.display());
    }
    if !missing.is_empty() && !opts.allow_missing {
        bail!(
            "missing inputs in {}: {} (pass --allow-missing to skip these tables)",
            opts.inputs.display(),
            missing.join(", ")
        );
    }

    // A stale manifest would leak tables from a previous run into this release,
    // so insist on a fresh staging dir rather than silently merging.
    let manifest_path = opts.out.join("manifest.json");
    if manifest_path.exists() {
        bail!(
            "{} already exists - publish into a fresh --out directory",
            manifest_path.display()
        );
    }

    let inputs_sha256 = fingerprint_inputs(&resolved)?;

    // Build each table into a scratch subdir of the staging dir; HashStore::publish
    // copies them into place under their immutable versioned names.
    let build_dir = opts.out.join(".build");
    fs::create_dir_all(&build_dir).with_context(|| format!("creating {}", build_dir.display()))?;
    let mut items = Vec::new();
    for (spec, files) in &resolved {
        let path = build_dir.join(format!("{}.lhdb", spec.table.id()));
        let stats = build_table(spec, files, &path, opts.compression)?;
        println!(
            "{}: {} entries, arena {} B -> {} B on disk, file {} B",
            spec.table.id(),
            stats.entries,
            stats.arena_decompressed_size,
            stats.arena_compressed_size,
            stats.file_len,
        );
        items.push(PublishItem::new(spec.table, &version, path));
    }

    let source = Source {
        repo: Some(opts.source_repo.clone()),
        commit: opts.source_commit.clone(),
        inputs_sha256: Some(inputs_sha256),
    };
    let manifest = HashStore::at(&opts.out).publish(&items, Some(source))?;
    fs::remove_dir_all(&build_dir).with_context(|| format!("removing {}", build_dir.display()))?;

    println!(
        "published {} tables (version {version}) -> {}",
        manifest.tables.len(),
        manifest_path.display()
    );
    Ok(())
}

/// The input files feeding one table: the unsplit file if present, then any
/// contiguous numbered parts (`hashes.game.txt.0`, `.1`, …).
fn gather_inputs(dir: &Path, spec: &TableSpec) -> Vec<PathBuf> {
    let mut files = Vec::new();

    let single = dir.join(spec.input);
    if single.is_file() {
        files.push(single);
    }
    if spec.split {
        for n in 0.. {
            let part = dir.join(format!("{}.{n}", spec.input));
            if !part.is_file() {
                break;
            }
            files.push(part);
        }
    }

    files
}

/// One sha256 over all input files, streamed in sorted-filename order so the
/// fingerprint is independent of table iteration order.
fn fingerprint_inputs(resolved: &[(&TableSpec, Vec<PathBuf>)]) -> Result<String> {
    let mut paths: Vec<&PathBuf> = resolved.iter().flat_map(|(_, files)| files).collect();
    paths.sort_unstable_by_key(|p| p.file_name().map(|n| n.to_os_string()));

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    for path in paths {
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
    }

    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        }))
}

fn build_table(
    spec: &TableSpec,
    files: &[PathBuf],
    out: &Path,
    compression: Compression,
) -> Result<ltk_hashdb::BuildStats> {
    // Every League table hashes the lowercased path.
    let mut writer = HashDbWriter::new(spec.key_width, compression)
        .hash_kind(spec.hash_kind)
        .casing(Casing::Insensitive);
    for file in files {
        read_hash_lines(file, |hash, _, path| {
            writer.insert(hash, path);
        })?;
    }

    let out_file =
        BufWriter::new(File::create(out).with_context(|| format!("creating {}", out.display()))?);
    Ok(writer.build(out_file)?)
}

/// Today's UTC date as `YYYY-MM-DD`, formatted by hand so no `time` formatting
/// features are needed.
fn today_utc() -> String {
    let date = time::OffsetDateTime::now_utc().date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltk_hashdb::HashDb;
    use ltk_mimir_cache::Manifest;

    use tempfile::{tempdir, TempDir};

    fn options(tmp: &TempDir) -> Options {
        Options {
            inputs: tmp.path().join("inputs"),
            out: tmp.path().join("out"),
            version: Some("2026-07-09".into()),
            source_repo: "CommunityDragon/Data".into(),
            source_commit: Some("abc123".into()),
            allow_missing: true,
            compression: Compression::Zeekstd {
                frame_size: 4096,
                level: 1,
            },
        }
    }

    fn write_input(dir: &Path, name: &str, lines: &[&str]) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), lines.join("\n") + "\n").unwrap();
    }

    #[test]
    fn publishes_present_tables_with_manifest() {
        let tmp = tempdir().unwrap();
        let opts = options(&tmp);
        // Game arrives as split parts, lcu as a single file; the rest are absent.
        write_input(
            &opts.inputs,
            "hashes.game.txt.0",
            &["00000000000011aa assets/foo.bin"],
        );
        write_input(
            &opts.inputs,
            "hashes.game.txt.1",
            &["00000000000022bb assets/bar.bin"],
        );
        write_input(
            &opts.inputs,
            "hashes.lcu.txt",
            &["00000000000033cc plugins/thing.json"],
        );

        run(&opts).unwrap();

        let manifest =
            Manifest::from_slice(&fs::read(opts.out.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            manifest.tables.keys().collect::<Vec<_>>(),
            ["game", "lcu"],
            "manifest lists exactly the built tables"
        );
        let source = manifest.source.unwrap();
        assert_eq!(source.repo.as_deref(), Some("CommunityDragon/Data"));
        assert_eq!(source.commit.as_deref(), Some("abc123"));
        assert!(source.inputs_sha256.is_some());

        // Both split parts landed in the game table, under the versioned name.
        let game = HashDb::open(opts.out.join("game-2026-07-09.lhdb")).unwrap();
        assert_eq!(game.len(), 2);
        assert_eq!(game.get(0x11aa).as_deref(), Some("assets/foo.bin"));
        assert!(!opts.out.join(".build").exists(), "scratch dir cleaned up");
    }

    #[test]
    fn missing_inputs_fail_without_allow_missing() {
        let tmp = tempdir().unwrap();
        let mut opts = options(&tmp);
        opts.allow_missing = false;
        write_input(
            &opts.inputs,
            "hashes.lcu.txt",
            &["00000000000033cc plugins/thing.json"],
        );

        let err = run(&opts).unwrap_err().to_string();
        assert!(
            err.contains("hashes.game.txt"),
            "names the missing input: {err}"
        );
    }

    #[test]
    fn refuses_a_stale_staging_dir() {
        let tmp = tempdir().unwrap();
        let opts = options(&tmp);
        write_input(
            &opts.inputs,
            "hashes.lcu.txt",
            &["00000000000033cc plugins/thing.json"],
        );

        run(&opts).unwrap();
        let err = run(&opts).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");
    }
}
