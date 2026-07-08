//! The hunt's shared inputs ([`GuessContext`]) and match collector ([`CandidateSink`]).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use ltk_hashdb::{HashKind, KeyWidth};

use crate::guessers::util::champ_of;
use crate::UnknownSet;

/// Extensions that exist in game/lcu assets even when no known path uses them
/// yet; merged with every extension seen in the known corpus.
const BUILTIN_EXTENSIONS: &[&str] = &[
    "anm",
    "bin",
    "bnk",
    "cfg",
    "dat",
    "dds",
    "jpg",
    "json",
    "lua",
    "luabin",
    "mapgeo",
    "ogg",
    "png",
    "preload",
    "scb",
    "sco",
    "skl",
    "skn",
    "stringtable",
    "svg",
    "tex",
    "tga",
    "troy",
    "troybin",
    "ttf",
    "txt",
    "wav",
    "webm",
    "wgeo",
    "wpk",
];

/// Everything a [`crate::Guesser`] draws candidates from: the known-path corpus,
/// the set of still-unknown hashes, and the table's hash algorithm.
///
/// [`crate::Hunt::run`] grows the corpus and shrinks the unknown set as rounds
/// resolve hashes.
pub struct GuessContext {
    hash_kind: HashKind,
    key_width: KeyWidth,
    known: Vec<Box<str>>,
    unknown: UnknownSet,
    // Derived from `known`; rebuilt lazily after each promotion.
    wordlist: OnceLock<Vec<Box<str>>>,
    extensions: OnceLock<Vec<Box<str>>>,
    champions: OnceLock<Vec<Box<str>>>,
}

impl GuessContext {
    pub fn new(hash_kind: HashKind, key_width: KeyWidth) -> Self {
        Self {
            hash_kind,
            key_width,
            known: Vec::new(),
            unknown: UnknownSet::new(Vec::new()),
            wordlist: OnceLock::new(),
            extensions: OnceLock::new(),
            champions: OnceLock::new(),
        }
    }

    /// Add already-resolved paths (the corpus guessers mine for templates and words).
    pub fn add_known<I>(&mut self, paths: I)
    where
        I: IntoIterator,
        I::Item: Into<Box<str>>,
    {
        self.known.extend(paths.into_iter().map(Into::into));

        self.wordlist = OnceLock::new();
        self.extensions = OnceLock::new();
        self.champions = OnceLock::new();
    }

    /// Add target hashes (present in game data, absent from the hash table).
    /// The caller is responsible for excluding hashes that are already known.
    pub fn add_unknown<I: IntoIterator<Item = u64>>(&mut self, hashes: I) {
        let mut keys = std::mem::take(&mut self.unknown).into_keys();
        keys.extend(hashes);

        self.unknown = UnknownSet::new(keys);
    }

    pub fn hash_kind(&self) -> HashKind {
        self.hash_kind
    }

    pub fn key_width(&self) -> KeyWidth {
        self.key_width
    }

    pub fn known_paths(&self) -> &[Box<str>] {
        &self.known
    }

    pub fn unknown(&self) -> &UnknownSet {
        &self.unknown
    }

    /// Hash a candidate with this table's algorithm.
    pub fn hash_candidate(&self, candidate: &str) -> u64 {
        self.hash_kind.hash(candidate, self.key_width)
    }

    /// Vocabulary mined from the known corpus: path segments split on
    /// `/ _ - . ` (space), minus single characters and pure numbers.
    /// Sorted and deduped; rebuilt after each promotion round.
    pub fn wordlist(&self) -> &[Box<str>] {
        self.wordlist.get_or_init(|| {
            let mut words: HashSet<&str> = HashSet::new();
            for path in &self.known {
                for token in path.split(['/', '_', '-', '.', ' ']) {
                    if token.len() >= 2 && !token.bytes().all(|b| b.is_ascii_digit()) {
                        words.insert(token);
                    }
                }
            }

            let mut words: Vec<Box<str>> = words.into_iter().map(Box::from).collect();
            words.sort_unstable();
            words
        })
    }

    /// Champion directory names mined from `characters/<champ>/` corpus paths,
    /// sorted and deduped. Rebuilt after each promotion round.
    pub fn champions(&self) -> &[Box<str>] {
        self.champions.get_or_init(|| {
            let mut champs: HashSet<&str> = HashSet::new();
            for path in &self.known {
                if let Some(champ) = champ_of(path) {
                    champs.insert(champ);
                }
            }

            let mut champs: Vec<Box<str>> = champs.into_iter().map(Box::from).collect();
            champs.sort_unstable();
            champs
        })
    }

    /// File extensions (no leading dot): everything seen in the known corpus
    /// merged with [`BUILTIN_EXTENSIONS`]. Sorted and deduped.
    pub fn extensions(&self) -> &[Box<str>] {
        self.extensions.get_or_init(|| {
            let mut exts: HashSet<&str> = BUILTIN_EXTENSIONS.iter().copied().collect();
            for path in &self.known {
                let basename = path.rsplit('/').next().unwrap_or(path);
                if let Some((_, ext)) = basename.rsplit_once('.') {
                    if (1..=11).contains(&ext.len())
                        && ext.bytes().all(|b| b.is_ascii_alphanumeric())
                    {
                        exts.insert(ext);
                    }
                }
            }

            let mut exts: Vec<Box<str>> = exts.into_iter().map(Box::from).collect();
            exts.sort_unstable();
            exts
        })
    }

    /// Move freshly resolved entries into the corpus and out of the unknown set.
    pub(crate) fn promote(&mut self, resolved: &[(u64, String)]) {
        if resolved.is_empty() {
            return;
        }

        let remove: HashSet<u64> = resolved.iter().map(|(hash, _)| *hash).collect();
        let keys = self
            .unknown
            .keys()
            .iter()
            .copied()
            .filter(|key| !remove.contains(key))
            .collect();
        self.unknown = UnknownSet::new(keys);

        self.add_known(resolved.iter().map(|(_, path)| path.as_str()));
    }
}

/// Where guessers report candidates. Hashes the candidate, tests it against
/// the unknown set, and collects hits. Shared across rayon threads.
pub struct CandidateSink<'a> {
    hash_kind: HashKind,
    key_width: KeyWidth,
    unknown: &'a UnknownSet,
    tried: AtomicU64,
    found: Mutex<Vec<(u64, String)>>,
}

impl<'a> CandidateSink<'a> {
    pub fn new(ctx: &'a GuessContext) -> Self {
        Self {
            hash_kind: ctx.hash_kind,
            key_width: ctx.key_width,
            unknown: &ctx.unknown,
            tried: AtomicU64::new(0),
            found: Mutex::new(Vec::new()),
        }
    }

    /// Test one candidate, recording it as a hit if it resolves an unknown hash.
    pub fn check(&self, candidate: &str) {
        self.tried.fetch_add(1, Ordering::Relaxed);

        let hash = self.hash_kind.hash(candidate, self.key_width);
        if !self.unknown.contains(hash) {
            return;
        }

        self.found
            .lock()
            .unwrap()
            .push((hash, candidate.to_owned()));
    }

    /// Number of candidates checked so far.
    pub fn tried(&self) -> u64 {
        self.tried.load(Ordering::Relaxed)
    }

    /// Consume the sink: `(candidates_tried, hits)`, deduped by hash
    /// (guessers can generate the same winning candidate many times).
    pub(crate) fn drain(self) -> (u64, Vec<(u64, String)>) {
        let mut found = self.found.into_inner().unwrap();
        let mut seen = HashSet::new();
        found.retain(|(hash, _)| seen.insert(*hash));

        (self.tried.into_inner(), found)
    }
}
