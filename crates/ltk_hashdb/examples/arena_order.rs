//! What the arena-order section costs and what it buys, measured on a real table.
//!
//! ```sh
//! cargo run --release --example arena_order -- game.lhdb            # as published
//! cargo run --release --example arena_order -- game.lhdb game+ao.lhdb   # and rebuilt with the section
//! ```
//!
//! With an output path it rebuilds the table with [`ArenaOrder::Stored`] and
//! reports both, which is where `docs/BENCHMARKS.md` gets its figures. Timings
//! are cold-per-process: each phase runs against a freshly opened table, so a
//! rebuilt permutation is paid for exactly once, as a consumer pays for it.

use std::io::BufWriter;
use std::path::Path;
use std::time::Instant;

use ltk_hashdb::{ArenaOrder, Compression, HashDb, HashDbWriter};

/// Prefixes to search for: one large directory, one small, one that matches nothing.
const PROBES: [&str; 3] = [
    "assets/characters/ahri/",
    "data/menu/",
    "zzz-nothing-starts-with-this/",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .ok_or("usage: arena_order <table.lhdb> [out.lhdb]")?;

    report(Path::new(&input))?;

    if let Some(output) = args.next() {
        rebuild(Path::new(&input), Path::new(&output))?;
        report(Path::new(&output))?;
    }

    Ok(())
}

/// Time the three arena-order reads, each against its own freshly opened table.
fn report(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let db = HashDb::open(path)?;
    let file_len = std::fs::metadata(path)?.len();
    let section = db.arena_order_size();

    println!("\n{}", path.display());
    println!("  entries      {}", db.len());
    println!("  file         {file_len} B");
    match section {
        Some(bytes) => println!(
            "  arena order  {bytes} B stored ({:.1}% of the file)",
            100.0 * bytes as f64 / file_len as f64
        ),
        None => println!("  arena order  not stored - the reader sorts for it"),
    }

    // The first arena-order read is what pays for a rebuild, so it gets a table
    // of its own; nothing else in the process has warmed the permutation.
    let db = HashDb::open(path)?;
    let start = Instant::now();
    let first = db.prefix(PROBES[0]).count();
    println!(
        "  prefix       {:>8.1} ms  first call, {first} hit(s) for {:?}",
        start.elapsed().as_secs_f64() * 1e3,
        PROBES[0]
    );

    for probe in PROBES {
        let start = Instant::now();
        let hits = db.prefix(probe).count();
        println!(
            "               {:>8.1} ms  warm, {hits} hit(s) for {probe:?}",
            start.elapsed().as_secs_f64() * 1e3
        );
    }

    let db = HashDb::open(path)?;
    let start = Instant::now();
    let count = db.values().count();
    println!(
        "  values       {:>8.1} ms  {count} paths",
        start.elapsed().as_secs_f64() * 1e3
    );

    Ok(())
}

/// Rewrite `input` with the arena-order section, entry for entry.
fn rebuild(input: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let db = HashDb::open(input)?;
    let mut writer = HashDbWriter::with_key_config(
        db.key_config(),
        Compression::Zeekstd {
            frame_size: 16 << 10,
            level: 19,
        },
    )
    .arena_order(ArenaOrder::Stored);

    for (hash, path) in db.iter() {
        writer.insert(hash, &path);
    }

    let start = Instant::now();
    let stats = writer.build(BufWriter::new(std::fs::File::create(output)?))?;
    println!(
        "\nrebuilt {} in {:.1} s: {} B, of which {} B is the arena order",
        output.display(),
        start.elapsed().as_secs_f64(),
        stats.file_len,
        stats.arena_order_size,
    );

    // The two tables must agree entry for entry - a stored permutation and a
    // sorted one are the same permutation or one of them is wrong.
    let rebuilt = HashDb::open(output)?;
    let mine: Vec<_> = rebuilt.values().map(|p| p.into_owned()).collect();
    let theirs: Vec<_> = db.values().map(|p| p.into_owned()).collect();
    assert_eq!(mine, theirs, "stored and sorted arena order disagree");
    println!(
        "stored and sorted arena order agree over all {} paths",
        mine.len()
    );

    Ok(())
}
