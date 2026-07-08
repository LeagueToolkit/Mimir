//! Extension and texture-prefix variants.

use rayon::prelude::*;

use super::util::split_dir;
use crate::{CandidateSink, GuessContext, Guesser};

/// Swap every known path's extension for every extension in
/// [`GuessContext::extensions`]: `bar.png` → `bar.dds`, `bar.tex`, …
pub struct ExtensionSwap;

impl Guesser for ExtensionSwap {
    fn name(&self) -> &str {
        "extension-swap"
    }

    fn guess(&self, ctx: &GuessContext, sink: &CandidateSink) {
        let extensions = ctx.extensions();
        ctx.known_paths()
            .par_iter()
            .for_each_init(String::new, |buf, path| {
                let (_, basename) = split_dir(path);
                let Some(dot) = basename.rfind('.').filter(|&i| i > 0) else {
                    return;
                };

                let base = &path[..path.len() - basename.len() + dot];
                let current = &basename[dot + 1..];
                for ext in extensions {
                    if **ext == *current {
                        continue;
                    }
                    buf.clear();
                    buf.push_str(base);
                    buf.push('.');
                    buf.push_str(ext);
                    sink.check(buf);
                }
            });
    }
}

/// Texture-resolution prefixes on basenames (CDTB's `2x_`/`4x_` checks):
/// add each of `2x_ 4x_ sd_` to bare basenames, and strip/swap them on
/// basenames that already carry one.
pub struct PrefixVariants;

const TEX_PREFIXES: &[&str] = &["2x_", "4x_", "sd_"];

impl Guesser for PrefixVariants {
    fn name(&self) -> &str {
        "prefix-variants"
    }

    fn guess(&self, ctx: &GuessContext, sink: &CandidateSink) {
        ctx.known_paths()
            .par_iter()
            .for_each_init(String::new, |buf, path| {
                let (dir, basename) = split_dir(path);
                let existing = TEX_PREFIXES
                    .iter()
                    .find(|prefix| basename.starts_with(**prefix))
                    .copied();
                let bare = existing.map_or(basename, |prefix| &basename[prefix.len()..]);
                if bare.is_empty() {
                    return;
                }

                if existing.is_some() {
                    buf.clear();
                    buf.push_str(dir);
                    buf.push_str(bare);
                    sink.check(buf);
                }
                for prefix in TEX_PREFIXES {
                    if Some(*prefix) == existing {
                        continue;
                    }
                    buf.clear();
                    buf.push_str(dir);
                    buf.push_str(prefix);
                    buf.push_str(bare);
                    sink.check(buf);
                }
            });
    }
}
