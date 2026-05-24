//! Core domain APIs and persisted state shapes for AgentMesh.

use std::path::Path;

use thiserror::Error;

pub mod config;
pub mod lockfile;
pub mod state;
pub mod types;

pub use agentmesh_protocol::EntityType;
pub use types::{EntityId, Hash, LocationKey, RuntimeName};

/// Current crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Core result type.
pub type Result<T> = std::result::Result<T, CoreError>;

/// Errors produced by core orchestration APIs.
#[derive(Debug, Error)]
pub enum CoreError {
    /// The requested behavior has not been wired into this build.
    #[error("{feature} is not available in the scaffold build")]
    FeatureUnavailable { feature: &'static str },
}

/// Options for repository initialization.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitOptions;

/// Summary returned by repository initialization.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct InitSummary {
    /// Whether repository-visible state changed.
    pub changed: bool,
}

/// Options for synchronization.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncOptions {
    /// When true, report drift without writing repository state.
    pub check: bool,
}

/// Summary returned by synchronization.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct SyncSummary {
    /// Whether synchronization detected or produced state movement.
    pub changed: bool,
}

/// Health report for a repository.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct DoctorReport {
    /// Machine-readable health findings.
    pub findings: Vec<String>,
}

/// Summary returned by binary trust updates.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct UpgradeSummary {
    /// Whether local trust state changed.
    pub changed: bool,
}

/// Options for removing local AgentMesh wiring.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UninstallOptions {
    /// When true, remove repository-visible generated state as well as machine-local wiring.
    pub prune_repository_state: bool,
}

/// Summary returned by uninstall operations.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct UninstallSummary {
    /// Files or local records removed by the operation.
    pub removed_entries: Vec<String>,
}

/// Summary returned by lockfile reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct ReconcileSummary {
    /// Whether the lockfile was rewritten.
    pub changed: bool,
}

/// Initializes AgentMesh state for a repository.
pub fn init(repo_root: &Path, opts: InitOptions) -> Result<InitSummary> {
    let _ = (repo_root, opts);
    unavailable("init")
}

/// Synchronizes AgentMesh state for a repository.
pub fn sync(repo_root: &Path, opts: SyncOptions) -> Result<SyncSummary> {
    let _ = (repo_root, opts);
    unavailable("sync")
}

/// Checks AgentMesh repository health.
pub fn doctor(repo_root: &Path) -> Result<DoctorReport> {
    let _ = repo_root;
    unavailable("doctor")
}

/// Restores an entity from a preserved runtime version.
pub fn restore(repo_root: &Path, entity_id: &EntityId, from: RuntimeName) -> Result<()> {
    let _ = (repo_root, entity_id, from);
    unavailable("restore")
}

/// Acknowledges an entity's current conflict-resolution state.
pub fn ack(repo_root: &Path, entity_id: &EntityId) -> Result<()> {
    let _ = (repo_root, entity_id);
    unavailable("ack")
}

/// Updates the local binary integrity pin.
pub fn upgrade(repo_root: &Path) -> Result<UpgradeSummary> {
    let _ = repo_root;
    unavailable("upgrade")
}

/// Removes AgentMesh local wiring from a repository.
pub fn uninstall(repo_root: &Path, opts: UninstallOptions) -> Result<UninstallSummary> {
    let _ = (repo_root, opts);
    unavailable("uninstall")
}

/// Rebuilds a clean lockfile from repository state.
pub fn reconcile_lock(repo_root: &Path) -> Result<ReconcileSummary> {
    let _ = repo_root;
    unavailable("reconcile-lock")
}

fn unavailable<T>(feature: &'static str) -> Result<T> {
    Err(CoreError::FeatureUnavailable { feature })
}
