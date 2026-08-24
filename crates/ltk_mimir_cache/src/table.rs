//! The logical hash tables, and the hash universe each one publishes into.

use std::fmt;
use std::str::FromStr;

use ltk_hashdb::{Casing, HashKind, KeyConfig, KeyWidth};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ParseTableError;

/// The logical hash tables, each stored as its own `.lhdb` file.
///
/// The two RST variants hash the same strings with different algorithms (XXH64
/// vs XXH3 for RST v5+), so they are separate tables (see `docs/FORMAT.md`).
///
/// The set grows as CommunityDragon publishes new lists, so this is
/// `#[non_exhaustive]`: match with a `_` arm, and iterate [`ALL`](Table::ALL)
/// rather than writing the variants out. It serializes as its [`id`](Table::id),
/// which is also what [`FromStr`] accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Table {
    Game,
    Lcu,
    BinEntries,
    BinTypes,
    BinFields,
    BinHashes,
    Rst,
    RstXxh3,
}

impl Table {
    /// Every logical table, in a stable order.
    pub const ALL: &'static [Table] = &[
        Table::Game,
        Table::Lcu,
        Table::BinEntries,
        Table::BinTypes,
        Table::BinFields,
        Table::BinHashes,
        Table::Rst,
        Table::RstXxh3,
    ];

    /// The stable string id used in filenames and manifest keys.
    pub const fn id(self) -> &'static str {
        match self {
            Table::Game => "game",
            Table::Lcu => "lcu",
            Table::BinEntries => "binentries",
            Table::BinTypes => "bintypes",
            Table::BinFields => "binfields",
            Table::BinHashes => "binhashes",
            Table::Rst => "rst",
            Table::RstXxh3 => "rst-xxh3",
        }
    }

    /// Parse a table from its [`id`](Table::id).
    pub fn from_id(id: &str) -> Option<Table> {
        Table::ALL.iter().copied().find(|t| t.id() == id)
    }

    /// Which universe of hashed strings this table's keys are drawn from.
    pub fn universe(self) -> HashUniverse {
        match self {
            Table::Game | Table::Lcu => HashUniverse::WadPath,
            Table::BinEntries => HashUniverse::BinEntry,
            Table::BinTypes => HashUniverse::BinType,
            Table::BinFields => HashUniverse::BinField,
            Table::BinHashes => HashUniverse::BinHash,
            Table::Rst => HashUniverse::RstXxh64,
            Table::RstXxh3 => HashUniverse::RstXxh3,
        }
    }

    /// How this table's keys were produced, from its
    /// [`universe`](Table::universe).
    pub fn key_config(self) -> KeyConfig {
        self.universe().key_config()
    }

    /// Key width: 8 bytes for the WAD path and RST tables, 4 for the bin tables.
    pub fn key_width(self) -> KeyWidth {
        self.key_config().key_width()
    }

    /// The algorithm this table's keys were hashed with.
    pub fn hash_kind(self) -> HashKind {
        self.key_config().hash_kind()
    }

    /// The casing rule: every League table hashes the ASCII-lowercased string.
    pub fn casing(self) -> Casing {
        self.key_config().casing()
    }
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// The universe of strings a set of hashes was drawn from.
///
/// Grows with [`Table`], so it is `#[non_exhaustive]` for the same reason.
///
/// A hash only means something inside its universe: `binentries` and `binfields`
/// are both 32-bit FNV-1a over an ASCII-lowercased string, so a property hash
/// looked up in the entry table can collide with an unrelated object path and
/// come back with a confident, wrong answer. Tables may therefore only be layered
/// together (see [`HashStore::open_layered`](crate::HashStore::open_layered)) when
/// they share a universe - `game` and `lcu`, which are two halves of one WAD path
/// space, and nothing else.
///
/// A shared [`KeyConfig`] is the weaker, mechanical half of that: it is what
/// `ltk_hashdb` can check without knowing what the strings mean. Universes are
/// where the meaning lives, so each one states its key configuration and the
/// tables read theirs off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum HashUniverse {
    /// WAD chunk paths (`game`, `lcu`).
    WadPath,

    /// `.bin` entry (object) paths.
    BinEntry,

    /// `.bin` class and struct names.
    BinType,

    /// `.bin` property (field) names.
    BinField,

    /// Other strings that appear hashed inside `.bin` values.
    BinHash,

    /// RST stringtable keys, XXH64.
    RstXxh64,

    /// RST stringtable keys, XXH3 (RST v5+).
    RstXxh3,
}

impl HashUniverse {
    /// How every table in this universe hashes its strings.
    ///
    /// This is the workspace's single statement of each table's width, algorithm,
    /// and casing; [`Table::key_config`] and the CLI both read it from here.
    pub fn key_config(self) -> KeyConfig {
        // Every League list is hashed from the lowercased string.
        let casing = Casing::AsciiInsensitive;
        match self {
            Self::WadPath | Self::RstXxh64 => {
                KeyConfig::new(KeyWidth::U64, HashKind::Xxh64, casing)
            }
            Self::RstXxh3 => KeyConfig::new(KeyWidth::U64, HashKind::Xxh3, casing),
            Self::BinEntry | Self::BinType | Self::BinField | Self::BinHash => {
                KeyConfig::new(KeyWidth::U32, HashKind::Fnv1a32, casing)
            }
        }
    }

    /// The stable string id, as it appears in diagnostics.
    pub fn id(self) -> &'static str {
        match self {
            Self::WadPath => "wad-path",
            Self::BinEntry => "bin-entry",
            Self::BinType => "bin-type",
            Self::BinField => "bin-field",
            Self::BinHash => "bin-hash",
            Self::RstXxh64 => "rst-xxh64",
            Self::RstXxh3 => "rst-xxh3",
        }
    }
}

impl fmt::Display for HashUniverse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl FromStr for Table {
    type Err = ParseTableError;

    /// Accepts exactly the [`id`](Table::id) spellings - `binentries`, not
    /// `BinEntries` or `bin-entries`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Table::from_id(s).ok_or_else(|| ParseTableError::new(s))
    }
}

impl Serialize for Table {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.id())
    }
}

impl<'de> Deserialize<'de> for Table {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        Table::from_id(&id).ok_or_else(|| D::Error::custom(ParseTableError::new(&id)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for &table in Table::ALL {
            assert_eq!(Table::from_id(table.id()), Some(table));
            assert_eq!(table.to_string(), table.id());
            assert_eq!(table.id().parse(), Ok(table));
        }
        assert_eq!(Table::from_id("nope"), None);
    }

    /// Serde round-trips through the id, not the variant name.
    #[test]
    fn serde_uses_the_id() {
        for &table in Table::ALL {
            let json = serde_json::to_string(&table).unwrap();
            assert_eq!(json, format!("{:?}", table.id()));
            assert_eq!(serde_json::from_str::<Table>(&json).unwrap(), table);
        }

        let err = serde_json::from_str::<Table>("\"BinEntries\"").unwrap_err();
        assert!(err.to_string().contains("binentries"), "{err}");
    }

    #[test]
    fn parse_error_lists_the_accepted_ids() {
        let err = "bin-entries".parse::<Table>().unwrap_err();
        assert_eq!(err.input(), "bin-entries");
        let msg = err.to_string();
        assert!(
            msg.contains("binentries") && msg.contains("rst-xxh3"),
            "{msg}"
        );
    }

    /// The facts the CLI and the bundler used to each carry a copy of.
    #[test]
    fn key_configs_match_the_published_tables() {
        use Table::*;
        for table in [Game, Lcu, Rst] {
            assert_eq!(table.key_width(), KeyWidth::U64, "{table}");
            assert_eq!(table.hash_kind(), HashKind::Xxh64, "{table}");
        }
        assert_eq!(RstXxh3.key_width(), KeyWidth::U64);
        assert_eq!(RstXxh3.hash_kind(), HashKind::Xxh3);
        for table in [BinEntries, BinTypes, BinFields, BinHashes] {
            assert_eq!(table.key_width(), KeyWidth::U32, "{table}");
            assert_eq!(table.hash_kind(), HashKind::Fnv1a32, "{table}");
        }
        for &table in Table::ALL {
            assert_eq!(table.casing(), Casing::AsciiInsensitive, "{table}");
        }
    }

    /// The point of having a universe at all: it is strictly finer than the key
    /// config, so a shared config is not licence to layer two tables.
    #[test]
    fn universes_are_finer_than_key_configs() {
        use Table::*;
        assert_eq!(Game.universe(), Lcu.universe(), "one WAD path space");

        for a in [BinEntries, BinTypes, BinFields, BinHashes] {
            for b in [BinEntries, BinTypes, BinFields, BinHashes] {
                assert_eq!(a.key_config(), b.key_config(), "{a} vs {b}");
                assert_eq!(a == b, a.universe() == b.universe(), "{a} vs {b}");
            }
        }
    }

    #[test]
    fn every_table_hashes_under_its_own_config() {
        // The one path all eight tables can agree on the spelling of.
        let path = "data/characters/aatrox/aatrox.bin";
        for &table in Table::ALL {
            let config = table.key_config();
            assert_eq!(
                config.hash(path),
                config
                    .hash_kind()
                    .hash(path, config.casing(), config.key_width()),
                "{table}"
            );
        }
    }
}
