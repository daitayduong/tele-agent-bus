//! Phase 4a — Per-auth-context file lock (spec §5.2).
//!
//! Serializes CLI invocations against the same `profile_dir`. One
//! `<profile_dir>/.agent-bus.lock` file per context; exclusive flock via
//! `fs2`. RAII drop releases the lock.
//!
//! Async wrapper polls at 200ms intervals up to `timeout`. Lock acquisition
//! uses `spawn_blocking` because `flock(2)` is a blocking syscall.

use std::path::{Path, PathBuf};
use std::time::Duration;

use fs2::FileExt;
use thiserror::Error;

const POLL_INTERVAL: Duration = Duration::from_millis(200);
const LOCK_FILE_NAME: &str = ".agent-bus.lock";

/// RAII handle to an exclusive flock. Drop releases the lock.
///
/// `_file` is kept alive so the flock persists; file descriptor close
/// releases the advisory lock at the kernel level.
pub struct ContextLock {
    _file: std::fs::File,
    path: PathBuf,
}

impl ContextLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for ContextLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextLock").field("path", &self.path).finish()
    }
}

#[derive(Debug, Error)]
pub enum LockError {
    #[error("timeout acquiring lock at {path}")]
    Timeout { path: PathBuf },
    #[error("profile_dir does not exist: {path}")]
    MissingDir { path: PathBuf },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Acquire an exclusive flock on `<profile_dir>/.agent-bus.lock`.
///
/// Creates the lock file if missing. Polls every 200ms. Returns
/// `LockError::Timeout` if `timeout` elapses.
///
/// **Precondition**: `profile_dir` must exist. A missing parent returns
/// `LockError::MissingDir` immediately (no polling).
pub async fn acquire(profile_dir: &Path, timeout: Duration) -> Result<ContextLock, LockError> {
    if !profile_dir.exists() {
        return Err(LockError::MissingDir {
            path: profile_dir.to_path_buf(),
        });
    }
    let path = profile_dir.join(LOCK_FILE_NAME);
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let path_cloned = path.clone();
        let attempt = tokio::task::spawn_blocking(move || try_acquire(&path_cloned))
            .await
            .map_err(|e| LockError::Io(std::io::Error::other(e)))?;

        match attempt {
            Ok(Some(file)) => {
                return Ok(ContextLock {
                    _file: file,
                    path,
                })
            }
            Ok(None) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(LockError::Timeout { path });
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            Err(e) => return Err(LockError::Io(e)),
        }
    }
}

/// Blocking single attempt. Returns `Ok(Some)` on acquired, `Ok(None)` on
/// contention, `Err` on hard io failure.
fn try_acquire(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(e) if is_would_block(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

fn is_would_block(e: &std::io::Error) -> bool {
    // fs2 surfaces contention either as WouldBlock or raw EAGAIN/EWOULDBLOCK.
    if e.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    matches!(e.raw_os_error(), Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn acquire_on_empty_dir_creates_lockfile() {
        let dir = TempDir::new().unwrap();
        let lock = acquire(dir.path(), Duration::from_millis(100)).await.unwrap();
        assert!(lock.path().exists());
        assert_eq!(lock.path().file_name().unwrap(), LOCK_FILE_NAME);
    }

    #[tokio::test]
    async fn contended_lock_times_out() {
        let dir = TempDir::new().unwrap();
        let _held = acquire(dir.path(), Duration::from_millis(100)).await.unwrap();
        // Second acquire within a short timeout must fail with Timeout.
        let start = std::time::Instant::now();
        let err = acquire(dir.path(), Duration::from_millis(400))
            .await
            .unwrap_err();
        let elapsed = start.elapsed();
        assert!(matches!(err, LockError::Timeout { .. }));
        // Sanity: waited at least close to the timeout (allow some slack).
        assert!(elapsed >= Duration::from_millis(300));
    }

    #[tokio::test]
    async fn release_on_drop_allows_reacquire() {
        let dir = TempDir::new().unwrap();
        {
            let _lock = acquire(dir.path(), Duration::from_millis(100)).await.unwrap();
        }
        // Now immediately re-acquire.
        let lock2 = acquire(dir.path(), Duration::from_millis(100)).await.unwrap();
        assert!(lock2.path().exists());
    }

    #[tokio::test]
    async fn missing_profile_dir_returns_error() {
        let missing = Path::new("/tmp/does-not-exist-abcdef-123456");
        let err = acquire(missing, Duration::from_millis(100)).await.unwrap_err();
        assert!(matches!(err, LockError::MissingDir { .. }));
    }

    #[tokio::test]
    async fn release_unblocks_waiter() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().to_path_buf();

        let held = acquire(&path, Duration::from_millis(100)).await.unwrap();

        // Waiter in separate task, with a timeout longer than the hold window.
        let path_clone = path.clone();
        let waiter = tokio::spawn(async move {
            acquire(&path_clone, Duration::from_secs(2)).await
        });

        // Hold briefly then drop.
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(held);

        let result = waiter.await.unwrap();
        assert!(result.is_ok(), "waiter should acquire after release");
    }
}
