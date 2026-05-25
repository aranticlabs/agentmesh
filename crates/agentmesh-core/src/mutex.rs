//! OS advisory locks used by core state transitions.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use thiserror::Error;

/// Mutex result type.
pub type Result<T> = std::result::Result<T, MutexError>;

/// Errors produced while acquiring advisory locks.
#[derive(Debug, Error)]
pub enum MutexError {
    /// A filesystem operation failed.
    #[error("failed to {action} lock at {}", path.display())]
    Io {
        /// Operation being performed.
        action: &'static str,
        /// Lock path.
        path: PathBuf,
        /// Source IO error.
        #[source]
        source: std::io::Error,
    },
}

/// Advisory lock file wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentmeshMutex {
    path: PathBuf,
}

impl AgentmeshMutex {
    /// Creates a lock wrapper for a specific lock path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the underlying lock path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Blocks until the lock can be acquired.
    pub fn acquire(&self) -> Result<MutexGuard> {
        let file = self.open_file()?;
        file.lock_exclusive().map_err(|source| MutexError::Io {
            action: "acquire",
            path: self.path.clone(),
            source,
        })?;
        Ok(MutexGuard {
            _file: file,
            path: self.path.clone(),
        })
    }

    /// Attempts to acquire the lock without blocking.
    pub fn try_acquire(&self) -> Result<Option<MutexGuard>> {
        let file = self.open_file()?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(MutexGuard {
                _file: file,
                path: self.path.clone(),
            })),
            Err(source) if is_lock_contention(&source) => Ok(None),
            Err(source) => Err(MutexError::Io {
                action: "try acquire",
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn open_file(&self) -> Result<File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| MutexError::Io {
                action: "create directory for",
                path: self.path.clone(),
                source,
            })?;
        }

        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .map_err(|source| MutexError::Io {
                action: "open",
                path: self.path.clone(),
                source,
            })
    }
}

fn is_lock_contention(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }

    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(32 | 33))
    }

    #[cfg(not(windows))]
    {
        false
    }
}

/// RAII guard that releases the advisory lock when dropped.
#[derive(Debug)]
pub struct MutexGuard {
    _file: File,
    path: PathBuf,
}

impl MutexGuard {
    /// Returns the lock file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::AgentmeshMutex;

    #[test]
    fn try_acquire_reports_contention() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let lock = AgentmeshMutex::new(temp.path().join("locks/state"));
        let guard = match lock.acquire() {
            Ok(guard) => guard,
            Err(error) => panic!("lock should acquire: {error}"),
        };

        let contended = match lock.try_acquire() {
            Ok(contended) => contended,
            Err(error) => panic!("try_acquire should report cleanly: {error}"),
        };
        assert!(contended.is_none());

        drop(guard);

        let reacquired = match lock.try_acquire() {
            Ok(reacquired) => reacquired,
            Err(error) => panic!("lock should reacquire: {error}"),
        };
        assert!(reacquired.is_some());
    }

    #[test]
    fn worker_lock_does_not_block_state_lock() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let worker = AgentmeshMutex::new(temp.path().join("locks/worker"));
        let state = AgentmeshMutex::new(temp.path().join("locks/state"));
        let worker_guard = match worker.acquire() {
            Ok(guard) => guard,
            Err(error) => panic!("worker lock should acquire: {error}"),
        };

        let state_guard = match state.try_acquire() {
            Ok(guard) => guard,
            Err(error) => panic!("state lock should be independent: {error}"),
        };

        drop(worker_guard);
        assert!(state_guard.is_some());
    }
}
