//! Mining WAD archives for hunt inputs: path-like strings (seed candidates)
//! and the chunk table's path hashes (the unknowns worth hunting).
//!
//! This is CDTB's highest-yield discovery strategy. `.bin` chunks are parsed
//! properly with `ltk_meta` - their literal string property values and
//! dependency lists are exactly where new paths first appear - and every other
//! chunk (JSON, Lua, plain text, string tables inside binaries) goes through a
//! printable-run scan that keeps path-shaped tokens. Feed the result to the
//! hunt via [`crate::guessers::SeedStrings`]; the other guessers mutate
//! whatever resolves.

use std::collections::HashSet;
use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ltk_meta::{Bin, PropertyValueEnum};
use ltk_wad::{decompress_raw, Wad, WadChunk, WadError};
use rayon::prelude::*;

/// What [`mine_wad`] pulled out of one archive.
#[derive(Debug, Default)]
pub struct WadMineReport {
    /// Deduped, sorted path-like strings mined from chunk contents.
    pub strings: Vec<Box<str>>,

    /// Every chunk's path hash (xxh64 of the lowercased game path) - the
    /// natural unknown set for a game-table hunt over this archive.
    pub chunk_hashes: Vec<u64>,

    /// Chunks whose data could not be loaded or decompressed (e.g. the
    /// unsupported satellite kind); they are skipped, not fatal.
    pub chunks_skipped: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum MineError {
    #[error("opening {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("reading WAD {path}: {source}")]
    Wad {
        path: PathBuf,
        #[source]
        source: WadError,
    },
}

/// Mine one WAD archive for seed strings and unknown chunk hashes.
pub fn mine_wad(path: &Path) -> Result<WadMineReport, MineError> {
    let file = File::open(path).map_err(|source| MineError::Open {
        path: path.into(),
        source,
    })?;
    let wad = Wad::mount(file).map_err(|source| MineError::Wad {
        path: path.into(),
        source,
    })?;
    let chunks: Vec<WadChunk> = wad.chunks().iter().copied().collect();
    let chunk_hashes: Vec<u64> = chunks.iter().map(|chunk| chunk.path_hash).collect();

    // Raw chunk reads serialize on the mutex (one file handle); decompression
    // and string extraction fan out across the rayon pool.
    let wad = Mutex::new(wad);
    let (strings, chunks_skipped) = chunks
        .par_iter()
        .map(|chunk| {
            let raw = wad.lock().unwrap().load_chunk_raw(chunk);
            let data = raw.and_then(|raw| {
                decompress_raw(&raw, chunk.compression_type, chunk.uncompressed_size)
            });
            match data {
                Ok(data) => {
                    let mut strings = HashSet::new();
                    extract_strings(&data, &mut strings);
                    (strings, 0usize)
                }
                Err(_) => (HashSet::new(), 1),
            }
        })
        .reduce(
            || (HashSet::new(), 0),
            |(mut acc, skipped), (found, s)| {
                acc.extend(found);
                (acc, skipped + s)
            },
        );

    let mut strings: Vec<Box<str>> = strings.into_iter().collect();
    strings.sort_unstable();

    Ok(WadMineReport {
        strings,
        chunk_hashes,
        chunks_skipped,
    })
}

/// Pull every string that could name a game asset out of one chunk's bytes.
pub fn extract_strings(data: &[u8], out: &mut HashSet<Box<str>>) {
    if data.len() >= 4 && (&data[..4] == b"PROP" || &data[..4] == b"PTCH") {
        if let Ok(bin) = Bin::from_reader(&mut Cursor::new(data)) {
            bin_strings(bin, out);
            return;
        }
        // A chunk that only looks like a bin still gets the raw scan below.
    }

    scan_tokens(data, out);
}

/// A parsed `.bin`'s literal strings: the dependency list plus every
/// string-typed property value, recursively.
fn bin_strings(bin: Bin, out: &mut HashSet<Box<str>>) {
    for dep in bin.dependencies {
        out.insert(dep.into());
    }
    for (_, object) in bin.objects {
        for (_, value) in object.properties {
            walk(value, out);
        }
    }
}

fn walk(value: PropertyValueEnum, out: &mut HashSet<Box<str>>) {
    match value {
        // A literal value is a candidate itself and may embed more paths
        // (space-separated lists, sentences quoting an asset).
        PropertyValueEnum::String(s) => {
            scan_tokens(s.value.as_bytes(), out);
            out.insert(s.value.into());
        }

        PropertyValueEnum::Struct(s) => {
            for (_, value) in s.properties {
                walk(value, out);
            }
        }
        PropertyValueEnum::Embedded(e) => {
            for (_, value) in e.0.properties {
                walk(value, out);
            }
        }
        PropertyValueEnum::Container(c) => {
            for item in c.into_items() {
                walk(item, out);
            }
        }
        PropertyValueEnum::UnorderedContainer(c) => {
            for item in c.0.into_items() {
                walk(item, out);
            }
        }
        PropertyValueEnum::Optional(o) => {
            if let Some(inner) = o.into_inner() {
                walk(inner, out);
            }
        }
        PropertyValueEnum::Map(m) => {
            for (key, value) in m.into_entries() {
                walk(key, out);
                walk(value, out);
            }
        }

        // Everything else is numeric (incl. Hash/WadChunkLink/ObjectLink -
        // those reference paths by hash, not by string).
        _ => {}
    }
}

/// Bytes a path can be spelled from. Runs of these are candidate tokens.
fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-')
}

/// Keep a token only if it is shaped like a path: long enough and containing
/// a directory separator or an extension dot.
fn keep(token: &str) -> bool {
    (4..=300).contains(&token.len()) && token.contains(['/', '.'])
}

/// Scan arbitrary bytes for path-shaped tokens (works on JSON, Lua, plain
/// text, and string tables embedded in binary formats alike).
fn scan_tokens(data: &[u8], out: &mut HashSet<Box<str>>) {
    let mut start = None;
    for (i, &b) in data.iter().enumerate() {
        match (is_path_byte(b), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                push_token(&data[s..i], out);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        push_token(&data[s..], out);
    }
}

fn push_token(bytes: &[u8], out: &mut HashSet<Box<str>>) {
    // Runs are ASCII by construction, so the conversion cannot fail.
    let token = std::str::from_utf8(bytes).unwrap_or_default();

    // Strip sentence punctuation a grep drags along; no real path starts or
    // ends with `.` or `-`.
    let token = token.trim_matches(['.', '-']);
    if keep(token) {
        out.insert(Box::from(token));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltk_meta::property::NoMeta;
    use ltk_meta::BinObject;

    fn extracted(data: &[u8]) -> HashSet<Box<str>> {
        let mut out = HashSet::new();
        extract_strings(data, &mut out);
        out
    }

    #[test]
    fn scans_path_tokens_out_of_json_and_noise() {
        let data = br#"{"icon": "assets/characters/ahri/hud/ahri_circle.png", "n": 3}
            garbage \x00\x01 see data/spells.bin. trailing sentence."#;
        let out = extracted(data);

        assert!(out.contains("assets/characters/ahri/hud/ahri_circle.png"));
        assert!(out.contains("data/spells.bin"), "sentence dot trimmed");
        assert!(
            !out.iter().any(|s| s.contains("garbage")),
            "non-path tokens are dropped: {out:?}"
        );
    }

    #[test]
    fn parses_bin_chunks_for_string_values_and_dependencies() {
        let object = BinObject::<NoMeta>::builder(1u32, 2u32)
            .property(
                3u32,
                PropertyValueEnum::String(
                    "ASSETS/Characters/Ahri/Skins/Base/Ahri_TX_CM.dds".into(),
                ),
            )
            .property(4u32, PropertyValueEnum::String("not a path".into()))
            .build();
        let bin = Bin::builder()
            .dependency("DATA/Characters/Ahri/Ahri.bin")
            .object(object)
            .build();
        let mut bytes = Cursor::new(Vec::new());
        bin.to_writer(&mut bytes).unwrap();

        let out = extracted(&bytes.into_inner());

        assert!(out.contains("DATA/Characters/Ahri/Ahri.bin"));
        assert!(out.contains("ASSETS/Characters/Ahri/Skins/Base/Ahri_TX_CM.dds"));
        assert!(
            out.contains("not a path"),
            "literal bin strings are kept verbatim even when not path-shaped"
        );
    }

    #[test]
    fn malformed_bin_magic_falls_back_to_the_raw_scan() {
        let data = b"PROPxxxx assets/characters/ahri/ahri.skn xxxx";
        let out = extracted(data);

        assert!(out.contains("assets/characters/ahri/ahri.skn"));
    }
}
