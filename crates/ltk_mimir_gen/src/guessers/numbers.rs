//! Numeric-range substitution (CDTB's `substitute_numbers`).

use std::fmt::Write as _;

use rayon::prelude::*;

use super::util::{dec_len, digit_runs};
use crate::{CandidateSink, GuessContext, Guesser};

/// Sweep every digit run of every known path through `0..=max`:
/// `bar_2.dds` → `bar_0.dds` … `bar_200.dds`. Runs are swept independently
/// (no cross product); zero-padded runs also get a same-width padded variant
/// (`icon_03.png` → `icon_07.png` as well as `icon_7.png`).
pub struct NumericRange {
    max: u32,
}

impl NumericRange {
    pub fn new(max: u32) -> Self {
        Self { max }
    }
}

impl Guesser for NumericRange {
    fn name(&self) -> &str {
        "numeric-range"
    }

    fn guess(&self, ctx: &GuessContext, sink: &CandidateSink) {
        ctx.known_paths()
            .par_iter()
            .for_each_init(String::new, |buf, path| {
                for (start, end) in digit_runs(path) {
                    let width = end - start;
                    for n in 0..=self.max {
                        buf.clear();
                        buf.push_str(&path[..start]);
                        write!(buf, "{n}").unwrap();
                        buf.push_str(&path[end..]);
                        sink.check(buf);
                        if dec_len(n) < width {
                            buf.clear();
                            buf.push_str(&path[..start]);
                            write!(buf, "{n:0width$}").unwrap();
                            buf.push_str(&path[end..]);
                            sink.check(buf);
                        }
                    }
                }
            });
    }
}
