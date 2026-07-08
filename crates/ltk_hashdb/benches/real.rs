//! Criterion benches over real CDragon tables.
//!
//! Expects prebuilt `.hashdb` files - run the builder first:
//!
//! ```text
//! cargo run --release -p ltk_hashdb --example bench_real -- data/cdragon data/build
//! cargo bench -p ltk_hashdb
//! ```
//!
//! Skips (benching nothing) when the build dir is absent so `cargo bench`
//! stays green without the 270 MB download. Override the dir with
//! `MIMIR_BUILD_DIR`.

use std::path::PathBuf;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use ltk_hashdb::{HashDb, KeyWidth};

#[path = "../utils/mod.rs"]
mod utils;
use utils::splitmix64;

fn build_dir() -> PathBuf {
    std::env::var_os("MIMIR_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/build"))
}

fn bench_real(c: &mut Criterion) {
    let dir = build_dir();
    // game = largest u64 table, binentries = largest u32 table; the plain
    // name is the zeekstd build, `.raw` the uncompressed one.
    let files = [
        "game.hashdb",
        "game.raw.hashdb",
        "binentries.hashdb",
        "binentries.raw.hashdb",
    ];
    if !files.iter().all(|f| dir.join(f).exists()) {
        eprintln!(
            "real-data benches skipped: prebuilt tables not found in {} \
             (run `cargo run --release -p ltk_hashdb --example bench_real` first)",
            dir.display()
        );
        return;
    }

    for file in files {
        let path = dir.join(file);
        let db = HashDb::open(&path).expect("open");
        let label = file.trim_end_matches(".hashdb");

        // A spread of existing keys, in random order.
        let all: Vec<u64> = db.iter().map(|(k, _)| k).collect();
        let mut rng = 0x5EEDu64;
        let keys: Vec<u64> = (0..10_000)
            .map(|_| all[(splitmix64(&mut rng) % all.len() as u64) as usize])
            .collect();
        let key_mask = match db.key_width() {
            KeyWidth::U32 => u32::MAX as u64,
            KeyWidth::U64 => u64::MAX,
        };

        let mut group = c.benchmark_group("real");
        group.measurement_time(Duration::from_secs(5));
        // iter_full over the game arena runs ~0.5 s per iteration; keep the
        // sample count small so the whole suite stays in minutes.
        group.sample_size(20);

        let mut i = 0usize;
        group.bench_function(BenchmarkId::new("hit", label), |b| {
            b.iter(|| {
                i = (i + 1) % keys.len();
                db.get(keys[i]).map(|p| p.len())
            })
        });

        group.bench_function(BenchmarkId::new("miss", label), |b| {
            b.iter(|| db.contains(splitmix64(&mut rng) & key_mask))
        });

        group.throughput(Throughput::Elements(keys.len() as u64));
        group.bench_function(BenchmarkId::new("batch_hit_10k", label), |b| {
            b.iter(|| {
                db.get_batch(&keys)
                    .map(|(_, p)| p.map_or(0, |p| p.len()))
                    .sum::<usize>()
            })
        });

        group.throughput(Throughput::Bytes(db.arena_decompressed_size()));
        group.bench_function(BenchmarkId::new("iter_full", label), |b| {
            b.iter(|| db.iter().map(|(_, p)| p.len()).sum::<usize>())
        });
        group.finish();

        drop(db);
        c.bench_function(&format!("real/open_first_hit/{label}"), |b| {
            b.iter(|| {
                let db = HashDb::open(&path).expect("open");
                db.get(keys[0]).map(|p| p.len())
            })
        });
    }
}

criterion_group!(benches, bench_real);
criterion_main!(benches);
