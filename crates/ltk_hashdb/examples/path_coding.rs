//! Does factoring paths beat leaving zstd to it? Measured on a real table.
//!
//! ```sh
//! cargo run --release --example path_coding -- game.lhdb
//! ```
//!
//! Four encodings of the same path list, each compressed with the arena's own
//! encoder, against the file they came from:
//!
//! - **raw** - what the arena stores today: sorted paths, concatenated.
//! - **front-coded** - each path as `varint(shared prefix with the previous)` and
//!   the suffix. Restart-free, so this is a *lower bound*: a real one needs a
//!   restart per frame and a replay on every read.
//! - **matched frames** - the same two at equal paths per frame. Front coding
//!   packs several times more paths into a 16 KiB frame, so some of its lead is
//!   a coarser lookup granularity rather than the coding; this separates them.
//! - **interned** - directories stored once, each entry a `(dir_id, filename)`.
//!   The dir_id array is index, not arena: fixed width, random access, and so
//!   not compressed at all.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use ltk_hashdb::HashDb;

/// The published arena settings, so every number here is comparable to the file.
const FRAME_SIZE: u32 = 16 << 10;
const LEVEL: i32 = 19;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: path_coding <table.lhdb>")?;
    let path = Path::new(&path);

    let db = HashDb::open(path)?;
    let file_len = std::fs::metadata(path)?.len();
    let entries = db.len();

    // The arena stores one copy of each distinct path, in path order - which is
    // exactly `values` with adjacent duplicates dropped.
    let mut paths: Vec<String> = Vec::with_capacity(entries);
    for path in db.values() {
        if paths.last().map(String::as_str) != Some(&*path) {
            paths.push(path.into_owned());
        }
    }

    println!();
    println!("{}", path.display());
    println!("  file            {file_len:>12} B");
    println!("  entries         {entries:>12}");
    println!("  distinct paths  {:>12}", paths.len());
    println!(
        "  arena on disk   {:>12} B  ({:.1}% of the file)",
        db.arena_compressed_size(),
        100.0 * db.arena_compressed_size() as f64 / file_len as f64
    );
    println!(
        "  index on disk   {:>12} B  ({:.1}% of the file, and random access, so not compressible)",
        file_len - db.arena_compressed_size(),
        100.0 * (file_len - db.arena_compressed_size()) as f64 / file_len as f64
    );

    // --- raw: what we ship today ---------------------------------------------
    let raw: Vec<u8> = paths.iter().flat_map(|p| p.as_bytes().to_vec()).collect();

    // --- front coding ---------------------------------------------------------
    let mut coded = Vec::with_capacity(raw.len() / 2);
    let mut prev: &str = "";
    for path in &paths {
        let shared = path
            .as_bytes()
            .iter()
            .zip(prev.as_bytes())
            .take_while(|(a, b)| a == b)
            .count();
        push_varint(&mut coded, shared as u64);
        coded.extend_from_slice(&path.as_bytes()[shared..]);
        prev = path;
    }

    // Paths per frame at the published frame size, and the frame sizes that give
    // each coding the other's granularity.
    let raw_per_frame = FRAME_SIZE as usize * paths.len() / raw.len();
    let coded_per_frame = FRAME_SIZE as usize * paths.len() / coded.len();
    let narrow = (coded.len() * raw_per_frame / paths.len()) as u32;
    let wide = (raw.len() * coded_per_frame / paths.len()) as u32;

    println!();
    println!("  encoding                        on disk  frame     paths/frame");
    row("raw", &raw, FRAME_SIZE, raw_per_frame, file_len)?;
    row("front-coded", &coded, FRAME_SIZE, coded_per_frame, file_len)?;
    row(
        "front-coded, raw's frames",
        &coded,
        narrow,
        raw_per_frame,
        file_len,
    )?;
    row(
        "raw, front coding's frames",
        &raw,
        wide,
        coded_per_frame,
        file_len,
    )?;

    // --- interning ------------------------------------------------------------
    let mut dirs: HashMap<&str, usize> = HashMap::new();
    let mut dir_bytes = Vec::new();
    let mut names = Vec::new();
    for path in &paths {
        let (dir, name) = match path.rfind('/') {
            Some(cut) => path.split_at(cut + 1),
            None => ("", path.as_str()),
        };
        let next = dirs.len();
        if dirs.insert(dir, next).is_none() {
            dir_bytes.extend_from_slice(dir.as_bytes());
        }
        names.extend_from_slice(name.as_bytes());
    }

    let id_width =
        ((usize::BITS - dirs.len().saturating_sub(1).leading_zeros()).div_ceil(8)).max(1) as usize;
    let ids = entries * id_width;
    let arena = compressed(&dir_bytes, FRAME_SIZE)? + compressed(&names, FRAME_SIZE)?;

    println!();
    println!(
        "  interned: {} directories stored once, {} B of file names",
        dirs.len(),
        names.len()
    );
    println!("    arena  {arena:>12} B  compressed");
    println!("    ids    {ids:>12} B  {id_width} bytes per entry, in the index, not compressed");
    println!(
        "    total  {:>12} B  ({:+.1}% against the {} B arena it replaces)",
        arena + ids,
        100.0 * (arena + ids) as f64 / db.arena_compressed_size() as f64 - 100.0,
        db.arena_compressed_size()
    );

    Ok(())
}

fn row(
    name: &str,
    bytes: &[u8],
    frame_size: u32,
    per_frame: usize,
    file_len: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let packed = compressed(bytes, frame_size)?;
    println!(
        "  {name:<28} {packed:>10} B  {frame_size:>6}  {per_frame:>6}   ({:.1}% of the file)",
        100.0 * packed as f64 / file_len as f64
    );

    Ok(())
}

/// Compress with the arena's own encoder, so the numbers are comparable.
fn compressed(bytes: &[u8], frame_size: u32) -> Result<usize, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    let mut encoder = zeekstd::EncodeOptions::new()
        .compression_level(LEVEL)
        .frame_size_policy(zeekstd::FrameSizePolicy::Uncompressed(frame_size))
        .into_encoder(&mut out)?;
    encoder.write_all(bytes)?;
    encoder.finish()?;

    Ok(out.len())
}

fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}
