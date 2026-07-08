//! `mimir merge`: sorted dedup merge of CDragon txt hash lists - the txt-level
//! maintenance step that replaces CDTB's shell pipelines.
//!
//! Lines are parsed, merged, sorted by `(hash, path)`, and re-emitted
//! zero-padded to the widest hash width seen in the inputs. Identical lines
//! collapse to one; the same hash mapped to two *different* paths is kept and
//! warned about - the txt lists are the canonical source, so a merge must
//! surface conflicts, never resolve them silently.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::read_hash_lines;

pub fn run(inputs: &[PathBuf], out: Option<&Path>) -> Result<()> {
    let mut entries: Vec<(u64, Box<str>)> = Vec::new();
    let mut width = 0;
    for input in inputs {
        read_hash_lines(input, |hash, hex, path| {
            width = width.max(hex.len());
            entries.push((hash, Box::from(path)));
        })?;
    }

    entries.sort_unstable();
    entries.dedup();

    let mut conflicts = 0usize;
    for pair in entries.windows(2) {
        if pair[0].0 == pair[1].0 {
            conflicts += 1;
            eprintln!(
                "warning: {:0width$x} maps to both {:?} and {:?}",
                pair[0].0, pair[0].1, pair[1].1
            );
        }
    }

    let mut writer: BufWriter<Box<dyn Write>> = BufWriter::new(match out {
        Some(path) => {
            Box::new(File::create(path).with_context(|| format!("creating {}", path.display()))?)
        }
        None => Box::new(std::io::stdout().lock()),
    });
    for (hash, path) in &entries {
        writeln!(writer, "{hash:0width$x} {path}")?;
    }
    writer.flush()?;

    // Status goes to stderr so a stdout merge stays pipeable.
    eprintln!(
        "merged {} input(s) -> {} unique lines{}",
        inputs.len(),
        entries.len(),
        if conflicts > 0 {
            format!(" ({conflicts} hash conflict(s) kept)")
        } else {
            String::new()
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::testutil::TempDir;

    fn write_lines(path: &Path, lines: &[&str]) {
        fs::write(path, lines.join("\n") + "\n").unwrap();
    }

    fn merged(tmp: &TempDir, a: &[&str], b: &[&str]) -> String {
        let a_path = tmp.path().join("a.txt");
        let b_path = tmp.path().join("b.txt");
        let out = tmp.path().join("merged.txt");
        write_lines(&a_path, a);
        write_lines(&b_path, b);

        run(&[a_path, b_path], Some(&out)).unwrap();
        fs::read_to_string(out).unwrap()
    }

    #[test]
    fn merges_sorted_and_deduped() {
        let tmp = TempDir::new("merge-basic");
        let out = merged(
            &tmp,
            &[
                "00000000000022bb assets/bar.bin",
                "00000000000011aa assets/foo.bin",
            ],
            &[
                "00000000000011aa assets/foo.bin", // duplicate line collapses
                "00000000000033cc assets/baz.bin",
            ],
        );

        assert_eq!(
            out,
            "00000000000011aa assets/foo.bin\n\
             00000000000022bb assets/bar.bin\n\
             00000000000033cc assets/baz.bin\n"
        );
    }

    #[test]
    fn keeps_conflicting_paths_for_the_same_hash() {
        let tmp = TempDir::new("merge-conflict");
        let out = merged(
            &tmp,
            &["00000000000011aa assets/foo.bin"],
            &["00000000000011aa assets/other.bin"],
        );

        // Both lines survive; a merge never picks a winner.
        assert_eq!(
            out,
            "00000000000011aa assets/foo.bin\n00000000000011aa assets/other.bin\n"
        );
    }

    #[test]
    fn preserves_hash_width_and_trailing_spaces() {
        let tmp = TempDir::new("merge-width");
        // A u32-table list (8-hex-digit hashes) with a path that legitimately
        // ends in a space; only the line terminator may be stripped.
        let out = merged(&tmp, &["811c9dc5 ", "16b3b962 Seed "], &[]);

        assert_eq!(out, "16b3b962 Seed \n811c9dc5 \n");
    }
}
