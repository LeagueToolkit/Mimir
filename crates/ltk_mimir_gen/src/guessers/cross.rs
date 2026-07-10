//! Cross-referencing the game and LCU corpora (CDTB's lcu ↔ game derivation).
//!
//! The LCU's `rcp-be-lol-game-data` plugin serves game assets under a
//! `plugins/rcp-be-lol-game-data/<region>/<locale>/` mount, so each corpus
//! names paths the other is missing: a known LCU path unmounts to a game
//! `assets/…` / `data/…` candidate, and a known game path mounts to an LCU
//! candidate under `global/default` (a hit there feeds
//! [`super::RegionLocale`], which permutes the other region/locale pairs).

use rayon::prelude::*;

use crate::{CandidateSink, GuessContext, Guesser};

const PLUGIN_MOUNT: &str = "plugins/rcp-be-lol-game-data/";

/// Derive game paths from known LCU paths and vice versa. Cheap (at most two
/// candidates per corpus path) and useful in both tables' hunts, since the
/// candidates that miss the current table simply never resolve.
pub struct CrossReference;

impl Guesser for CrossReference {
    fn name(&self) -> &str {
        "cross-reference"
    }

    fn guess(&self, ctx: &GuessContext, sink: &CandidateSink) {
        ctx.known_paths()
            .par_iter()
            .for_each_init(String::new, |buf, path| {
                // lcu → game: unmount the plugin prefix.
                if let Some(rest) = strip_plugin_mount(path) {
                    if rest.starts_with("assets/") || rest.starts_with("data/") {
                        sink.check(rest);
                    }
                }

                // game → lcu: mount the asset under the plugin.
                if path.starts_with("assets/") || path.starts_with("data/") {
                    buf.clear();
                    buf.push_str(PLUGIN_MOUNT);
                    buf.push_str("global/default/");
                    buf.push_str(path);
                    sink.check(buf);
                }
            });
    }
}

/// `plugins/rcp-be-lol-game-data/<region>/<locale>/<rest>` → `<rest>`.
fn strip_plugin_mount(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(PLUGIN_MOUNT)?;
    let (_region, rest) = rest.split_once('/')?;
    let (_locale, rest) = rest.split_once('/')?;

    Some(rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltk_hashdb::{Casing, HashKind, KeyWidth};

    fn hunt(known: &[&str], unknown_paths: &[&str]) -> Vec<String> {
        let mut ctx = GuessContext::new(HashKind::Xxh64, Casing::Insensitive, KeyWidth::U64);
        ctx.add_known(known.iter().map(|s| s.to_string()));
        let hashes: Vec<u64> = unknown_paths
            .iter()
            .map(|p| ctx.hash_candidate(p))
            .collect();
        ctx.add_unknown(hashes);

        let sink = CandidateSink::new(&ctx);
        CrossReference.guess(&ctx, &sink);

        let mut found: Vec<String> = sink.drain().1.into_iter().map(|(_, p)| p).collect();
        found.sort_unstable();
        found
    }

    #[test]
    fn derives_game_paths_from_lcu_paths() {
        let found = hunt(
            &["plugins/rcp-be-lol-game-data/global/default/assets/characters/ahri/ahri_circle.png"],
            &["assets/characters/ahri/ahri_circle.png"],
        );
        assert_eq!(found, ["assets/characters/ahri/ahri_circle.png"]);
    }

    #[test]
    fn derives_lcu_paths_from_game_paths() {
        let found = hunt(
            &["assets/characters/ahri/ahri_circle.png"],
            &["plugins/rcp-be-lol-game-data/global/default/assets/characters/ahri/ahri_circle.png"],
        );
        assert_eq!(
            found,
            ["plugins/rcp-be-lol-game-data/global/default/assets/characters/ahri/ahri_circle.png"]
        );
    }

    #[test]
    fn ignores_paths_outside_the_asset_mount() {
        let found = hunt(
            &[
                "plugins/rcp-fe-lol-loot/global/default/index.html",
                "plugins/rcp-be-lol-game-data/global/default/v1/champion-summary.json",
            ],
            &["v1/champion-summary.json", "index.html"],
        );
        assert!(found.is_empty(), "{found:?}");
    }
}
