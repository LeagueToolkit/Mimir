//! Shared utility helpers for examples, benches, and integration tests.
//!
//! Included into each target with `#[path = "../utils/mod.rs"] mod utils;`
//! (examples/benches/tests compile as separate crates, so there is no other way
//! to share code between them). Every target uses only a subset, hence the
//! module-wide `dead_code` allowance.
#![allow(dead_code)]

use std::path::Path;

/// SplitMix64 PRNG step - deterministic, dependency-free key sampling for
/// benches and randomized tests.
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Parse a CDragon `<hex-hash> <path>` list into `(hash, path)` pairs in file
/// order. Only the line terminator is stripped - paths can legitimately be
/// empty or end in a space.
pub fn parse(path: &Path) -> Vec<(u64, String)> {
    let text = std::fs::read_to_string(path).expect("read txt");
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let (hash, entry_path) = line.split_once(' ').expect("<hex-hash> <path>");
        entries.push((
            u64::from_str_radix(hash, 16).expect("hex hash"),
            entry_path.to_owned(),
        ));
    }
    entries
}

/// Human-readable byte size: KiB below 1 MiB, MiB above.
pub fn mib(bytes: u64) -> String {
    if bytes < 1 << 20 {
        format!("{:.0} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1 << 20) as f64)
    }
}

/// Human-readable duration: ns below 10 µs, µs above.
pub fn fmt_ns(ns: u128) -> String {
    if ns < 10_000 {
        format!("{ns} ns")
    } else {
        format!("{:.1} µs", ns as f64 / 1e3)
    }
}

/// Group an integer with thousands separators (`1234567` -> `1,234,567`).
pub fn group_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// bench_real report - the JSON contract between `bench_real` (writer) and
// `gen_charts` (reader).

/// One `bench_real` run: everything the run measured, written to
/// `<out_dir>/bench_real.json`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct BenchReport {
    /// Per-table build results at the default frame size and level.
    pub tables: Vec<TableBuild>,

    /// Frame-size sweeps with per-lookup timings on the largest tables.
    pub sweeps: Vec<Sweep>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct TableBuild {
    /// Logical table name (`game`, `lcu`, ...).
    pub table: String,

    pub entries: u64,

    /// Size of the source `hashes.<table>.txt`, bytes.
    pub txt_len: u64,

    /// Size of the uncompressed `.hashdb`, bytes.
    pub raw_len: u64,

    /// Size of the zstd `.hashdb` (default frame size / level), bytes.
    pub zstd_len: u64,

    /// Wall-clock build time of the zstd file, seconds.
    pub build_secs: f64,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Sweep {
    pub table: String,
    pub entries: u64,
    pub rows: Vec<SweepRow>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SweepRow {
    /// Zeekstd frame size in bytes; `None` for the uncompressed build.
    pub frame_size: Option<u32>,

    pub file_len: u64,

    /// Open + first point lookup, milliseconds.
    pub open_first_ms: f64,

    /// Average random point hit, nanoseconds.
    pub hit_ns: u64,

    /// Average per-key `get_batch` hit, nanoseconds.
    pub batch_hit_ns: u64,

    /// Average membership-probe miss, nanoseconds.
    pub miss_ns: u64,

    /// Full arena-order iteration, milliseconds.
    pub iter_ms: f64,
}
