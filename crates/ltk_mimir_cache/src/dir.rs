//! Cache directory resolution.
//!
//! The platform data directory is the default; `MIMIR_DIR` overrides it:
//!
//! - Windows: `%LOCALAPPDATA%\LeagueToolkit\hashes\`
//! - Linux:   `$XDG_DATA_HOME/LeagueToolkit/hashes` (fallback `~/.local/share/...`)
//! - macOS:   `~/Library/Application Support/LeagueToolkit/hashes`

use std::path::PathBuf;

use crate::{Error, Result};

/// The vendor/organization folder under the platform data dir.
const ORG_DIR: &str = "LeagueToolkit";
/// The tables subfolder under the org dir.
const HASHES_DIR: &str = "hashes";

/// Resolve the shared cache directory without creating it.
///
/// `MIMIR_DIR`, when set (and non-empty), points directly at the tables directory and
/// overrides everything; otherwise we use the platform data directory.
pub fn resolve() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("MIMIR_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    platform_dir()
}

fn platform_dir() -> Result<PathBuf> {
    let base = directories::BaseDirs::new().ok_or(Error::NoCacheDir)?;
    // Windows keeps hash tables machine-local (they are large, derived, and
    // per-install), so we use LocalAppData rather than the roaming profile.
    #[cfg(windows)]
    let root = base.data_local_dir();
    #[cfg(not(windows))]
    let root = base.data_dir();
    Ok(root.join(ORG_DIR).join(HASHES_DIR))
}
