//! Cross-process single-updater lock: std's advisory whole-file lock on
//! `.update.lock`, acquired non-blocking so a second updater backs off instead
//! of piling on. Readers never take it - the read path is lock-free over
//! immutable files.
//!
//! The OS lock is the whole mechanism. Beside it, the holder writes its pid and
//! start time into `.update.holder`, so a second process can say "pid 8123,
//! since 14:02" instead of "someone is already syncing", which reads the same as
//! a crashed updater. Nothing trusts that file, and it is a *separate* file
//! rather than the lock's own body because Windows file locks are mandatory: a
//! reader cannot touch the bytes of a file another process has locked.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::manifest::{now_rfc3339, parse_rfc3339};

/// The lock file itself. Its contents are never read or written; only the OS
/// lock on it means anything.
const LOCK_FILE: &str = ".update.lock";

/// Who holds the lock, for diagnostics only.
const HOLDER_FILE: &str = ".update.holder";

/// First poll interval when waiting for the lock, doubling up to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_millis(10);

/// Longest gap between attempts - an update takes seconds, so a quarter of one
/// is short enough to feel instant and long enough to stay idle.
const MAX_BACKOFF: Duration = Duration::from_millis(250);

/// A held exclusive lock on the cache's `.update.lock`; dropping it releases the
/// lock. The lock file stays behind - its presence means nothing, only the OS
/// lock does.
#[derive(Debug)]
pub struct UpdateLock {
    _file: File,
}

/// Who is updating the cache, per the record beside the lock.
///
/// Reported by [`HashStore::lock_holder`](crate::HashStore::lock_holder) only
/// while the lock is actually held, so this is never a leftover from a run that
/// has since finished. It can still name a process that died without releasing
/// it - the OS drops the lock when it does, and this is what is left to explain
/// the mess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LockHolder {
    /// Process id of the updater, as that process saw itself.
    pub pid: u32,

    /// When it took the lock, RFC-3339 UTC.
    pub since: String,
}

impl LockHolder {
    /// When the lock was taken, parsed from [`since`](LockHolder::since).
    ///
    /// `None` if the stamp is unreadable; the lock is still held either way.
    pub fn since_time(&self) -> Option<SystemTime> {
        parse_rfc3339(&self.since)
    }
}

impl UpdateLock {
    /// Try to acquire the lock over `dir` without blocking. `Ok(None)` means
    /// another process holds it.
    pub(crate) fn try_acquire(dir: &Path) -> std::io::Result<Option<Self>> {
        let Some(file) = open_and_lock(&dir.join(LOCK_FILE))? else {
            return Ok(None);
        };

        // Stamped only once the lock is ours, so the record can never describe a
        // process that lost the race.
        let _ = stamp(&dir.join(HOLDER_FILE));

        Ok(Some(Self { _file: file }))
    }

    /// Try to acquire the lock, giving up after `timeout`.
    ///
    /// Polls rather than blocking: std's blocking `lock` has no deadline, and an
    /// updater that waits forever is worse than one that reports who it waited
    /// for. A zero timeout is exactly [`try_acquire`](UpdateLock::try_acquire).
    pub(crate) fn acquire_timeout(dir: &Path, timeout: Duration) -> std::io::Result<Option<Self>> {
        let deadline = Instant::now() + timeout;
        let mut backoff = MIN_BACKOFF;

        loop {
            if let Some(lock) = Self::try_acquire(dir)? {
                return Ok(Some(lock));
            }

            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }

            std::thread::sleep(backoff.min(left));
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Who holds the lock over `dir`, if anyone does.
    ///
    /// The lock is the authority: taking it and dropping it again is the only
    /// reliable test, since both files outlive every holder. Only when that
    /// fails is the record read.
    pub(crate) fn holder(dir: &Path) -> std::io::Result<Option<LockHolder>> {
        // Opened rather than created: asking who is updating must not be what
        // brings a cache directory into existence.
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join(LOCK_FILE))
        {
            Ok(file) => file,
            // No lock file, or no cache directory: nobody has ever updated here.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };

        match file.try_lock() {
            // Free: whatever the record says is a leftover from a finished run.
            Ok(()) => return Ok(None),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(e)) => return Err(e),
        }

        // Written in one go, but not atomically - a torn read is possible and
        // reads as "nobody named themselves", which is what an absent record
        // means too.
        let body = match fs::read(dir.join(HOLDER_FILE)) {
            Ok(body) => body,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };

        Ok(serde_json::from_slice(&body).ok())
    }
}

/// Open the lock file and try to take it, `Ok(None)` if someone else has it.
fn open_and_lock(path: &Path) -> std::io::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Record this process as the holder, replacing whatever a previous one left.
///
/// Best-effort by design: a failure here costs a nicer error message, never the
/// lock, so the caller ignores it.
fn stamp(path: &Path) -> std::io::Result<()> {
    let holder = LockHolder {
        pid: std::process::id(),
        since: now_rfc3339(),
    };

    fs::write(path, serde_json::to_vec(&holder).unwrap_or_default())
}
