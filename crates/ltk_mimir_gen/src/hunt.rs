//! The [`Guesser`] trait and the loop-until-dry [`Hunt`] driver.

use std::time::{Duration, Instant};

use crate::guessers::{
    CharacterSkin, CrossReference, ExtensionSwap, NumericRange, PrefixVariants, RegionLocale,
};
use crate::{CandidateSink, GuessContext};

/// One candidate-generation strategy. Implementations parallelize internally
/// with rayon and report every candidate through [`CandidateSink::check`].
pub trait Guesser: Send + Sync {
    fn name(&self) -> &str;
    fn guess(&self, ctx: &GuessContext, sink: &CandidateSink);
}

/// Per-guesser numbers for one round.
#[derive(Debug)]
pub struct GuesserStats {
    pub name: String,

    /// Candidates hashed and tested.
    pub candidates: u64,

    /// Unknown hashes resolved.
    pub found: usize,

    pub elapsed: Duration,
}

/// One full pass over all guessers.
#[derive(Debug, Default)]
pub struct RoundStats {
    pub guessers: Vec<GuesserStats>,

    /// Total hashes resolved this round.
    pub resolved: usize,
}

/// What a [`Hunt::run`] discovered.
#[derive(Debug, Default)]
pub struct HuntReport {
    /// Newly resolved `(hash, path)` pairs, in discovery order.
    pub resolved: Vec<(u64, String)>,

    pub rounds: Vec<RoundStats>,
}

/// An ordered set of guessers, run round after round until a full round
/// resolves nothing new (finds from earlier guessers feed later ones within
/// the same round, and every guesser again in the next).
#[derive(Default)]
pub struct Hunt {
    guessers: Vec<Box<dyn Guesser>>,
}

impl Hunt {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, guesser: impl Guesser + 'static) -> Self {
        self.guessers.push(Box::new(guesser));
        self
    }

    /// The game-table set: cheap, high-yield strategies. The wordlist guessers
    /// ([`crate::guessers::WordSubstitution`] / [`crate::guessers::WordAdd`])
    /// are opt-in - their cost scales with `|corpus| × |vocabulary|`.
    pub fn default_game() -> Self {
        Self::new()
            .with(ExtensionSwap)
            .with(PrefixVariants)
            .with(CrossReference)
            .with(NumericRange::new(200))
            .with(CharacterSkin::new(100))
    }

    /// The LCU-table set.
    pub fn default_lcu() -> Self {
        Self::new()
            .with(ExtensionSwap)
            .with(RegionLocale)
            .with(CrossReference)
            .with(NumericRange::new(200))
    }

    /// Run every guesser until a full round resolves nothing new or the
    /// unknown set is exhausted.
    pub fn run(&self, ctx: &mut GuessContext) -> HuntReport {
        let mut report = HuntReport::default();
        if self.guessers.is_empty() {
            return report;
        }

        loop {
            let mut round = RoundStats::default();
            for guesser in &self.guessers {
                if ctx.unknown().is_empty() {
                    break;
                }

                let sink = CandidateSink::new(ctx);
                let start = Instant::now();
                guesser.guess(ctx, &sink);
                let elapsed = start.elapsed();
                let (candidates, found) = sink.drain();

                round.guessers.push(GuesserStats {
                    name: guesser.name().to_owned(),
                    candidates,
                    found: found.len(),
                    elapsed,
                });
                round.resolved += found.len();
                ctx.promote(&found);
                report.resolved.extend(found);
            }

            let dry = round.resolved == 0;
            report.rounds.push(round);
            if dry || ctx.unknown().is_empty() {
                return report;
            }
        }
    }
}
