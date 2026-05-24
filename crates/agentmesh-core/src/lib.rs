//! Core domain APIs and persisted state shapes for AgentMesh.

use std::path::{Path, PathBuf};

use thiserror::Error;

pub mod config;
pub mod drainer;
pub mod identity;
pub mod lockfile;
pub mod merge;
pub mod mutex;
pub mod pending_queue;
pub mod pipeline;
pub mod state;
pub mod types;

pub use agentmesh_protocol::EntityType;
pub use pipeline::AdapterRegistry;
pub use types::{EntityId, Hash, LocationKey, RuntimeName};

/// Current crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Core result type.
pub type Result<T> = std::result::Result<T, CoreError>;

/// Errors produced by core orchestration APIs.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Core sync pipeline failed.
    #[error(transparent)]
    Pipeline(#[from] pipeline::PipelineError),
}

/// Preferred root instructions source when initialization finds more than one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalInstructions {
    /// Use root `AGENTS.md` as the canonical instructions file.
    AgentsMd,
    /// Use root `CLAUDE.md` as the canonical instructions file.
    ClaudeMd,
}

/// Options for repository initialization.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InitOptions {
    /// Preferred root instructions source for non-interactive conflict resolution.
    pub canonical_instructions: Option<CanonicalInstructions>,
    /// When true, report planned initialization without writing state.
    pub dry_run: bool,
    /// When true, do not install or update runtime hooks.
    pub skip_hooks: bool,
}

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
    /// When true, process the pending queue after a full sync.
    pub await_drain: bool,
    /// Trigger label stored on pending records created by this pass.
    pub trigger: Option<String>,
    /// When true, callers intend the drain path to run in the background.
    pub background: bool,
    /// When true, only drain pending records and skip the scanner pass.
    pub drain_pending: bool,
    /// When true, suppress optional progress-producing behavior.
    pub silent: bool,
}

/// Summary returned by synchronization.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct SyncSummary {
    /// Whether synchronization detected or produced state movement.
    pub changed: bool,
    /// Number of entities with repository-visible file changes.
    pub entities_changed: usize,
    /// Number of pending records enqueued.
    pub pending_enqueued: usize,
    /// Number of pending records drained.
    pub pending_drained: usize,
    /// Number of entities still requiring conflict acknowledgement.
    pub pending_conflicts: usize,
    /// Number of entities skipped because a runtime lacks support for them.
    pub capability_skipped: usize,
}

/// Health report for a repository.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct DoctorReport {
    /// Machine-readable health findings.
    pub findings: Vec<String>,
    /// Typed health counters for callers that need stable decisions.
    pub health: DoctorHealth,
}

/// Stable health counters returned by doctor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct DoctorHealth {
    /// Number of entities whose current files differ from the lockfile.
    pub entities_out_of_sync: usize,
    /// Number of entities still requiring conflict acknowledgement.
    pub pending_conflicts: usize,
    /// Number of pending sync records waiting to drain.
    pub pending_syncs: usize,
    /// Number of pending sync records that exhausted retries.
    pub failed_pending_syncs: usize,
    /// Number of entities skipped because no runtime can represent them.
    pub capability_skips: usize,
}

/// Options for restoring a preserved entity version.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestoreOptions {
    /// Specific preserved timestamp to restore. When absent, the latest version is used.
    pub at: Option<String>,
    /// When true, validate and report the restore plan without writing files.
    pub dry_run: bool,
}

/// Summary returned by restore operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct RestoreSummary {
    /// Whether repository-visible files changed.
    pub changed: bool,
    /// Preserved file selected for the restore.
    pub preserved_version: PathBuf,
    /// Number of files written.
    pub files_written: usize,
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
    pipeline::init(repo_root, opts).map_err(Into::into)
}

/// Initializes AgentMesh state with an explicit adapter registry.
pub fn init_with_adapter_registry(
    repo_root: &Path,
    opts: InitOptions,
    adapters: &dyn AdapterRegistry,
) -> Result<InitSummary> {
    pipeline::init_with_adapter_registry(repo_root, opts, adapters).map_err(Into::into)
}

/// Synchronizes AgentMesh state for a repository.
pub fn sync(repo_root: &Path, opts: SyncOptions) -> Result<SyncSummary> {
    pipeline::sync(repo_root, opts).map_err(Into::into)
}

/// Synchronizes AgentMesh state with an explicit adapter registry.
pub fn sync_with_adapter_registry(
    repo_root: &Path,
    opts: SyncOptions,
    adapters: &dyn AdapterRegistry,
) -> Result<SyncSummary> {
    pipeline::sync_with_adapter_registry(repo_root, opts, adapters).map_err(Into::into)
}

/// Checks AgentMesh repository health.
pub fn doctor(repo_root: &Path) -> Result<DoctorReport> {
    pipeline::doctor(repo_root).map_err(Into::into)
}

/// Checks AgentMesh repository health with an explicit adapter registry.
pub fn doctor_with_adapter_registry(
    repo_root: &Path,
    adapters: &dyn AdapterRegistry,
) -> Result<DoctorReport> {
    pipeline::doctor_with_adapter_registry(repo_root, adapters).map_err(Into::into)
}

/// Restores an entity from a preserved runtime version.
pub fn restore(repo_root: &Path, entity_id: &EntityId, from: RuntimeName) -> Result<()> {
    pipeline::restore(repo_root, entity_id, from).map_err(Into::into)
}

/// Restores an entity from a preserved runtime version with explicit options.
pub fn restore_with_options(
    repo_root: &Path,
    entity_id: &EntityId,
    from: RuntimeName,
    opts: RestoreOptions,
) -> Result<RestoreSummary> {
    pipeline::restore_with_options(repo_root, entity_id, from, opts).map_err(Into::into)
}

/// Restores an entity with an explicit adapter registry.
pub fn restore_with_options_and_adapter_registry(
    repo_root: &Path,
    entity_id: &EntityId,
    from: RuntimeName,
    opts: RestoreOptions,
    adapters: &dyn AdapterRegistry,
) -> Result<RestoreSummary> {
    pipeline::restore_with_options_and_adapter_registry(repo_root, entity_id, from, opts, adapters)
        .map_err(Into::into)
}

/// Acknowledges an entity's current conflict-resolution state.
pub fn ack(repo_root: &Path, entity_id: &EntityId) -> Result<()> {
    pipeline::ack(repo_root, entity_id).map_err(Into::into)
}

/// Updates the local binary integrity pin.
pub fn upgrade(repo_root: &Path) -> Result<UpgradeSummary> {
    pipeline::upgrade(repo_root).map_err(Into::into)
}

/// Removes AgentMesh local wiring from a repository.
pub fn uninstall(repo_root: &Path, opts: UninstallOptions) -> Result<UninstallSummary> {
    pipeline::uninstall(repo_root, opts).map_err(Into::into)
}

/// Rebuilds a clean lockfile from repository state.
pub fn reconcile_lock(repo_root: &Path) -> Result<ReconcileSummary> {
    pipeline::reconcile_lock(repo_root).map_err(Into::into)
}

/// Rebuilds a clean lockfile from repository state with an explicit adapter registry.
pub fn reconcile_lock_with_adapter_registry(
    repo_root: &Path,
    adapters: &dyn AdapterRegistry,
) -> Result<ReconcileSummary> {
    pipeline::reconcile_lock_with_adapter_registry(repo_root, adapters).map_err(Into::into)
}
