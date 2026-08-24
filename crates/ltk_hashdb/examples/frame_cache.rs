//! Measures what the frame cache is for: point lookups against batched ones.
//!
//! A WAD extractor resolves one chunk hash at a time - `ltk_wad`'s `PathResolver` is
//! point-shaped by construction - but the chunks of any one archive share a path prefix,
//! so they land in a handful of arena frames. This walks that shape: take a contiguous
//! run of paths (one "archive"), probe them in hash order (a WAD's table-of-contents
//! order, which is unrelated to path order), and compare point lookups with the cache on,
//! with it off, and one `get_batch` over the same keys.
//!
//! ```text
//! cargo run --release -p ltk_hashdb --example frame_cache -- <table.lhdb> [chunks]
//! ```

use std::time::Instant;

use ltk_hashdb::HashDb;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: frame_cache <table.lhdb> [chunks]");
        std::process::exit(2);
    };
    let chunks: usize = args
        .next()
        .and_then(|n| n.to_str().and_then(|n| n.parse().ok()))
        .unwrap_or(20_000);

    let db = HashDb::open(&path).expect("open");
    println!(
        "{}: {} entries, arena {:.1} MiB raw / {:.1} MiB on disk\n",
        path.to_string_lossy(),
        db.len(),
        db.arena_decompressed_size() as f64 / (1 << 20) as f64,
        db.arena_compressed_size() as f64 / (1 << 20) as f64,
    );

    // `iter` yields in arena order, so a window of it is a run of neighbouring paths -
    // one archive's worth. Probing order is by hash, as a WAD's TOC would give them.
    let started = Instant::now();
    let arena_order: Vec<u64> = db.iter().map(|(key, _)| key).collect();
    println!(
        "full scan: {:?} for {} entries",
        started.elapsed(),
        arena_order.len()
    );

    let window = arena_order.len().min(chunks);
    let start = (arena_order.len() - window) / 2;
    let mut keys: Vec<u64> = arena_order[start..start + window].to_vec();
    keys.sort_unstable();

    let cached = HashDb::open(&path).expect("open");
    let uncached = HashDb::options()
        .frame_cache_bytes(0)
        .open(&path)
        .expect("open");

    // Warm the mapping so the first pass isn't paying for page faults the others skip.
    let _ = uncached.get(keys[0]);

    let point_uncached = time(|| {
        for &key in &keys {
            std::hint::black_box(uncached.get(key));
        }
    });
    let point_cached = time(|| {
        for &key in &keys {
            std::hint::black_box(cached.get(key));
        }
    });
    let batched = time(|| {
        for entry in cached.get_batch(&keys) {
            std::hint::black_box(entry);
        }
    });

    let per = |d: std::time::Duration| d.as_secs_f64() * 1e9 / keys.len() as f64;
    println!("\n{window} chunks, probed in hash order:");
    println!("  point, no cache : {:>8.0} ns/lookup", per(point_uncached));
    println!("  point, cached   : {:>8.0} ns/lookup", per(point_cached));
    println!("  get_batch       : {:>8.0} ns/lookup", per(batched));
    println!(
        "\n  cache speedup   : {:>8.1}x     point/batch ratio: {:.2}x",
        point_uncached.as_secs_f64() / point_cached.as_secs_f64(),
        point_cached.as_secs_f64() / batched.as_secs_f64(),
    );

    // The other end of the spectrum: keys spread over the whole table, where no cache
    // of any size holds the working set. This is what the cache must not make slower.
    let stride = arena_order.len() / window.max(1);
    let mut scattered: Vec<u64> = arena_order.iter().copied().step_by(stride.max(1)).collect();
    scattered.truncate(window);
    scattered.sort_unstable();

    let scattered_uncached = time(|| {
        for &key in &scattered {
            std::hint::black_box(uncached.get(key));
        }
    });
    let scattered_cached = time(|| {
        for &key in &scattered {
            std::hint::black_box(cached.get(key));
        }
    });

    println!("\n{} keys spread across the whole table:", scattered.len());
    println!(
        "  point, no cache : {:>8.0} ns/lookup",
        scattered_uncached.as_secs_f64() * 1e9 / scattered.len() as f64
    );
    println!(
        "  point, cached   : {:>8.0} ns/lookup",
        scattered_cached.as_secs_f64() * 1e9 / scattered.len() as f64
    );
}

fn time(mut f: impl FnMut()) -> std::time::Duration {
    let started = Instant::now();
    f();
    started.elapsed()
}
