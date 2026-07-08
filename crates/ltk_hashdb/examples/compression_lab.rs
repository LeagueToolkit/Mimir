//! Compression lab: what would a higher zstd level, a trained dictionary, or
//! a different arena ordering buy on the real game arena? Feeds the ratio
//! discussion in `docs/BENCHMARKS.md`.
//!
//! ```text
//! cargo run --release -p ltk_hashdb --example compression_lab -- data/cdragon/hashes.game.txt
//! ```
//!
//! Frames are modeled with `zstd::bulk` (16 KiB decompressed chunks, exactly
//! what the seekable writer produces minus a few bytes of framing), so every
//! variant is comparable to the shipping format.

use std::path::PathBuf;
use std::time::Instant;

#[path = "../utils/mod.rs"]
mod utils;
use utils::{parse, splitmix64};

const FRAME_SIZE: usize = 16 << 10;

fn main() {
    let input = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "data/cdragon/hashes.game.txt".into()),
    );
    let mut entries = parse(&input);

    // Arena as shipped: strings concatenated in key order.
    entries.sort_unstable_by_key(|&(k, _)| k);
    let arena_key: Vec<u8> = entries
        .iter()
        .flat_map(|(_, p)| p.as_bytes().iter().copied())
        .collect();

    // Arena reordered lexicographically: the upper bound of what grouping
    // similar paths into the same frame can buy (would need a format change -
    // offsets are required to be monotonic today).
    let mut by_path: Vec<&str> = entries.iter().map(|(_, p)| p.as_str()).collect();
    by_path.sort_unstable();
    let arena_path: Vec<u8> = by_path
        .iter()
        .flat_map(|p| p.as_bytes().iter().copied())
        .collect();

    println!(
        "arena: {:.1} MiB, {} entries, {} frames of {} KiB\n",
        arena_key.len() as f64 / (1 << 20) as f64,
        entries.len(),
        arena_key.len().div_ceil(FRAME_SIZE),
        FRAME_SIZE >> 10,
    );

    // Dictionaries trained on a sample of key-order frames.
    let dict_small = train(&arena_key, 64 << 10);
    let dict_big = train(&arena_key, 112 << 10);

    println!("| variant | level | compressed | ratio | compress | frame decompress |");
    println!("|---------|------:|-----------:|------:|---------:|-----------------:|");
    for level in [3, 9, 15, 19] {
        run("key-order", &arena_key, level, None);
    }
    for level in [3, 19] {
        run("key-order + dict64K", &arena_key, level, Some(&dict_small));
        run("key-order + dict112K", &arena_key, level, Some(&dict_big));
    }
    for level in [3, 19] {
        run("path-order", &arena_path, level, None);
    }
    {
        // Path order changes frame contents, so train a matching dictionary.
        let dict = train(&arena_path, 112 << 10);
        run("path-order + dict112K", &arena_path, 3, Some(&dict));
    }

    // Reference floor: one solid frame (no random access at all).
    let start = Instant::now();
    let solid = zstd::bulk::compress(&arena_key, 19).expect("solid compress");
    println!(
        "| solid (no random access) | 19 | {:.1} MiB | {:.1}% | {:.0}s | - |",
        solid.len() as f64 / (1 << 20) as f64,
        100.0 * solid.len() as f64 / arena_key.len() as f64,
        start.elapsed().as_secs_f64(),
    );
}

/// Compresses `arena` frame by frame, then times decompression of 2 000
/// random frames - the per-hit cost a reader would pay.
fn run(label: &str, arena: &[u8], level: i32, dict: Option<&[u8]>) {
    // Contexts are created once and reused - a real writer/reader would hold
    // the digested dictionary, not re-load it per frame.
    let mut compressor = match dict {
        Some(d) => zstd::bulk::Compressor::with_dictionary(level, d),
        None => zstd::bulk::Compressor::new(level),
    }
    .expect("compressor");
    let start = Instant::now();
    let frames: Vec<Vec<u8>> = arena
        .chunks(FRAME_SIZE)
        .map(|chunk| compressor.compress(chunk).expect("compress"))
        .collect();
    let compress_time = start.elapsed();
    let total: usize = frames.iter().map(Vec::len).sum();

    let mut decompressor = match dict {
        Some(d) => zstd::bulk::Decompressor::with_dictionary(d),
        None => zstd::bulk::Decompressor::new(),
    }
    .expect("decompressor");
    let mut rng = 0x1AB5EEDu64;
    let n = 2_000u32;
    let start = Instant::now();
    for _ in 0..n {
        let i = (splitmix64(&mut rng) % frames.len() as u64) as usize;
        let out = decompressor
            .decompress(&frames[i], FRAME_SIZE)
            .expect("decompress");
        std::hint::black_box(out.len());
    }
    let decompress = start.elapsed() / n;

    let dict_len = dict.map_or(0, <[u8]>::len);
    println!(
        "| {label} | {level} | {:.1} MiB | {:.1}% | {:.0}s | {:.1} µs |",
        (total + dict_len) as f64 / (1 << 20) as f64,
        100.0 * (total + dict_len) as f64 / arena.len() as f64,
        compress_time.as_secs_f64(),
        decompress.as_nanos() as f64 / 1e3,
    );
}

/// Trains a dictionary on every 4th frame (plenty of coverage, fast).
fn train(arena: &[u8], max_size: usize) -> Vec<u8> {
    let mut samples = Vec::new();
    let mut sizes = Vec::new();
    for chunk in arena.chunks(FRAME_SIZE).step_by(4) {
        samples.extend_from_slice(chunk);
        sizes.push(chunk.len());
    }
    zstd::dict::from_continuous(&samples, &sizes, max_size).expect("train dictionary")
}
