//! The `manifest.json` pointer file: the active `.lhdb` version per table plus
//! its sha256 and a little provenance. The manifest is the only mutable file in the
//! cache; it is swapped atomically so a reader never sees a half-written pointer.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{fsutil, ManifestError, Table};

/// The manifest schema version this build reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

/// The first schema ever published. Anything below it is not an old manifest,
/// it is not a manifest.
const OLDEST_SCHEMA: u32 = 1;

/// The `manifest.json` document: schema version, generation timestamp, optional input
/// provenance, and one [`TableEntry`] per published table keyed by [`Table::id`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The schema the writer used. Informational - see
    /// [`from_slice`](Manifest::from_slice) for what is actually gated.
    pub schema: u32,

    /// The lowest schema version that can read this document correctly, set
    /// only when that is not every version.
    ///
    /// The schema is meant to grow by adding fields, which costs older readers
    /// nothing. This is the escape hatch for the day something genuinely
    /// incompatible is needed: a writer that sets it takes older builds offline
    /// deliberately, rather than as a side effect of bumping a number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_reader_schema: Option<u32>,

    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    #[serde(default)]
    pub tables: BTreeMap<String, TableEntry>,
}

/// Provenance of the inputs a manifest was built from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// Where the txt hash lists came from: a git URL or a GitHub `owner/repo`
    /// (canonically `CommunityDragon/Data`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,

    /// The commit of that repo the inputs were taken at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,

    /// One sha256 over all input files, in sorted-filename order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs_sha256: Option<String>,
}

/// The active file for one table plus the metadata a reader/updater needs without
/// opening it: download checksum, entry count, and key width.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableEntry {
    pub file: String,
    pub sha256: String,
    pub entries: u64,
    pub key_width: u8,

    /// The `.hashdb` format version `file` is written in.
    ///
    /// Per table, not per manifest, so one release can carry a table in a new
    /// format without stranding a build on the tables it can still read.
    /// Manifests written before this field existed described format 1 only,
    /// which is what its absence means.
    #[serde(default = "default_format_version")]
    pub format_version: u16,
}

impl TableEntry {
    /// Whether this build can open `file` at all.
    ///
    /// The format version is an equality gate in the header, so this asks the
    /// same question [`HashDb::open`](ltk_hashdb::HashDb::open) would - just
    /// without a download and an mmap first.
    pub fn is_supported(&self) -> bool {
        self.format_version == ltk_hashdb::FORMAT_VERSION
    }
}

/// What a `format_version`-less entry meant: the only format that existed then.
fn default_format_version() -> u16 {
    1
}

impl Manifest {
    /// An empty manifest stamped with the current time.
    pub fn empty() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            min_reader_schema: None,
            generated_at: now_rfc3339(),
            source: None,
            tables: BTreeMap::new(),
        }
    }

    /// The release-asset filename carrying the manifest for one `.hashdb`
    /// format version: `manifest-v1.json`.
    ///
    /// A release publishes one of these per format it still builds, next to the
    /// unversioned `manifest.json`. That is what lets a build outlive a format
    /// bump it cannot follow - it asks for its own channel and keeps getting
    /// tables it can open, instead of a manifest it has to skip its way through.
    pub fn asset_for_format(format_version: u16) -> String {
        format!("manifest-v{format_version}.json")
    }

    /// The active entry for `table`, if present.
    pub fn entry(&self, table: Table) -> Option<&TableEntry> {
        self.tables.get(table.id())
    }

    /// Parse a manifest, refusing only what this build genuinely cannot use.
    ///
    /// [`schema`](Manifest::schema) is not an equality gate. The document grows
    /// by adding fields and serde drops the ones this build has never heard of,
    /// so a manifest written by a newer tool still parses; a writer that breaks
    /// that promise has to say so through
    /// [`min_reader_schema`](Manifest::min_reader_schema).
    ///
    /// That distinction is the whole reason a shared cache works. Several
    /// independently-versioned tools point at one directory, and the first to
    /// sync writes the manifest all the others then read - so an equality gate
    /// here takes every not-yet-upgraded tool on the machine offline, including
    /// the ones that only wanted to report cache status.
    ///
    /// # Errors
    ///
    /// [`ManifestError::Json`] for a malformed document,
    /// [`ManifestError::UnsupportedSchema`] for a `schema` below the first one
    /// ever published, and [`ManifestError::ReaderTooOld`] when the manifest
    /// asks for a newer reader than this.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest: Manifest = serde_json::from_slice(bytes)?;
        if manifest.schema < OLDEST_SCHEMA {
            return Err(ManifestError::UnsupportedSchema(manifest.schema));
        }
        if let Some(required) = manifest.min_reader_schema {
            if required > SCHEMA_VERSION {
                return Err(ManifestError::ReaderTooOld {
                    required,
                    supported: SCHEMA_VERSION,
                });
            }
        }

        Ok(manifest)
    }

    /// Read and parse the manifest at `path`.
    pub fn read(path: &Path) -> Result<Self, ManifestError> {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_slice(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ManifestError::Missing(path.to_path_buf()))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Serialize (pretty, trailing newline) and atomically swap the manifest at `path`.
    pub fn write_atomic(&self, path: &Path) -> Result<(), ManifestError> {
        let mut json = serde_json::to_vec_pretty(self)?;
        json.push(b'\n');
        fsutil::atomic_write(path, &json)?;
        Ok(())
    }
}

/// The current time as an RFC-3339 UTC string, e.g. `2026-07-08T12:34:56Z`.
pub(crate) fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc3339(secs)
}

/// Format a UNIX timestamp (seconds) as an RFC-3339 UTC string; an out-of-range
/// value degrades to an empty string rather than panicking.
fn format_rfc3339(secs: u64) -> String {
    OffsetDateTime::from_unix_timestamp(secs as i64)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest as a newer tool might write it: higher schema, a field this
    /// build has never heard of, and an entry carrying extra keys.
    const FROM_THE_FUTURE: &str = r#"{
        "schema": 99,
        "generated_at": "2026-07-10T00:00:00Z",
        "mirrors": ["https://example.invalid"],
        "tables": {
            "game": {
                "file": "game-2026-07-10.lhdb",
                "sha256": "abc",
                "entries": 3,
                "key_width": 8,
                "compression": "zeekstd"
            }
        }
    }"#;

    #[test]
    fn a_newer_manifest_still_parses() {
        let manifest = Manifest::from_slice(FROM_THE_FUTURE.as_bytes()).expect("parses");
        assert_eq!(manifest.schema, 99);
        assert_eq!(manifest.entry(Table::Game).unwrap().entries, 3);
    }

    /// Absent means format 1 - the only format that existed before the field.
    #[test]
    fn format_version_defaults_to_the_first_one() {
        let manifest = Manifest::from_slice(FROM_THE_FUTURE.as_bytes()).expect("parses");
        let entry = manifest.entry(Table::Game).unwrap();
        assert_eq!(entry.format_version, 1);
        assert!(entry.is_supported());
    }

    /// The escape hatch: a writer that breaks additivity has to say so, and
    /// only then does an older build refuse the document.
    #[test]
    fn a_manifest_demanding_a_newer_reader_is_refused() {
        let json = format!(
            r#"{{"schema": 1, "min_reader_schema": {}, "generated_at": "", "tables": {{}}}}"#,
            SCHEMA_VERSION + 1
        );
        let err = Manifest::from_slice(json.as_bytes()).unwrap_err();
        assert!(
            matches!(err, ManifestError::ReaderTooOld { required, supported }
                if required == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION),
            "{err}"
        );

        let json = r#"{"schema": 1, "min_reader_schema": 1, "generated_at": "", "tables": {}}"#;
        assert!(
            Manifest::from_slice(json.as_bytes()).is_ok(),
            "1 is readable"
        );
    }

    #[test]
    fn a_schema_below_the_first_published_one_is_refused() {
        let json = r#"{"schema": 0, "generated_at": "", "tables": {}}"#;
        let err = Manifest::from_slice(json.as_bytes()).unwrap_err();
        assert!(matches!(err, ManifestError::UnsupportedSchema(0)), "{err}");
    }

    /// The channel a build asks for is named after the format it can read.
    #[test]
    fn the_channel_asset_names_the_format() {
        assert_eq!(Manifest::asset_for_format(1), "manifest-v1.json");
        assert_eq!(
            Manifest::asset_for_format(ltk_hashdb::FORMAT_VERSION),
            "manifest-v1.json"
        );
    }

    /// `min_reader_schema` stays out of the document until it is set, so today's
    /// manifests are byte-identical to yesterday's.
    #[test]
    fn an_unset_min_reader_schema_is_not_written() {
        let json = serde_json::to_string(&Manifest::empty()).unwrap();
        assert!(!json.contains("min_reader_schema"), "{json}");
    }

    #[test]
    fn rfc3339_matches_known_epochs() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        // 2026-07-08T00:00:00Z
        assert_eq!(format_rfc3339(1_783_468_800), "2026-07-08T00:00:00Z");
        // A leap day: 2024-02-29T13:45:30Z
        assert_eq!(format_rfc3339(1_709_214_330), "2024-02-29T13:45:30Z");
    }
}
