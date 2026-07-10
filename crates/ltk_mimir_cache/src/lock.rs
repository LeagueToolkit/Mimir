//! Cross-process single-updater lock: std's advisory whole-file lock on
//! `.update.lock`, acquired non-blocking so a second updater backs off instead
//! of piling on. Readers never take it - the read path is lock-free over
//! immutable files.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

/// A held exclusive lock on the cache's `.update.lock`; dropping it releases the
/// lock. The lock file stays behind - its presence means nothing, only the OS
/// lock does.
#[derive(Debug)]
pub struct UpdateLock {
    _file: File,
}

impl UpdateLock {
    /// Try to acquire the lock without blocking. `Ok(None)` means another
    /// process holds it.
    pub(crate) fn try_acquire(path: &Path) -> std::io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}
