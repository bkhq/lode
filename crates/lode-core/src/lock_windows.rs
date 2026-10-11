//! Single-instance lock at `$LODE_DIR/lode.pid.lock` (Windows).
//!
//! An exclusive file lock on `lode.pid.lock` is held for the whole run; the
//! system releases it when the process ends, however it ends. `lode.pid` records
//! the holder's pid and is removed on a clean exit. The lock file itself is
//! never removed.

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// RAII handle for the held lock; removes `lode.pid` on drop.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    _lock: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn open_lock(dir: &Path) -> std::io::Result<File> {
    File::options()
        .read(true)
        .append(true)
        .create(true)
        .open(dir.join("lode.pid.lock"))
}

/// Acquire the single-instance lock under `dir`.
///
/// # Errors
/// Returns [`Error::Lock`] when another lode already holds it.
pub fn acquire(dir: &Path) -> Result<LockGuard> {
    fs::create_dir_all(dir)?;
    let path = dir.join("lode.pid");
    let lock = open_lock(dir).map_err(|e| Error::Lock(format!("open lode.pid.lock: {e}")))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(Error::Lock(format!(
                "another lode instance is already running (lock {})",
                path.display()
            )));
        }
        Err(TryLockError::Error(e)) => {
            return Err(Error::Lock(format!("lock lode.pid.lock: {e}")));
        }
    }
    fs::write(&path, format!("{}\n", std::process::id()))?;
    Ok(LockGuard { path, _lock: lock })
}

/// The pid of a live lode currently holding the lock under `dir`, if any.
#[must_use]
pub fn live_holder(dir: &Path) -> Option<u32> {
    if !dir.join("lode.pid.lock").exists() {
        return None;
    }
    let probe = open_lock(dir).ok()?;
    match probe.try_lock() {
        // Nobody holds it; dropping `probe` releases our lock again.
        Ok(()) | Err(TryLockError::Error(_)) => None,
        Err(TryLockError::WouldBlock) => {
            let pid = fs::read_to_string(dir.join("lode.pid")).ok()?;
            pid.trim().parse().ok()
        }
    }
}
