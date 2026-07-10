//! The hash-discovery ("hunt") engine.
//!
//! Ports CDTB's `HashGuesser` strategies as composable [`Guesser`]s,
//! parallelized with rayon. A [`GuessContext`] holds the known-path corpus and
//! the set of still-unknown hashes; each guesser mutates known paths (or checks
//! externally mined strings) and reports candidates to a [`CandidateSink`],
//! which hashes them with the table's algorithm and tests them against the
//! unknown set. [`Hunt::run`] drives rounds of all guessers until a round
//! resolves nothing new, promoting each find into the corpus so later guessers
//! build on it.
//!
//! ```no_run
//! use ltk_hashdb::{Casing, HashKind, KeyWidth};
//! use ltk_mimir_gen::{GuessContext, Hunt};
//!
//! let mut ctx = GuessContext::new(HashKind::Xxh64, Casing::Insensitive, KeyWidth::U64);
//! ctx.add_known(["assets/characters/ahri/skins/skin01/ahri_tx.dds".to_owned()]);
//! ctx.add_unknown([0x123456789abcdef0]); // e.g. mined from a WAD's chunk table
//! let report = Hunt::default_game().run(&mut ctx);
//! for (hash, path) in &report.resolved {
//!     println!("{hash:016x} {path}");
//! }
//! ```

mod context;
pub mod guessers;
mod hunt;
pub mod mine;
mod unknown;

pub use context::{CandidateSink, GuessContext};
pub use hunt::{Guesser, GuesserStats, Hunt, HuntReport, RoundStats};
pub use mine::{mine_wad, MineError, WadMineReport};
pub use unknown::UnknownSet;
