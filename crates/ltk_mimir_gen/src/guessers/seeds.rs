//! Verbatim checking of externally mined strings.

use rayon::prelude::*;

use crate::{CandidateSink, GuessContext, Guesser};

/// Checks candidate strings mined outside the hunt (WAD chunks via
/// [`crate::mine_wad`], game JSON, community wordlists) as-is.
///
/// This is the entry point for every mining pipeline: mine strings however
/// you like, feed them here, and the hunt's other guessers mutate whatever
/// resolves.
pub struct SeedStrings {
    seeds: Vec<Box<str>>,
}

impl SeedStrings {
    pub fn new<I>(seeds: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<Box<str>>,
    {
        Self {
            seeds: seeds.into_iter().map(Into::into).collect(),
        }
    }
}

impl Guesser for SeedStrings {
    fn name(&self) -> &str {
        "seed-strings"
    }

    fn guess(&self, _ctx: &GuessContext, sink: &CandidateSink) {
        self.seeds.par_iter().for_each(|seed| {
            sink.check(seed);
        });
    }
}
