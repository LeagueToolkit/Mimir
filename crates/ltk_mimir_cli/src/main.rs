//! The `mimir` CLI. Verbs: build / get / update / gen / merge / publish /
//! verify / stats.

mod merge;
mod publish;
#[cfg(test)]
mod testutil;
mod update;

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use ltk_hashdb::{Casing, Compression, HashDb, HashDbWriter, HashKind, KeyWidth};
use ltk_mimir_cache::{HashStore, Table as CacheTable};
use ltk_mimir_gen::guessers::{
    CharacterSkin, CrossReference, ExtensionSwap, NumericRange, PrefixVariants, RegionLocale,
    SeedStrings, WordAdd, WordSubstitution,
};
use ltk_mimir_gen::{GuessContext, Hunt};

#[derive(Parser)]
#[command(name = "mimir", version, about = "League Toolkit hash tables")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// The logical CDragon tables; picks key width and hash algorithm.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Table {
    Game,
    Lcu,
    BinEntries,
    BinTypes,
    BinFields,
    BinHashes,
    Rst,
    RstXxh3,
}

impl Table {
    fn key_width(self) -> KeyWidth {
        match self {
            Self::Game | Self::Lcu | Self::Rst | Self::RstXxh3 => KeyWidth::U64,
            _ => KeyWidth::U32,
        }
    }

    fn hash_kind(self) -> HashKind {
        match self {
            Self::Game | Self::Lcu | Self::Rst => HashKind::Xxh64,
            Self::RstXxh3 => HashKind::Xxh3,
            _ => HashKind::Fnv1a32,
        }
    }

    /// Every League table hashes the lowercased path.
    fn casing(self) -> Casing {
        Casing::Insensitive
    }

    /// The corresponding shared-cache table.
    fn cache(self) -> CacheTable {
        match self {
            Self::Game => CacheTable::Game,
            Self::Lcu => CacheTable::Lcu,
            Self::BinEntries => CacheTable::BinEntries,
            Self::BinTypes => CacheTable::BinTypes,
            Self::BinFields => CacheTable::BinFields,
            Self::BinHashes => CacheTable::BinHashes,
            Self::Rst => CacheTable::Rst,
            Self::RstXxh3 => CacheTable::RstXxh3,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Build a .hashdb table from a txt hash list (lines of `<hex-hash> <path>`).
    Build {
        /// Input txt file (CDragon `hashes.*.txt` format).
        #[arg(long)]
        input: PathBuf,

        /// Which logical table this is (sets key width + hash algorithm).
        #[arg(long)]
        table: Table,

        /// Output .hashdb file.
        #[arg(long)]
        out: PathBuf,

        /// Store the arena uncompressed instead of zeekstd-compressed.
        #[arg(long)]
        raw: bool,

        /// Uncompressed frame size for the zeekstd arena, in bytes.
        /// 16 KiB is the measured size/latency knee - see docs/BENCHMARKS.md.
        #[arg(long, default_value_t = 16384, conflicts_with = "raw")]
        frame_size: u32,

        /// zstd compression level. Only build time depends on it, so published
        /// tables use 19; decompression speed is level-independent.
        #[arg(long, default_value_t = 19, conflicts_with = "raw")]
        level: i32,
    },

    /// Resolve one hash from a .hashdb file or the shared cache.
    Get {
        /// The hash, in hex (with or without `0x`).
        hash: String,

        /// Look in this .hashdb file directly.
        #[arg(long, conflicts_with = "table", required_unless_present = "table")]
        file: Option<PathBuf>,

        /// Resolve from the shared cache's active version of this table instead
        /// (cache dir: MIMIR_DIR override, else the platform data dir).
        #[arg(long, conflicts_with = "file", required_unless_present = "file")]
        table: Option<Table>,
    },

    /// Download the latest published tables into the shared cache.
    Update {
        /// GitHub repository whose latest release ships the tables.
        #[arg(long, default_value = "LeagueToolkit/mimir")]
        repo: String,

        /// Base URL serving `manifest.json` and the `.lhdb` assets (a mirror);
        /// overrides --repo.
        #[arg(long)]
        url: Option<String>,

        /// Install into this directory instead of the shared cache
        /// (MIMIR_DIR override, else the platform data dir).
        #[arg(long)]
        dir: Option<PathBuf>,

        /// Reinstall every table even if the local copy already matches.
        #[arg(long)]
        force: bool,
    },

    /// Run the hunt engine: discover paths for still-unknown hashes.
    Gen {
        /// Known-hashes txt (CDragon `<hex-hash> <path>` format); repeatable.
        #[arg(long, required = true)]
        known: Vec<PathBuf>,

        /// Target hashes to resolve, one hex hash per line. Hashes already
        /// present in --known are skipped. Optional when --wad supplies the
        /// unknown set instead.
        #[arg(long, required_unless_present = "wad")]
        unknown: Option<PathBuf>,

        /// WAD archives to mine: chunk contents are parsed/grepped for seed
        /// strings, and (game table) the chunk path hashes join the unknown
        /// set. Repeatable.
        #[arg(long)]
        wad: Vec<PathBuf>,

        /// Which logical table (sets key width, hash algorithm, guesser preset).
        #[arg(long)]
        table: Table,

        /// Extra candidate strings (one per line) checked verbatim, e.g.
        /// grepped from WAD chunks or bin files.
        #[arg(long)]
        seeds: Option<PathBuf>,

        /// Also run the wordlist guessers. Their cost scales with
        /// corpus × vocabulary - hours on the full game table.
        #[arg(long)]
        words: bool,

        /// Upper bound for numeric-range substitution.
        #[arg(long, default_value_t = 200)]
        max_number: u32,

        /// Upper bound for skin-number substitution (game preset only).
        #[arg(long, default_value_t = 100)]
        max_skin: u32,

        /// Write newly resolved `<hex-hash> <path>` lines here, sorted by hash.
        #[arg(long)]
        out: PathBuf,
    },

    /// Sorted dedup merge of CDragon txt hash lists.
    Merge {
        /// Input txt files (`<hex-hash> <path>` per line).
        #[arg(required = true)]
        inputs: Vec<PathBuf>,

        /// Output file; stdout when omitted.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Build all tables + manifest from CDragon txt inputs, staged for a GH release.
    Publish {
        /// Directory of CDragon `hashes.*.txt` inputs (the data repo's `hashes/lol`).
        /// `hashes.game.txt` may be a single file or split into `.0`, `.1`, … parts.
        #[arg(long)]
        inputs: PathBuf,

        /// Staging directory; receives the `<table>-<version>.lhdb` files plus a
        /// `manifest.json` listing exactly the tables built by this run. Must not
        /// already contain a manifest.
        #[arg(long)]
        out: PathBuf,

        /// Version label for the immutable filenames. Defaults to today's UTC date
        /// (`YYYY-MM-DD`).
        #[arg(long)]
        version: Option<String>,

        /// Where the txt inputs come from - a git URL or a GitHub `owner/repo` -
        /// recorded as provenance in the manifest.
        #[arg(long, default_value = "CommunityDragon/Data")]
        source_repo: String,

        /// Commit of the source repo the inputs were taken at, recorded as
        /// provenance in the manifest.
        #[arg(long)]
        source_commit: Option<String>,

        /// Skip tables whose inputs are absent instead of failing.
        #[arg(long)]
        allow_missing: bool,

        /// Uncompressed frame size for the zeekstd arena, in bytes.
        #[arg(long, default_value_t = 16384)]
        frame_size: u32,

        /// zstd compression level. Only build time depends on it; published tables
        /// use 19.
        #[arg(long, default_value_t = 19)]
        level: i32,
    },

    /// Structural + checksum validation of a .hashdb file.
    Verify { file: PathBuf },

    /// Sizes, entry counts, compression ratio of a .hashdb file.
    Stats { file: PathBuf },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Build {
            input,
            table,
            out,
            raw,
            frame_size,
            level,
        } => {
            let compression = if raw {
                Compression::None
            } else {
                Compression::Zeekstd { frame_size, level }
            };
            build(input, table, out, compression)
        }
        Command::Get { hash, file, table } => get(&hash, file, table),
        Command::Gen {
            known,
            unknown,
            wad,
            table,
            seeds,
            words,
            max_number,
            max_skin,
            out,
        } => gen_hashes(
            known, unknown, wad, table, seeds, words, max_number, max_skin, out,
        ),
        Command::Verify { file } => verify(file),
        Command::Stats { file } => stats(file),
        Command::Publish {
            inputs,
            out,
            version,
            source_repo,
            source_commit,
            allow_missing,
            frame_size,
            level,
        } => publish::run(&publish::Options {
            inputs,
            out,
            version,
            source_repo,
            source_commit,
            allow_missing,
            compression: Compression::Zeekstd { frame_size, level },
        }),
        Command::Update {
            repo,
            url,
            dir,
            force,
        } => update::run(&update::Options {
            repo,
            url,
            dir,
            force,
        }),
        Command::Merge { inputs, out } => merge::run(&inputs, out.as_deref()),
    }
}

/// Parse a hex hash, tolerating a leading `0x`.
fn parse_hex_hash(s: &str) -> std::result::Result<u64, std::num::ParseIntError> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16)
}

/// Parse a CDragon-format txt: `<hex-hash> <path>` per line. The callback also
/// receives the raw hex token so callers can preserve its width.
fn read_hash_lines(input: &Path, mut on_entry: impl FnMut(u64, &str, &str)) -> Result<()> {
    let reader =
        BufReader::new(File::open(input).with_context(|| format!("opening {}", input.display()))?);
    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        // Strip only the line terminator: paths can legitimately end in a space
        // (e.g. binhashes has `811c9dc5 ` - FNV-1a of the empty string).
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if line.is_empty() {
            continue;
        }
        let (hash, path) = line.split_once(' ').with_context(|| {
            format!(
                "{}:{}: expected `<hex-hash> <path>`",
                input.display(),
                i + 1
            )
        })?;
        let value = u64::from_str_radix(hash, 16)
            .with_context(|| format!("{}:{}: bad hex hash {hash:?}", input.display(), i + 1))?;
        on_entry(value, hash, path);
    }
    Ok(())
}

fn build(input: PathBuf, table: Table, out: PathBuf, compression: Compression) -> Result<()> {
    let mut writer = HashDbWriter::new(table.key_width(), compression)
        .hash_kind(table.hash_kind())
        .casing(table.casing());
    read_hash_lines(&input, |hash, _, path| {
        writer.insert(hash, path);
    })?;

    let out_file =
        BufWriter::new(File::create(&out).with_context(|| format!("creating {}", out.display()))?);
    let stats = writer.build(out_file)?;
    println!(
        "{}: {} entries, arena {} B -> {} B on disk, file {} B",
        out.display(),
        stats.entries,
        stats.arena_decompressed_size,
        stats.arena_compressed_size,
        stats.file_len,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn gen_hashes(
    known: Vec<PathBuf>,
    unknown: Option<PathBuf>,
    wads: Vec<PathBuf>,
    table: Table,
    seeds: Option<PathBuf>,
    words: bool,
    max_number: u32,
    max_skin: u32,
    out: PathBuf,
) -> Result<()> {
    let mut ctx = GuessContext::new(table.hash_kind(), table.casing(), table.key_width());
    let mut known_hashes = HashSet::new();
    for input in &known {
        let mut paths = Vec::new();
        read_hash_lines(input, |hash, _, path| {
            known_hashes.insert(hash);
            paths.push(Box::<str>::from(path));
        })?;
        ctx.add_known(paths);
    }

    let mut targets = Vec::new();
    if let Some(unknown) = &unknown {
        let reader = BufReader::new(
            File::open(unknown).with_context(|| format!("opening {}", unknown.display()))?,
        );
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let hash = parse_hex_hash(line).with_context(|| {
                format!("{}:{}: bad hex hash {line:?}", unknown.display(), i + 1)
            })?;
            targets.push(hash);
        }
    }

    // Each WAD contributes its mined strings as seeds, and - for the game
    // table, whose keys are exactly the chunk path hashes - its chunk table
    // as unknowns.
    let mut seed_strings: Vec<Box<str>> = Vec::new();
    for path in &wads {
        let report =
            ltk_mimir_gen::mine_wad(path).with_context(|| format!("mining {}", path.display()))?;
        eprintln!(
            "{}: mined {} strings from {} chunks{}",
            path.display(),
            report.strings.len(),
            report.chunk_hashes.len(),
            if report.chunks_skipped > 0 {
                format!(" ({} skipped)", report.chunks_skipped)
            } else {
                String::new()
            }
        );

        if matches!(table, Table::Game) {
            targets.extend(report.chunk_hashes);
        }
        seed_strings.extend(report.strings);
    }

    targets.retain(|hash| !known_hashes.contains(hash));
    targets.sort_unstable();
    targets.dedup();
    ctx.add_unknown(targets);

    if let Some(seeds) = seeds {
        let reader = BufReader::new(
            File::open(&seeds).with_context(|| format!("opening {}", seeds.display()))?,
        );
        for line in reader.lines() {
            let line = line?;
            if !line.is_empty() {
                seed_strings.push(line.into());
            }
        }
    }

    let mut hunt = Hunt::new();
    if !seed_strings.is_empty() {
        hunt = hunt.with(SeedStrings::new(seed_strings));
    }
    hunt = match table {
        Table::Game => hunt
            .with(ExtensionSwap)
            .with(PrefixVariants)
            .with(CrossReference)
            .with(NumericRange::new(max_number))
            .with(CharacterSkin::new(max_skin)),
        Table::Lcu => hunt
            .with(ExtensionSwap)
            .with(RegionLocale)
            .with(CrossReference)
            .with(NumericRange::new(max_number)),
        _ => hunt.with(ExtensionSwap).with(NumericRange::new(max_number)),
    };
    if words {
        hunt = hunt.with(WordSubstitution).with(WordAdd);
    }

    let total_unknown = ctx.unknown().len();
    eprintln!(
        "hunting {} unknown hashes with a corpus of {} known paths",
        total_unknown,
        ctx.known_paths().len()
    );

    let report = hunt.run(&mut ctx);
    for (i, round) in report.rounds.iter().enumerate() {
        for g in &round.guessers {
            eprintln!(
                "round {} {:<18} {:>13} candidates {:>6} found  {:.1?}",
                i + 1,
                g.name,
                g.candidates,
                g.found,
                g.elapsed
            );
        }
    }

    let mut resolved = report.resolved;
    resolved.sort_unstable();

    let mut out_file =
        BufWriter::new(File::create(&out).with_context(|| format!("creating {}", out.display()))?);
    let hex_width = 2 * table.key_width().bytes();
    // The game-class CDragon lists store paths lowercased (the bin lists keep
    // original casing); match, so merging finds into a list never produces
    // case-only duplicates of the same hash.
    let lowercase = matches!(
        table,
        Table::Game | Table::Lcu | Table::Rst | Table::RstXxh3
    );
    for (hash, path) in &resolved {
        let path = if lowercase {
            std::borrow::Cow::Owned(path.to_lowercase())
        } else {
            std::borrow::Cow::Borrowed(path.as_str())
        };
        writeln!(out_file, "{hash:0hex_width$x} {path}")?;
    }
    println!(
        "resolved {} of {} unknown hashes -> {}",
        resolved.len(),
        total_unknown,
        out.display()
    );
    Ok(())
}

fn get(hash: &str, file: Option<PathBuf>, table: Option<Table>) -> Result<()> {
    let hash = parse_hex_hash(hash).with_context(|| format!("bad hex hash {hash:?}"))?;

    // clap guarantees exactly one of `file` / `table` is set.
    let (db, source) = match (file, table) {
        (Some(file), _) => {
            let db = HashDb::open(&file).with_context(|| format!("opening {}", file.display()))?;
            (db, file.display().to_string())
        }
        (None, Some(table)) => {
            let store = HashStore::discover()?;
            let db = store
                .open(table.cache())
                .with_context(|| format!("opening {table:?} from the shared cache"))?;
            (db, format!("the shared cache ({table:?})"))
        }
        (None, None) => unreachable!("clap requires --file or --table"),
    };

    match db.get(hash) {
        Some(path) => {
            println!("{path}");
            Ok(())
        }
        None => bail!("{hash:#x} not found in {source}"),
    }
}

fn verify(file: PathBuf) -> Result<()> {
    let db = HashDb::open(&file).with_context(|| format!("opening {}", file.display()))?;
    db.verify()?;
    println!("{}: ok ({} entries)", file.display(), db.len());
    Ok(())
}

fn stats(file: PathBuf) -> Result<()> {
    let db = HashDb::open(&file).with_context(|| format!("opening {}", file.display()))?;
    let file_len = std::fs::metadata(&file)?.len();

    println!("file:       {} ({file_len} B)", file.display());
    println!("entries:    {}", db.len());
    println!("key width:  {} bytes", db.key_width().bytes());
    println!("hash kind:  {:?}", db.hash_kind());
    println!("casing:     {:?}", db.casing());
    println!(
        "arena:      {} B raw, {} B on disk ({})",
        db.arena_decompressed_size(),
        db.arena_compressed_size(),
        if db.is_compressed() {
            format!(
                "zeekstd, {:.1}%",
                100.0 * db.arena_compressed_size() as f64 / db.arena_decompressed_size() as f64
            )
        } else {
            "raw".to_owned()
        }
    );
    Ok(())
}
