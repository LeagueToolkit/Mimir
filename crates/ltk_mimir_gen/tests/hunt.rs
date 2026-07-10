//! End-to-end hunts over synthetic corpora: seed a few known paths, hash the
//! target paths into the unknown set, and assert the guessers rediscover them.

use ltk_hashdb::{Casing, HashKind, KeyWidth};
use ltk_mimir_gen::guessers::{
    CharacterSkin, ExtensionSwap, NumericRange, PrefixVariants, RegionLocale, SeedStrings, WordAdd,
    WordSubstitution,
};
use ltk_mimir_gen::{GuessContext, Hunt};

fn xxh64(s: &str) -> u64 {
    HashKind::Xxh64.hash(s, Casing::Insensitive, KeyWidth::U64)
}

fn ctx_with(known: &[&str], targets: &[&str]) -> GuessContext {
    let mut ctx = GuessContext::new(HashKind::Xxh64, Casing::Insensitive, KeyWidth::U64);
    ctx.add_known(known.iter().copied().map(Box::from));
    ctx.add_unknown(targets.iter().map(|t| xxh64(t)));
    ctx
}

fn assert_resolves(hunt: &Hunt, known: &[&str], targets: &[&str]) {
    let mut ctx = ctx_with(known, targets);
    let report = hunt.run(&mut ctx);

    for target in targets {
        assert!(
            report
                .resolved
                .iter()
                .any(|(h, p)| *h == xxh64(target) && p == target),
            "hunt failed to resolve {target:?}; resolved: {:?}",
            report.resolved
        );
    }

    assert!(ctx.unknown().is_empty());
}

#[test]
fn seed_strings_check_verbatim() {
    let hunt = Hunt::new().with(SeedStrings::new(["mined/from/a/wad.dds".to_owned()]));
    assert_resolves(&hunt, &[], &["mined/from/a/wad.dds"]);
}

#[test]
fn extension_swap() {
    let hunt = Hunt::new().with(ExtensionSwap);
    assert_resolves(
        &hunt,
        &["assets/foo/bar.png"],
        &["assets/foo/bar.dds", "assets/foo/bar.tex"],
    );
}

#[test]
fn extension_swap_learns_corpus_extensions() {
    // `.customext` is not built in; it must be mined from the corpus.
    let hunt = Hunt::new().with(ExtensionSwap);
    assert_resolves(
        &hunt,
        &["assets/foo/bar.png", "assets/other/thing.customext"],
        &["assets/foo/bar.customext"],
    );
}

#[test]
fn numeric_range_plain_and_padded() {
    let hunt = Hunt::new().with(NumericRange::new(10));
    assert_resolves(
        &hunt,
        &["assets/foo/bar_2.dds", "ui/icon_03.png"],
        &["assets/foo/bar_7.dds", "ui/icon_07.png", "ui/icon_7.png"],
    );
}

#[test]
fn prefix_variants_add_strip_swap() {
    let hunt = Hunt::new().with(PrefixVariants);
    assert_resolves(
        &hunt,
        &["assets/ui/icon.dds", "assets/ui/2x_orb.png"],
        &[
            "assets/ui/2x_icon.dds",
            "assets/ui/orb.png",
            "assets/ui/4x_orb.png",
        ],
    );
}

#[test]
fn word_substitution_uses_corpus_vocabulary() {
    // `riven` only appears in an unrelated path; substitution carries it over.
    let hunt = Hunt::new().with(WordSubstitution);
    assert_resolves(
        &hunt,
        &["assets/foo/aatrox_base.dds", "assets/words/riven.txt"],
        &["assets/foo/riven_base.dds", "assets/foo/aatrox_words.dds"],
    );
}

#[test]
fn word_add_glues_with_separators() {
    let hunt = Hunt::new().with(WordAdd);
    assert_resolves(
        &hunt,
        &["foo/ring.dds", "x/glow.txt"],
        &["foo/ring_glow.dds", "foo/glow-ring.dds"],
    );
}

#[test]
fn character_skin_sweep_and_transfer() {
    let hunt = Hunt::new().with(CharacterSkin::new(20));
    assert_resolves(
        &hunt,
        &[
            "assets/characters/ahri/skins/skin01/ahri_skin01_tx.dds",
            "assets/characters/zed/zed_base.bin",
        ],
        &[
            // Skin-number sweep: every skinNN occurrence replaced together,
            // padded to the template's width and plain.
            "assets/characters/ahri/skins/skin07/ahri_skin07_tx.dds",
            "assets/characters/ahri/skins/skin7/ahri_skin7_tx.dds",
            // Champion transfer: ahri's layout tried for zed, all tokens replaced.
            "assets/characters/zed/skins/skin01/zed_skin01_tx.dds",
        ],
    );
}

#[test]
fn character_skin_sweeps_mismatched_tokens_together() {
    // A path whose skin tokens disagree (dir `skin1`, filename `skin2`) must
    // still be swept as one group, so the coherent `skin7/..._skin7_...`
    // variant is produced rather than leaving the filename token fixed.
    let hunt = Hunt::new().with(CharacterSkin::new(20));
    assert_resolves(
        &hunt,
        &["assets/characters/ahri/skins/skin1/ahri_skin2_tx.dds"],
        &["assets/characters/ahri/skins/skin7/ahri_skin7_tx.dds"],
    );
}

#[test]
fn region_locale_permutation() {
    let hunt = Hunt::new().with(RegionLocale);
    assert_resolves(
        &hunt,
        &["plugins/rcp-fe-lol-champ-select/global/default/index.html"],
        &[
            "plugins/rcp-fe-lol-champ-select/ru/ru_ru/index.html",
            "plugins/rcp-fe-lol-champ-select/global/ja_jp/index.html",
        ],
    );
}

#[test]
fn rounds_compose_until_dry() {
    // b_2.dds needs two guessers chained: numeric finds b_2.png in round 1,
    // extension swap (which already ran this round) picks it up in round 2.
    let hunt = Hunt::new().with(ExtensionSwap).with(NumericRange::new(5));
    let mut ctx = ctx_with(&["a/b_1.png"], &["a/b_2.png", "a/b_2.dds"]);
    let report = hunt.run(&mut ctx);

    assert!(ctx.unknown().is_empty());
    assert_eq!(report.resolved.len(), 2);
    // Round 1 resolves b_2.png, round 2 resolves b_2.dds; a final dry round
    // is not needed because the unknown set empties.
    assert_eq!(report.rounds.len(), 2);
    assert_eq!(report.rounds[0].resolved, 1);
    assert_eq!(report.rounds[1].resolved, 1);
}

#[test]
fn dry_round_terminates() {
    let hunt = Hunt::new().with(ExtensionSwap);
    let mut ctx = ctx_with(&["a/b.png"], &["never/found.dds"]);
    let report = hunt.run(&mut ctx);

    assert!(report.resolved.is_empty());
    assert_eq!(report.rounds.len(), 1);
    assert_eq!(ctx.unknown().len(), 1);
}

#[test]
fn empty_inputs_are_safe() {
    let mut ctx = GuessContext::new(HashKind::Xxh64, Casing::Insensitive, KeyWidth::U64);
    let report = Hunt::default_game().run(&mut ctx);
    assert!(report.resolved.is_empty());

    // No guessers at all.
    let mut ctx = ctx_with(&["a/b.png"], &["a/c.png"]);
    let report = Hunt::new().run(&mut ctx);
    assert!(report.resolved.is_empty());
}

#[test]
fn fnv1a32_tables_hash_case_insensitively() {
    // Bin tables keep original-case strings but hash lowercased.
    let target = "Data/Spells/AhriOrbMissile.lua";
    let mut ctx = GuessContext::new(HashKind::Fnv1a32, Casing::Insensitive, KeyWidth::U32);
    ctx.add_known(["Data/Spells/AhriOrbMissile.luabin".to_owned()]);
    ctx.add_unknown([HashKind::Fnv1a32.hash(target, Casing::Insensitive, KeyWidth::U32)]);
    let report = Hunt::new().with(ExtensionSwap).run(&mut ctx);

    assert_eq!(report.resolved.len(), 1);
    assert_eq!(report.resolved[0].1, target);
}

#[test]
fn default_game_preset_end_to_end() {
    assert_resolves(
        &Hunt::default_game(),
        &[
            "assets/characters/ahri/skins/skin11/ahri_skin11_cm.dds",
            "assets/characters/lulu/lulu_square.png",
        ],
        &[
            "assets/characters/lulu/skins/skin11/lulu_skin11_cm.dds",
            "assets/characters/ahri/ahri_square.png",
            "assets/characters/ahri/skins/skin3/ahri_skin11_cm.dds",
        ],
    );
}

#[test]
fn report_counts_candidates() {
    let hunt = Hunt::new().with(ExtensionSwap);
    let mut ctx = ctx_with(&["a/b.png"], &["a/b.dds"]);
    let report = hunt.run(&mut ctx);
    let stats = &report.rounds[0].guessers[0];

    assert_eq!(stats.name, "extension-swap");
    assert!(stats.candidates > 0);
    assert_eq!(stats.found, 1);
}
