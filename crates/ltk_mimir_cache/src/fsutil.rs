//! Small filesystem helpers shared by the manifest and store: atomic replace,
//! sibling temp paths, and streaming sha256.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// A sibling temp path (`<name>.tmp`) in `path`'s directory, so a subsequent
/// rename is an in-volume move.
pub fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

/// Write `bytes` to `path` atomically: sibling temp file, `fsync`, rename over
/// the destination. `fs::rename` replaces an existing file on both POSIX and
/// Windows, so readers see the old or new file whole, never a partial write.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = tmp_sibling(path);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

/// Copy `src` into `dst` atomically (temp copy + fsync + rename), so a partial
/// copy is never visible under its final versioned name.
pub fn atomic_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let tmp = tmp_sibling(dst);
    fs::copy(src, &tmp)?;
    // Re-open for writing to fsync: on Windows `FlushFileBuffers` needs write access,
    // so a read-only handle would fail with `ERROR_ACCESS_DENIED`.
    fs::OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
    fs::rename(&tmp, dst)
}

/// sha256 of an in-memory buffer, returned as lowercase hex.
pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Streaming sha256 of a file, returned as lowercase hex.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
