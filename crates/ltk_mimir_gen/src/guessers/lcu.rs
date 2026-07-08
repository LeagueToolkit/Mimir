//! LCU region/locale permutation (CDTB's plugin-path regionalization).

use rayon::prelude::*;

use crate::{CandidateSink, GuessContext, Guesser};

const REGIONS: &[&str] = &[
    "global", "br", "cn", "eune", "euw", "garena2", "garena3", "id", "jp", "kr", "la1", "la2",
    "lan", "las", "na", "oc1", "oce", "pbe", "ph", "ru", "sg", "tencent", "th", "tr", "tw", "vn",
];

const LOCALES: &[&str] = &[
    "default", "cs_cz", "de_de", "el_gr", "en_au", "en_gb", "en_ph", "en_sg", "en_us", "es_ar",
    "es_es", "es_mx", "fr_fr", "hu_hu", "id_id", "it_it", "ja_jp", "ko_kr", "ms_my", "pl_pl",
    "pt_br", "ro_ro", "ru_ru", "th_th", "tr_tr", "vi_vn", "vn_vn", "zh_cn", "zh_my", "zh_tw",
];

/// Does this segment look like a locale (`default` or `xx_yy`)?
fn is_locale(segment: &str) -> bool {
    segment == "default"
        || (segment.len() == 5
            && segment.as_bytes()[2] == b'_'
            && segment
                .bytes()
                .enumerate()
                .all(|(i, b)| i == 2 || b.is_ascii_lowercase()))
}

/// Swap every `<region>/<locale>` segment pair of every known LCU path for
/// every known combination: `plugins/x/global/default/index.html` →
/// `plugins/x/ru/ru_ru/index.html`, ….
pub struct RegionLocale;

impl Guesser for RegionLocale {
    fn name(&self) -> &str {
        "region-locale"
    }

    fn guess(&self, ctx: &GuessContext, sink: &CandidateSink) {
        ctx.known_paths()
            .par_iter()
            .for_each_init(String::new, |buf, path| {
                // Byte span of each `<region>/<locale>` pair in this path.
                let mut prev: Option<(usize, &str)> = None;
                let mut offset = 0;
                for segment in path.split('/') {
                    let start = offset;
                    offset += segment.len() + 1;
                    if let Some((region_start, region)) = prev {
                        if REGIONS.contains(&region) && is_locale(segment) {
                            let end = start + segment.len();
                            for r in REGIONS {
                                for l in LOCALES {
                                    if *r == region && *l == segment {
                                        continue;
                                    }
                                    buf.clear();
                                    buf.push_str(&path[..region_start]);
                                    buf.push_str(r);
                                    buf.push('/');
                                    buf.push_str(l);
                                    buf.push_str(&path[end..]);
                                    sink.check(buf);
                                }
                            }
                        }
                    }
                    prev = Some((start, segment));
                }
            });
    }
}
