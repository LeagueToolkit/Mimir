//! Character/skin combinatorics (CDTB's `GameHashGuesser` skin strategies).

use std::fmt::Write as _;

use rayon::prelude::*;

use super::util::{champ_of, dec_len, replace_token};
use crate::{CandidateSink, GuessContext, Guesser};

/// Two game-specific sweeps over `characters/<champ>/` paths:
///
/// 1. **Skin numbers**: every `skin<NN>` token in a path swept together
///    through `0..=max_skin` to the same target number, preserving each
///    occurrence's zero-padding
///    (`…/skins/skin07/ahri_skin7_tx.dds` → `…/skins/skin03/ahri_skin3_tx.dds`),
///    plus an all-plain variant.
/// 2. **Champion transfer**: every known path's champion token replaced with
///    every other champion seen in the corpus, so a layout known for one
///    champion is tried for all - `…/ahri/skins/skin01/ahri_tx.dds` →
///    `…/zed/skins/skin01/zed_tx.dds`.
pub struct CharacterSkin {
    max_skin: u32,
}

impl CharacterSkin {
    pub fn new(max_skin: u32) -> Self {
        Self { max_skin }
    }
}

/// Every `skin<digits>` token of `path`, as `(digits_start, digits_end)` spans.
fn skin_tokens(path: &str) -> Vec<(usize, usize)> {
    let mut tokens = Vec::new();
    let mut search = 0;
    while let Some(rel) = path[search..].find("skin") {
        let digit_start = search + rel + "skin".len();
        let digits = path[digit_start..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        if digits > 0 {
            tokens.push((digit_start, digit_start + digits));
        }
        search = digit_start;
    }
    tokens
}

impl Guesser for CharacterSkin {
    fn name(&self) -> &str {
        "character-skin"
    }

    fn guess(&self, ctx: &GuessContext, sink: &CandidateSink) {
        let champs = ctx.champions();

        // Skin-number sweep: all `skin<NN>` tokens move together to one target
        // number. Real paths keep the number consistent across dir and filename,
        // so sweeping as a group yields coherent variants even when tokens differ.
        ctx.known_paths()
            .par_iter()
            .for_each_init(String::new, |buf, path| {
                let spans = skin_tokens(path);
                let Some(&(first_start, first_end)) = spans.first() else {
                    return;
                };

                // The target that would reproduce the original path exactly -
                // defined only when every token already shares that number. Skip
                // it so the sweep never re-emits an already-known path.
                let uniform = path[first_start..first_end]
                    .parse::<u32>()
                    .ok()
                    .filter(|v| spans.iter().all(|&(s, e)| path[s..e].parse() == Ok(*v)));
                let any_padded = spans
                    .iter()
                    .any(|&(s, e)| path[s..e].parse::<u32>().is_ok_and(|v| e - s > dec_len(v)));
                for n in 0..=self.max_skin {
                    if Some(n) == uniform {
                        continue;
                    }

                    // Preserve each occurrence's width.
                    buf.clear();
                    let mut cursor = 0;
                    for &(s, e) in &spans {
                        buf.push_str(&path[cursor..s]);
                        let width = e - s;
                        write!(buf, "{n:0width$}").unwrap();
                        cursor = e;
                    }
                    buf.push_str(&path[cursor..]);
                    sink.check(buf);

                    if any_padded {
                        // All-plain variant.
                        buf.clear();
                        let mut cursor = 0;
                        for &(s, e) in &spans {
                            buf.push_str(&path[cursor..s]);
                            write!(buf, "{n}").unwrap();
                            cursor = e;
                        }
                        buf.push_str(&path[cursor..]);
                        sink.check(buf);
                    }
                }
            });

        // Champion transfer.
        ctx.known_paths()
            .par_iter()
            .for_each_init(String::new, |buf, path| {
                let Some(champ) = champ_of(path) else {
                    return;
                };

                for other in champs {
                    let other: &str = other;
                    if other == champ {
                        continue;
                    }
                    if replace_token(path, champ, other, buf) {
                        sink.check(buf);
                    }
                }
            });
    }
}
