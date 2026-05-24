//! Watcher daemon public API surface.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Watcher result type.
pub type Result<T> = std::result::Result<T, WatcherError>;

/// Errors produced by watcher APIs.
#[derive(Debug, Error)]
pub enum WatcherError {
    /// The requested behavior has not been wired into this build.
    #[error("{feature} is not available in the scaffold build")]
    FeatureUnavailable { feature: &'static str },
}

/// Watcher startup options.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WatchOptions {
    /// Keep the watcher alive until explicitly stopped.
    pub persistent: bool,
}

/// Handle returned after starting a watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct WatcherHandle {
    /// Repository root watched by this handle.
    pub repo_root: PathBuf,
}

/// Watcher status for a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct WatcherStatus {
    /// Whether a watcher process is currently known to be running.
    pub running: bool,
}

/// Starts a watcher for the repository.
pub fn start(repo_root: &Path, opts: WatchOptions) -> Result<WatcherHandle> {
    let _ = (repo_root, opts);
    unavailable("watcher start")
}

/// Stops a watcher for the repository.
pub fn stop(repo_root: &Path) -> Result<()> {
    let _ = repo_root;
    unavailable("watcher stop")
}

/// Reports watcher status for the repository.
pub fn status(repo_root: &Path) -> Result<WatcherStatus> {
    let _ = repo_root;
    unavailable("watcher status")
}

fn unavailable<T>(feature: &'static str) -> Result<T> {
    Err(WatcherError::FeatureUnavailable { feature })
}
