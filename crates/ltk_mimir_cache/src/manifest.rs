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

/// The `manifest.json` document: schema version, generation timestamp, optional input
/// provenance, and one [`TableEntry`] per published table keyed by [`Table::id`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
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
}

impl Manifest {
    /// An empty manifest stamped with the current time.
    pub fn empty() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            generated_at: now_rfc3339(),
            source: None,
            tables: BTreeMap::new(),
        }
    }

    /// The active entry for `table`, if present.
    pub fn entry(&self, table: Table) -> Option<&TableEntry> {
        self.tables.get(table.id())
    }

    /// Parse a manifest from bytes, rejecting an unknown schema version.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest: Manifest = serde_json::from_slice(bytes)?;
        if manifest.schema != SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema(manifest.schema));
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

    #[test]
    fn rfc3339_matches_known_epochs() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        // 2026-07-08T00:00:00Z
        assert_eq!(format_rfc3339(1_783_468_800), "2026-07-08T00:00:00Z");
        // A leap day: 2024-02-29T13:45:30Z
        assert_eq!(format_rfc3339(1_709_214_330), "2024-02-29T13:45:30Z");
    }
}
