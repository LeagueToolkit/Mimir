//! Wordlist-driven basename mutation (CDTB's `substitute_basename_words` /
//! `add_basename_word`).
//!
//! Both guessers cost `O(|corpus| × |vocabulary|)` candidates - hours on the
//! full game table - so they are opt-in rather than part of
//! [`crate::Hunt::default_game`].

use rayon::prelude::*;

use super::util::{split_dir, split_ext, token_spans};
use crate::{CandidateSink, GuessContext, Guesser};

/// Replace each `_ - .`-separated token of every known stem with every
/// vocabulary word: `aatrox_base.dds` + `victorious` →
/// `victorious_base.dds`, `aatrox_victorious.dds`.
pub struct WordSubstitution;

impl Guesser for WordSubstitution {
    fn name(&self) -> &str {
        "word-substitution"
    }

    fn guess(&self, ctx: &GuessContext, sink: &CandidateSink) {
        let words = ctx.wordlist();
        ctx.known_paths()
            .par_iter()
            .for_each_init(String::new, |buf, path| {
                let (dir, basename) = split_dir(path);
                let (stem, ext) = split_ext(basename);
                for (start, end) in token_spans(stem) {
                    for word in words {
                        if **word == stem[start..end] {
                            continue;
                        }
                        buf.clear();
                        buf.push_str(dir);
                        buf.push_str(&stem[..start]);
                        buf.push_str(word);
                        buf.push_str(&stem[end..]);
                        buf.push_str(ext);
                        sink.check(buf);
                    }
                }
            });
    }
}

/// Glue each vocabulary word onto every known stem with `_` and `-`:
/// `ring.dds` + `glow` → `ring_glow.dds`, `glow_ring.dds`, `ring-glow.dds`, …
pub struct WordAdd;

impl Guesser for WordAdd {
    fn name(&self) -> &str {
        "word-add"
    }

    fn guess(&self, ctx: &GuessContext, sink: &CandidateSink) {
        let words = ctx.wordlist();
        ctx.known_paths()
            .par_iter()
            .for_each_init(String::new, |buf, path| {
                let (dir, basename) = split_dir(path);
                let (stem, ext) = split_ext(basename);
                if stem.is_empty() {
                    return;
                }

                for word in words {
                    for sep in ['_', '-'] {
                        buf.clear();
                        buf.push_str(dir);
                        buf.push_str(stem);
                        buf.push(sep);
                        buf.push_str(word);
                        buf.push_str(ext);
                        sink.check(buf);

                        buf.clear();
                        buf.push_str(dir);
                        buf.push_str(word);
                        buf.push(sep);
                        buf.push_str(stem);
                        buf.push_str(ext);
                        sink.check(buf);
                    }
                }
            });
    }
}
