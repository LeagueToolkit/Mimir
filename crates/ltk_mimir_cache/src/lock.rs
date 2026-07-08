//! Cross-process single-updater lock.
//!
//! Only one process should download + build + publish at a time; readers never take
//! this lock (the read path is lock-free over immutable files). We use std's native
//! advisory whole-file lock on `.update.lock` (`File::try_lock`); acquisition is
//! non-blocking, so a second updater backs off (`Ok(None)`) instead of piling on.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

/// A held exclusive lock on the cache's `.update.lock`. The OS lock is tied to the open
/// file handle, so dropping this (closing the handle) releases it; the lock file itself
/// is left in place (its presence means nothing - only the OS lock does).
#[derive(Debug)]
pub struct UpdateLock {
    _file: File,
}

impl UpdateLock {
    /// Try to acquire the lock without blocking. `Ok(None)` means another process holds
    /// it (i.e. an update is already in progress).
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
