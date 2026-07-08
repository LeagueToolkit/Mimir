//! Real-data benchmark: builds every CDragon table (raw + zeekstd), sweeps
//! the zeekstd frame size on the largest u64/u32 tables, and prints the
//! markdown tables behind `docs/BENCHMARKS.md`.
//!
//! ```text
//! cargo run --release -p ltk_hashdb --example bench_real -- data/cdragon data/build
//! ```
//!
//! Built `.hashdb` files are left in the output directory so they can be
//! reused (CLI verify/stats, criterion, manual poking). The measurements are
//! also written to `<out_dir>/bench_real.json`, which the `gen_charts`
//! example turns into the README charts.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ltk_hashdb::{Compression, HashDb, HashDbWriter, HashKind, KeyWidth};

#[path = "../utils/mod.rs"]
mod utils;
use utils::{fmt_ns, group_thousands, mib, parse, splitmix64};
use utils::{BenchReport, Sweep, SweepRow, TableBuild};

const TABLES: &[(&str, &str, KeyWidth, HashKind)] = &[
    (
        "game",
        "hashes.game.txt",
        KeyWidth::U64,
        HashKind::Xxh64Lower,
    ),
    ("lcu", "hashes.lcu.txt", KeyWidth::U64, HashKind::Xxh64Lower),
    (
        "binentries",
        "hashes.binentries.txt",
        KeyWidth::U32,
        HashKind::Fnv1a32Lower,
    ),
    (
        "binfields",
        "hashes.binfields.txt",
        KeyWidth::U32,
        HashKind::Fnv1a32Lower,
    ),
    (
        "binhashes",
        "hashes.binhashes.txt",
        KeyWidth::U32,
        HashKind::Fnv1a32Lower,
    ),
    (
        "bintypes",
        "hashes.bintypes.txt",
        KeyWidth::U32,
        HashKind::Fnv1a32Lower,
    ),
    (
        "rst.xxh64",
        "hashes.rst.xxh64.txt",
        KeyWidth::U64,
        HashKind::Xxh64Lower,
    ),
    (
        "rst.xxh3",
        "hashes.rst.xxh3.txt",
        KeyWidth::U64,
        HashKind::Xxh3Lower,
    ),
];

const SWEEP_TABLES: &[&str] = &["game", "binentries"];
const SWEEP_FRAME_SIZES: &[u32] = &[
    4 << 10,
    8 << 10,
    16 << 10,
    32 << 10,
    64 << 10,
    128 << 10,
    256 << 10,
];
const DEFAULT_FRAME_SIZE: u32 = 16 << 10;
const DEFAULT_LEVEL: i32 = 19;

fn main() {
    let mut args = std::env::args().skip(1);
    let data_dir = PathBuf::from(args.next().unwrap_or_else(|| "data/cdragon".into()));
    let out_dir = PathBuf::from(args.next().unwrap_or_else(|| "data/build".into()));
    std::fs::create_dir_all(&out_dir).expect("create out dir");

    let mut report = BenchReport {
        tables: Vec::new(),
        sweeps: Vec::new(),
    };

    println!(
        "## Table builds (zeekstd frame size = {} KiB, level {})\n",
        DEFAULT_FRAME_SIZE >> 10,
        DEFAULT_LEVEL,
    );
    println!("| table | entries | txt | raw `.hashdb` | zstd `.hashdb` | vs txt | build |");
    println!("|-------|--------:|----:|-------------:|--------------:|-------:|------:|");
    for &(name, file, key_width, hash_kind) in TABLES {
        let txt_path = data_dir.join(file);
        let txt_len = std::fs::metadata(&txt_path).expect("txt metadata").len();
        let entries = parse(&txt_path);

        let raw_path = out_dir.join(format!("{name}.raw.hashdb"));
        let (raw_len, _) = build(&entries, key_width, hash_kind, Compression::None, &raw_path);

        let zstd_path = out_dir.join(format!("{name}.hashdb"));
        let start = Instant::now();
        let (zstd_len, _) = build(
            &entries,
            key_width,
            hash_kind,
            Compression::Zeekstd {
                frame_size: DEFAULT_FRAME_SIZE,
                level: DEFAULT_LEVEL,
            },
            &zstd_path,
        );
        let build_time = start.elapsed();

        println!(
            "| {name} | {} | {} | {} | {} | {:.1}% | {:.1}s |",
            group_thousands(entries.len() as u64),
            mib(txt_len),
            mib(raw_len),
            mib(zstd_len),
            100.0 * zstd_len as f64 / txt_len as f64,
            build_time.as_secs_f64(),
        );

        report.tables.push(TableBuild {
            table: name.to_owned(),
            entries: entries.len() as u64,
            txt_len,
            raw_len,
            zstd_len,
            build_secs: build_time.as_secs_f64(),
        });
    }

    for &sweep in SWEEP_TABLES {
        let &(name, file, key_width, hash_kind) =
            TABLES.iter().find(|t| t.0 == sweep).expect("sweep table");
        let entries = parse(&data_dir.join(file));
        let keys: Vec<u64> = {
            let mut k: Vec<u64> = entries.iter().map(|&(k, _)| k).collect();
            k.sort_unstable();
            k
        };

        println!(
            "\n## Frame-size sweep - `{name}` ({} entries)\n",
            group_thousands(keys.len() as u64)
        );
        println!("| frame | file | open+1st hit | hit | batch hit | miss | full iter |");
        println!("|-------|-----:|-------------:|----:|----------:|-----:|----------:|");

        let raw_path = out_dir.join(format!("{name}.raw.hashdb"));
        let (raw_len, _) = build(&entries, key_width, hash_kind, Compression::None, &raw_path);
        let mut rows = vec![bench_row(None, raw_len, &raw_path, &keys)];

        for &frame_size in SWEEP_FRAME_SIZES {
            let path = out_dir.join(format!("{name}.f{}k.hashdb", frame_size >> 10));
            let (len, _) = build(
                &entries,
                key_width,
                hash_kind,
                Compression::Zeekstd {
                    frame_size,
                    level: DEFAULT_LEVEL,
                },
                &path,
            );
            rows.push(bench_row(Some(frame_size), len, &path, &keys));
        }

        report.sweeps.push(Sweep {
            table: name.to_owned(),
            entries: keys.len() as u64,
            rows,
        });
    }

    let json_path = out_dir.join("bench_real.json");
    let json = BufWriter::new(File::create(&json_path).expect("create json"));
    serde_json::to_writer_pretty(json, &report).expect("write json");
    println!("\nwrote {}", json_path.display());
}

/// One benchmark row: open+first-hit, random point hits, batched hits,
/// misses, and a full iteration. Prints the markdown row and returns the
/// measurements for the JSON report.
fn bench_row(frame_size: Option<u32>, file_len: u64, path: &Path, keys: &[u64]) -> SweepRow {
    let mut rng = 0x5EEDu64;

    // Open + first lookup (page cache warm - this measures parse + setup,
    // not disk).
    let start = Instant::now();
    let db = HashDb::open(path).expect("open");
    let first = keys[(splitmix64(&mut rng) % keys.len() as u64) as usize];
    assert!(db.get(first).is_some());
    let open_first = start.elapsed();

    // Random point hits.
    let n_hits = 20_000.min(keys.len());
    let sample: Vec<u64> = (0..n_hits)
        .map(|_| keys[(splitmix64(&mut rng) % keys.len() as u64) as usize])
        .collect();
    let start = Instant::now();
    let mut sink = 0usize;
    for &k in &sample {
        sink += db.get(k).map_or(0, |p| p.len());
    }
    let hit = start.elapsed() / n_hits as u32;

    // Same sample via get_batch (arena-ordered, frame cache).
    let start = Instant::now();
    sink += db
        .get_batch(&sample)
        .map(|(_, p)| p.map_or(0, |p| p.len()))
        .sum::<usize>();
    let batch_hit = start.elapsed() / n_hits as u32;

    // Misses.
    let key_mask = match db.key_width() {
        KeyWidth::U32 => u32::MAX as u64,
        KeyWidth::U64 => u64::MAX,
    };
    let n_misses = 1_000_000u32;
    let start = Instant::now();
    for _ in 0..n_misses {
        sink += db.contains(splitmix64(&mut rng) & key_mask) as usize;
    }
    let miss = start.elapsed() / n_misses;

    // Full iteration (decompresses the whole arena frame by frame).
    let start = Instant::now();
    sink += db.iter().map(|(_, p)| p.len()).sum::<usize>();
    let iter = start.elapsed();

    std::hint::black_box(sink);

    let label = frame_size.map_or("raw".to_owned(), |f| format!("{} KiB", f >> 10));
    println!(
        "| {label} | {} | {:.1} ms | {} | {} | {} | {:.0} ms |",
        mib(file_len),
        open_first.as_secs_f64() * 1e3,
        fmt_ns(hit.as_nanos()),
        fmt_ns(batch_hit.as_nanos()),
        fmt_ns(miss.as_nanos()),
        iter.as_secs_f64() * 1e3,
    );

    SweepRow {
        frame_size,
        file_len,
        open_first_ms: open_first.as_secs_f64() * 1e3,
        hit_ns: hit.as_nanos() as u64,
        batch_hit_ns: batch_hit.as_nanos() as u64,
        miss_ns: miss.as_nanos() as u64,
        iter_ms: iter.as_secs_f64() * 1e3,
    }
}

fn build(
    entries: &[(u64, String)],
    key_width: KeyWidth,
    hash_kind: HashKind,
    compression: Compression,
    out: &Path,
) -> (u64, ltk_hashdb::BuildStats) {
    let mut w = HashDbWriter::new(key_width, compression).hash_kind(hash_kind);
    w.extend(entries.iter().map(|(k, p)| (*k, p.as_str())));
    let file = BufWriter::new(File::create(out).expect("create output"));
    let stats = w.build(file).expect("build");
    (stats.file_len, stats)
}
