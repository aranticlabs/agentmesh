//! Core repository scanner and sync orchestration.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use agentmesh_adapter_sdk_rust::{Adapter, AdapterError};
use agentmesh_protocol::{
    DetectResponse, EmitEntity, EmitRequest, EmitResponse, EntityFile, ImportRequest,
    ImportResponse, ImportedEntity, ProtocolError, RuntimeMode as ProtocolRuntimeMode,
};
use agentmesh_protocol::{
    InitializeRequest, JsonRpcRequest, JsonRpcResponse, OkResponse, PROTOCOL_VERSION,
    read_json_frame, write_json_frame,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;

use crate::config::{
    AgentmeshConfig, CapabilityFallback, ConfigError, RuntimeMode as ConfigRuntimeMode, load_config,
};
use crate::drainer::{DrainerError, DrainerProcessError, drain_pending};
use crate::identity::{
    IdentityError, RenameCandidate, derive_entity_id, detect_rename, parse_pin_marker,
    resolve_collision,
};
use crate::lockfile::{
    AdapterDeclaration, AdapterMode, HookKind, Lockfile, LockfileEntity, LockfileError,
    OverrideEntry, RenameRecord, entity_schema_for_type, read_lockfile, write_lockfile,
};
use crate::merge::{MergeError, MergeSide, MergeStatus, merge_markdown, preserve_losing_version};
use crate::mutex::{AgentmeshMutex, MutexError};
use crate::pending_queue::{PendingQueue, PendingQueueError};
use crate::state::{
    CacheLayout, IntegrityPin, PendingAction, PendingSyncRecord, StateError, conflict_entity_dir,
    conflict_version_file_name, read_hook_ownership, read_integrity_pin, read_json, sha256_bytes,
    sha256_file, write_atomic, write_integrity_pin,
};
use crate::types::{EntityId, Hash, LocationKey, RuntimeName, TypeError};
use crate::{
    CanonicalInstructions, CapabilitySkipFinding, DoctorHealth, DoctorReport, EntityType,
    InitOptions, InitSummary, ReconcileSummary, RestoreOptions, RestoreSummary, SyncOptions,
    SyncSummary, UninstallOptions, UninstallSummary, UpgradeSummary, VERSION,
};

mod doctor;
pub use doctor::{doctor, doctor_with_adapter_registry};

/// Pipeline result type.
pub type Result<T> = std::result::Result<T, PipelineError>;

const MAX_ENTITY_TREE_DEPTH: usize = 32;
const MAX_ENTITY_FILE_COUNT: usize = 1024;
const MAX_ENTITY_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Runtime adapter operations required by the sync pipeline.
pub trait AdapterRegistry {
    /// Detects whether a runtime is present in a repository.
    fn detect(&self, runtime: &RuntimeName, repo_root: &Path) -> Result<DetectResponse>;

    /// Imports runtime-native entities into canonical protocol payloads.
    fn import(
        &self,
        runtime: &RuntimeName,
        repo_root: &Path,
        request: ImportRequest,
    ) -> Result<ImportResponse>;

    /// Emits canonical protocol payloads into a runtime.
    fn emit(
        &self,
        runtime: &RuntimeName,
        repo_root: &Path,
        request: EmitRequest,
    ) -> Result<EmitResponse>;
}

#[derive(Debug, Clone, Copy, Default)]
struct SubprocessAdapterRegistry;

impl AdapterRegistry for SubprocessAdapterRegistry {
    fn detect(&self, runtime: &RuntimeName, repo_root: &Path) -> Result<DetectResponse> {
        call_adapter_subprocess(repo_root, runtime, "detect", ())
    }

    fn import(
        &self,
        runtime: &RuntimeName,
        repo_root: &Path,
        request: ImportRequest,
    ) -> Result<ImportResponse> {
        call_adapter_subprocess(repo_root, runtime, "import", request)
    }

    fn emit(
        &self,
        runtime: &RuntimeName,
        repo_root: &Path,
        request: EmitRequest,
    ) -> Result<EmitResponse> {
        call_adapter_subprocess(repo_root, runtime, "emit", request)
    }
}

/// Errors produced by core sync orchestration.
#[derive(Debug, Error)]
pub enum PipelineError {
    /// Filesystem operation failed.
    #[error("failed to {action} at {}", path.display())]
    Io {
        /// Operation being performed.
        action: &'static str,
        /// Path involved in the operation.
        path: PathBuf,
        /// Source IO error.
        #[source]
        source: std::io::Error,
    },
    /// Lockfile operation failed.
    #[error(transparent)]
    Lockfile(#[from] LockfileError),
    /// Machine-local state operation failed.
    #[error(transparent)]
    State(#[from] StateError),
    /// Identity operation failed.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// Entity merge operation failed.
    #[error(transparent)]
    Merge(#[from] MergeError),
    /// Typed value validation failed.
    #[error(transparent)]
    Type(#[from] TypeError),
    /// Mutex operation failed.
    #[error(transparent)]
    Mutex(#[from] MutexError),
    /// Pending queue operation failed.
    #[error(transparent)]
    Queue(#[from] PendingQueueError),
    /// Drainer operation failed.
    #[error(transparent)]
    Drainer(#[from] DrainerError),
    /// Configuration operation failed.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Adapter invocation failed.
    #[error("adapter `{runtime}` failed: {message}")]
    Adapter {
        /// Runtime whose adapter failed.
        runtime: RuntimeName,
        /// Human-readable adapter failure.
        message: String,
    },
    /// Adapter protocol transport failed.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    /// Requested emit is not supported by the target runtime.
    #[error("runtime `{runtime}` does not support `{entity_type}` for entity `{entity_id}`")]
    CapabilityMismatch {
        /// Runtime that lacks the capability.
        runtime: RuntimeName,
        /// Entity ID being emitted.
        entity_id: EntityId,
        /// Entity type being emitted.
        entity_type: EntityType,
    },
    /// The configured document fallback has no safe target for this entity.
    #[error(
        "runtime `{runtime}` cannot render `{entity_type}` for entity `{entity_id}` as a supported document"
    )]
    RenderAsDocUnsupported {
        /// Runtime that lacks a safe document target.
        runtime: RuntimeName,
        /// Entity ID being emitted.
        entity_id: EntityId,
        /// Entity type being emitted.
        entity_type: EntityType,
    },
    /// Entity content could not be translated into canonical form.
    #[error("failed to translate entity at {}: {message}", path.display())]
    EntityFormat {
        /// Path involved in the translation.
        path: PathBuf,
        /// Human-readable format error.
        message: String,
    },
    /// Requested entity was not present in the lockfile.
    #[error("entity `{entity_id}` is not present in agentmesh.lock")]
    EntityNotFound {
        /// Entity ID.
        entity_id: EntityId,
    },
    /// Entity has no canonical location to update.
    #[error("entity `{entity_id}` has no canonical location")]
    MissingCanonicalLocation {
        /// Entity ID.
        entity_id: EntityId,
    },
    /// No preserved conflict version was found.
    #[error("no preserved conflict version found for `{entity_id}` from `{runtime}`")]
    PreservedVersionNotFound {
        /// Entity ID.
        entity_id: EntityId,
        /// Runtime name.
        runtime: RuntimeName,
    },
    /// Local binary integrity pin is missing.
    #[error("missing binary integrity pin at {}", path.display())]
    IntegrityPinMissing {
        /// Expected integrity pin path.
        path: PathBuf,
    },
    /// Local binary integrity pin does not match the running binary.
    #[error("binary integrity pin mismatch: pinned {} ({}) but running {} ({})", pinned_path.display(), pinned_hash, current_path.display(), current_hash)]
    IntegrityMismatch {
        /// Pinned executable path.
        pinned_path: PathBuf,
        /// Pinned executable hash.
        pinned_hash: Hash,
        /// Running executable path.
        current_path: PathBuf,
        /// Running executable hash.
        current_hash: Hash,
    },
}

#[derive(Debug, Clone)]
struct EntityView {
    entity_type: EntityType,
    scope: Option<String>,
    location: LocationKey,
    relative_path: PathBuf,
    lockfile_path: PathBuf,
    canonical_contents: Vec<u8>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    runtime_overrides: BTreeMap<String, Value>,
    native_hash: Hash,
    mtime: SystemTime,
    id_pin: Option<EntityId>,
}

#[derive(Debug, Clone)]
struct EntityPlan {
    id: EntityId,
    entity_type: EntityType,
    writes: Vec<PlannedWrite>,
}

#[derive(Debug, Clone)]
struct PlannedWrite {
    location: LocationKey,
    lockfile_path: PathBuf,
    entity_file_path: PathBuf,
    absolute_path: PathBuf,
    contents: Vec<u8>,
    primary: bool,
}

#[derive(Debug, Clone)]
struct SyncPlan {
    lockfile: Lockfile,
    entities: Vec<EntityPlan>,
}

#[derive(Debug, Clone)]
struct PlanOptions {
    canonical_instructions: Option<CanonicalInstructions>,
    preserve_conflicts: bool,
}

impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            canonical_instructions: None,
            preserve_conflicts: true,
        }
    }
}

#[derive(Debug, Clone)]
struct ScannedEntities {
    views: BTreeMap<EntityId, BTreeMap<LocationKey, EntityView>>,
    renames: BTreeMap<EntityId, Vec<RenameRecord>>,
}

#[derive(Debug, Clone, Default)]
struct RuntimeMarkers {
    claude: bool,
    codex: bool,
    copilot: bool,
    cursor: bool,
    gemini: bool,
}

#[derive(Debug, Clone)]
struct CanonicalDecision {
    contents: Vec<u8>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    pending_conflict_resolution: bool,
}

#[derive(Debug, Clone)]
struct DetectedPreviousRename {
    entity_id: EntityId,
    record: RenameRecord,
}

/// Initializes core repository state.
pub fn init(repo_root: &Path, opts: InitOptions) -> Result<InitSummary> {
    init_with_adapter_registry(repo_root, opts, &SubprocessAdapterRegistry)
}

/// Initializes core repository state with an explicit adapter registry.
pub fn init_with_adapter_registry(
    repo_root: &Path,
    opts: InitOptions,
    adapters: &dyn AdapterRegistry,
) -> Result<InitSummary> {
    let cache = CacheLayout::new(&default_cache_root()?, repo_root)?;
    cache.ensure_dirs()?;
    write_integrity(repo_root, &cache)?;
    let summary = run_sync_with_plan_options(
        repo_root,
        SyncOptions {
            check: opts.dry_run,
            await_drain: !opts.dry_run,
            trigger: Some("core-init".to_string()),
            ..SyncOptions::default()
        },
        &cache,
        PlanOptions {
            canonical_instructions: opts.canonical_instructions,
            preserve_conflicts: !opts.dry_run,
        },
        adapters,
    )?;
    Ok(InitSummary {
        changed: summary.changed,
    })
}

/// Synchronizes repository state.
pub fn sync(repo_root: &Path, opts: SyncOptions) -> Result<SyncSummary> {
    sync_with_adapter_registry(repo_root, opts, &SubprocessAdapterRegistry)
}

/// Synchronizes repository state with an explicit adapter registry.
pub fn sync_with_adapter_registry(
    repo_root: &Path,
    opts: SyncOptions,
    adapters: &dyn AdapterRegistry,
) -> Result<SyncSummary> {
    let cache = CacheLayout::new(&default_cache_root()?, repo_root)?;
    cache.ensure_dirs()?;
    verify_integrity(repo_root, &cache)?;
    run_sync(repo_root, opts, &cache, adapters)
}

/// Updates the local binary integrity pin.
pub fn upgrade(repo_root: &Path) -> Result<UpgradeSummary> {
    let cache = CacheLayout::new(&default_cache_root()?, repo_root)?;
    cache.ensure_dirs()?;
    let current_pin = current_integrity_pin(repo_root)?;
    let previous = match read_integrity_pin(&cache.integrity_json) {
        Ok(pin) => Some(pin),
        Err(StateError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let changed = previous
        .as_ref()
        .map(|pin| {
            pin.binary_path != current_pin.binary_path
                || pin.binary_sha256 != current_pin.binary_sha256
                || pin.binary_version != current_pin.binary_version
        })
        .unwrap_or(true);
    if changed {
        write_integrity_pin(&cache.integrity_json, &current_pin)?;
    }

    Ok(UpgradeSummary { changed })
}

/// Removes local AgentMesh state and, when requested, repository-visible generated state.
pub fn uninstall(repo_root: &Path, opts: UninstallOptions) -> Result<UninstallSummary> {
    let cache = CacheLayout::new(&default_cache_root()?, repo_root)?;
    let mut removed_entries = Vec::new();

    if cache.root.exists() {
        fs::remove_dir_all(&cache.root).map_err(|source| PipelineError::Io {
            action: "remove directory",
            path: cache.root.clone(),
            source,
        })?;
        removed_entries.push(cache.root.display().to_string());
    }

    if opts.prune_repository_state {
        let lockfile = repo_root.join("agentmesh.lock");
        match fs::remove_file(&lockfile) {
            Ok(()) => removed_entries.push(lockfile.display().to_string()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(PipelineError::Io {
                    action: "remove file",
                    path: lockfile,
                    source,
                });
            }
        }

        let canonical_dir = repo_root.join(".ai");
        match fs::remove_dir_all(&canonical_dir) {
            Ok(()) => removed_entries.push(canonical_dir.display().to_string()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(PipelineError::Io {
                    action: "remove directory",
                    path: canonical_dir,
                    source,
                });
            }
        }

        let config = repo_root.join("agentmesh.config.yaml");
        match fs::remove_file(&config) {
            Ok(()) => removed_entries.push(config.display().to_string()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(PipelineError::Io {
                    action: "remove file",
                    path: config,
                    source,
                });
            }
        }
    }

    Ok(UninstallSummary { removed_entries })
}

/// Builds a health report from core state.
fn entity_location_hash(
    repo_root: &Path,
    entity_type: EntityType,
    location: &LocationKey,
    lockfile_path: &Path,
) -> Result<Option<Hash>> {
    let absolute_path = path_from_lockfile(repo_root, location, lockfile_path)?;
    if !is_regular_file_path(&absolute_path)? {
        return Ok(None);
    }
    if entity_type != EntityType::Skill {
        return sha256_file(&absolute_path).map(Some).map_err(Into::into);
    }
    let Some(root) = absolute_path.parent() else {
        return Ok(None);
    };
    if !is_regular_dir_path(root)? {
        return Ok(None);
    }
    let files = collect_entity_text_files(root, root)?;
    hash_entity_files(&files).map(Some)
}

/// Restores the latest preserved losing version for an entity/runtime pair.
pub fn restore(repo_root: &Path, entity_id: &EntityId, from: RuntimeName) -> Result<()> {
    restore_with_options(repo_root, entity_id, from, RestoreOptions::default()).map(|_| ())
}

/// Restores a preserved losing version for an entity/runtime pair.
pub fn restore_with_options(
    repo_root: &Path,
    entity_id: &EntityId,
    from: RuntimeName,
    opts: RestoreOptions,
) -> Result<RestoreSummary> {
    restore_with_options_and_adapter_registry(
        repo_root,
        entity_id,
        from,
        opts,
        &SubprocessAdapterRegistry,
    )
}

/// Restores a preserved losing version with an explicit adapter registry.
pub fn restore_with_options_and_adapter_registry(
    repo_root: &Path,
    entity_id: &EntityId,
    from: RuntimeName,
    opts: RestoreOptions,
    adapters: &dyn AdapterRegistry,
) -> Result<RestoreSummary> {
    let cache = CacheLayout::new(&default_cache_root()?, repo_root)?;
    cache.ensure_dirs()?;
    restore_with_cache(
        repo_root,
        &cache,
        entity_id,
        from,
        opts.at.as_deref(),
        opts.dry_run,
        adapters,
    )
}

/// Clears the pending conflict flag for one entity.
pub fn ack(repo_root: &Path, entity_id: &EntityId) -> Result<()> {
    let mut lockfile = read_lockfile(repo_root)?;
    let Some(entity) = lockfile.entities.get_mut(entity_id) else {
        return Err(PipelineError::EntityNotFound {
            entity_id: entity_id.clone(),
        });
    };
    entity.pending_conflict_resolution = None;
    write_lockfile(repo_root, &lockfile)?;
    Ok(())
}

/// Rebuilds the lockfile from the current filesystem when conflict markers are present.
pub fn reconcile_lock(repo_root: &Path) -> Result<ReconcileSummary> {
    reconcile_lock_with_adapter_registry(repo_root, &SubprocessAdapterRegistry)
}

/// Rebuilds the lockfile with an explicit adapter registry.
pub fn reconcile_lock_with_adapter_registry(
    repo_root: &Path,
    adapters: &dyn AdapterRegistry,
) -> Result<ReconcileSummary> {
    let path = repo_root.join("agentmesh.lock");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            let cache = CacheLayout::new(&default_cache_root()?, repo_root)?;
            let summary = run_sync(repo_root, SyncOptions::default(), &cache, adapters)?;
            return Ok(ReconcileSummary {
                changed: summary.changed,
            });
        }
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read file",
                path,
                source,
            });
        }
    };

    if !contents.contains("<<<<<<<") {
        return Ok(ReconcileSummary { changed: false });
    }

    let cache = CacheLayout::new(&default_cache_root()?, repo_root)?;
    let (left, right) = split_lockfile_conflict_sides(&contents);
    let left = parse_lockfile_side(&path, &left)?;
    let right = parse_lockfile_side(&path, &right)?;
    let union = union_lockfiles(left, right);
    let plan = build_sync_plan(repo_root, union, &cache, PlanOptions::default(), adapters)?;
    write_lockfile(repo_root, &plan.lockfile)?;
    cache.ensure_dirs()?;
    Ok(ReconcileSummary { changed: true })
}

fn split_lockfile_conflict_sides(contents: &str) -> (String, String) {
    enum Side {
        Both,
        Left,
        Right,
    }

    let mut side = Side::Both;
    let mut left = String::new();
    let mut right = String::new();

    for line in contents.lines() {
        if line.starts_with("<<<<<<<") {
            side = Side::Left;
            continue;
        }
        if line.starts_with("=======") {
            side = Side::Right;
            continue;
        }
        if line.starts_with(">>>>>>>") {
            side = Side::Both;
            continue;
        }

        match side {
            Side::Both => {
                left.push_str(line);
                left.push('\n');
                right.push_str(line);
                right.push('\n');
            }
            Side::Left => {
                left.push_str(line);
                left.push('\n');
            }
            Side::Right => {
                right.push_str(line);
                right.push('\n');
            }
        }
    }

    (left, right)
}

fn parse_lockfile_side(path: &Path, contents: &str) -> Result<Lockfile> {
    let lockfile = match serde_norway::from_str::<Lockfile>(contents) {
        Ok(lockfile) => lockfile,
        Err(_source) if !contents.contains("schema:") => return Ok(Lockfile::empty()),
        Err(source) => {
            return Err(PipelineError::Lockfile(LockfileError::Parse {
                path: path.to_path_buf(),
                source,
            }));
        }
    };
    lockfile.ensure_supported()?;
    Ok(lockfile)
}

fn union_lockfiles(mut left: Lockfile, right: Lockfile) -> Lockfile {
    left.version = left.version.max(right.version);
    left.schema = left.schema.max(right.schema);
    left.entities.extend(right.entities);
    left.overrides.extend(right.overrides);
    left.adapters.extend(right.adapters);
    left
}

fn run_sync(
    repo_root: &Path,
    opts: SyncOptions,
    cache: &CacheLayout,
    adapters: &dyn AdapterRegistry,
) -> Result<SyncSummary> {
    run_sync_with_plan_options(repo_root, opts, cache, PlanOptions::default(), adapters)
}

fn run_sync_with_plan_options(
    repo_root: &Path,
    opts: SyncOptions,
    cache: &CacheLayout,
    mut plan_options: PlanOptions,
    adapters: &dyn AdapterRegistry,
) -> Result<SyncSummary> {
    let sync_mutex = AgentmeshMutex::new(&cache.sync_in_progress_lock);
    let _sync_guard = if sync_requires_global_mutex(&opts) {
        Some(sync_mutex.acquire()?)
    } else {
        None
    };

    let queue = PendingQueue::new(&cache.pending_syncs_dir);
    let current_lockfile = read_lockfile_or_empty(repo_root)?;
    if opts.drain_pending {
        let drain = drain_pending_records(repo_root, cache, &queue, adapters)?;
        return Ok(SyncSummary {
            changed: drain.processed > 0,
            entities_changed: 0,
            pending_enqueued: 0,
            pending_drained: drain.processed,
            pending_conflicts: count_pending_conflicts(&current_lockfile),
            capability_skipped: 0,
            capability_skips: Vec::new(),
        });
    }

    let config = load_config(repo_root)?.config;
    plan_options.preserve_conflicts = plan_options.preserve_conflicts && !opts.check;
    let plan = build_sync_plan(
        repo_root,
        current_lockfile.clone(),
        cache,
        plan_options,
        adapters,
    )?;
    let lockfile_changed = plan.lockfile != current_lockfile;
    let file_changes = planned_file_changes(&plan)?;
    let lockfile_entity_changes = lockfile_entity_changes(&current_lockfile, &plan.lockfile);
    let pending_emit_changes =
        pending_emit_entities(&current_lockfile, &plan.lockfile, &plan.entities);
    let mut changed_entities = file_changes.clone();
    changed_entities.extend(lockfile_entity_changes);
    changed_entities.extend(pending_emit_changes);
    let capability_report = capability_skip_report_for_lockfile(
        &plan.lockfile,
        &config,
        CapabilityReportMode::Enforce,
    )?;
    let capability_skipped = capability_report.skipped;
    let pending_ready_count = queue.read_ready()?.len();
    let changed = lockfile_changed
        || !file_changes.is_empty()
        || capability_skipped > 0
        || (opts.check && pending_ready_count > 0);

    if opts.check {
        return Ok(SyncSummary {
            changed,
            entities_changed: changed_entities.len(),
            pending_enqueued: 0,
            pending_drained: 0,
            pending_conflicts: count_pending_conflicts(&plan.lockfile),
            capability_skipped,
            capability_skips: capability_report.findings,
        });
    }

    if changed {
        let state_mutex = AgentmeshMutex::new(&cache.state_lock);
        for entity in &plan.entities {
            if entity_has_change(entity, &file_changes) {
                let _state_guard = state_mutex.acquire()?;
                for write in &entity.writes {
                    if needs_write(&write.absolute_path, &write.contents)? {
                        write_atomic(&write.absolute_path, &write.contents)?;
                    }
                }
            }
        }
        let _state_guard = state_mutex.acquire()?;
        write_lockfile(repo_root, &plan.lockfile)?;
    }

    let enqueued = if changed {
        enqueue_changed_entities(repo_root, &queue, &plan, &changed_entities, &opts)?
    } else {
        0
    };
    if enqueued > 0 && should_kick_background_drainer(&opts) {
        kick_background_drainer(repo_root, cache)?;
    }
    let drain = if opts.await_drain {
        drain_pending_records(repo_root, cache, &queue, adapters)?
    } else {
        Default::default()
    };

    Ok(SyncSummary {
        changed,
        entities_changed: changed_entities.len(),
        pending_enqueued: enqueued,
        pending_drained: drain.processed,
        pending_conflicts: count_pending_conflicts(&plan.lockfile),
        capability_skipped,
        capability_skips: capability_report.findings,
    })
}

fn sync_requires_global_mutex(opts: &SyncOptions) -> bool {
    !opts.drain_pending
        && !matches!(
            opts.trigger.as_deref(),
            Some("claude-hook") | Some("codex-hook") | Some("watcher")
        )
}

fn should_kick_background_drainer(opts: &SyncOptions) -> bool {
    !opts.await_drain
        && !opts.background
        && !opts.check
        && !opts.drain_pending
        && matches!(
            opts.trigger.as_deref(),
            Some("claude-hook") | Some("codex-hook")
        )
}

#[cfg(test)]
fn kick_background_drainer(_repo_root: &Path, _cache: &CacheLayout) -> Result<()> {
    Ok(())
}

#[cfg(not(test))]
fn kick_background_drainer(repo_root: &Path, cache: &CacheLayout) -> Result<()> {
    if cache.watcher_pid.exists() {
        return Ok(());
    }
    let worker = AgentmeshMutex::new(&cache.worker_lock);
    let Some(guard) = worker.try_acquire()? else {
        return Ok(());
    };
    drop(guard);

    let current_exe = std::env::current_exe().map_err(|source| PipelineError::Io {
        action: "resolve current executable",
        path: repo_root.to_path_buf(),
        source,
    })?;
    Command::new(current_exe)
        .arg("sync")
        .arg("--background")
        .arg("--drain-pending")
        .arg("--silent")
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|source| PipelineError::Io {
            action: "spawn background drainer",
            path: repo_root.to_path_buf(),
            source,
        })
}

fn count_pending_conflicts(lockfile: &Lockfile) -> usize {
    lockfile
        .entities
        .values()
        .filter(|entry| entry.pending_conflict_resolution == Some(true))
        .count()
}

fn normalize_canonical_contents(entity_type: EntityType, contents: Vec<u8>) -> Result<Vec<u8>> {
    if entity_type != EntityType::Instructions {
        return Ok(contents);
    }
    let text = String::from_utf8(contents).map_err(|source| PipelineError::EntityFormat {
        path: PathBuf::from("<canonical>"),
        message: source.to_string(),
    })?;
    Ok(strip_empty_frontmatter(&text).as_bytes().to_vec())
}

fn normalize_canonical_files(
    entity_type: EntityType,
    mut files: BTreeMap<PathBuf, Vec<u8>>,
    primary_contents: &[u8],
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let key = primary_file_key(entity_type, &files)
        .unwrap_or_else(|| primary_entity_file_path(entity_type));
    files.insert(key, primary_contents.to_vec());
    Ok(files)
}

fn pending_emit_entities(
    previous: &Lockfile,
    current: &Lockfile,
    entities: &[EntityPlan],
) -> BTreeSet<EntityId> {
    let mut pending = BTreeSet::new();
    let previous_adapters = previous.adapters.keys().collect::<BTreeSet<_>>();
    let current_adapters = current.adapters.keys().collect::<BTreeSet<_>>();
    if previous_adapters != current_adapters {
        pending.extend(entities.iter().map(|entity| entity.id.clone()));
    }
    for entity in entities {
        let previous_hash = previous
            .entities
            .get(&entity.id)
            .map(|entry| entry.canonical_sha256.clone());
        let current_hash = current
            .entities
            .get(&entity.id)
            .map(|entry| entry.canonical_sha256.clone());
        if previous_hash != current_hash {
            pending.insert(entity.id.clone());
        }
    }
    pending
}

#[cfg(test)]
fn capability_skip_count_for_lockfile(
    lockfile: &Lockfile,
    config: &AgentmeshConfig,
) -> Result<usize> {
    Ok(
        capability_skip_report_for_lockfile(lockfile, config, CapabilityReportMode::Enforce)?
            .skipped,
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CapabilitySkipReport {
    skipped: usize,
    findings: Vec<CapabilitySkipFinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilityReportMode {
    Enforce,
    Diagnostic,
}

fn capability_skip_report_for_lockfile(
    lockfile: &Lockfile,
    config: &AgentmeshConfig,
    mode: CapabilityReportMode,
) -> Result<CapabilitySkipReport> {
    let mut report = CapabilitySkipReport::default();
    for (entity_id, entity) in &lockfile.entities {
        for (runtime, adapter) in &lockfile.adapters {
            if adapter.entities.contains(&entity.entity_type) {
                continue;
            }
            let fallback = capability_fallback(config, runtime, entity.entity_type);
            match fallback {
                CapabilityFallback::Skip => {}
                CapabilityFallback::Warn => {
                    report.skipped += 1;
                    report.findings.push(capability_skip_finding(
                        runtime, entity_id, entity, fallback,
                    ));
                }
                CapabilityFallback::RenderAsDoc => {
                    if mode == CapabilityReportMode::Enforce {
                        return Err(PipelineError::RenderAsDocUnsupported {
                            runtime: runtime.clone(),
                            entity_id: entity_id.clone(),
                            entity_type: entity.entity_type,
                        });
                    }
                    report.skipped += 1;
                    report.findings.push(capability_skip_finding(
                        runtime, entity_id, entity, fallback,
                    ));
                }
                CapabilityFallback::Fail => {
                    if mode == CapabilityReportMode::Diagnostic {
                        report.skipped += 1;
                        report.findings.push(capability_skip_finding(
                            runtime, entity_id, entity, fallback,
                        ));
                        continue;
                    }
                    return Err(PipelineError::CapabilityMismatch {
                        runtime: runtime.clone(),
                        entity_id: entity_id.clone(),
                        entity_type: entity.entity_type,
                    });
                }
            }
        }
    }
    Ok(report)
}

fn capability_skip_finding(
    runtime: &RuntimeName,
    entity_id: &EntityId,
    entity: &LockfileEntity,
    fallback: CapabilityFallback,
) -> CapabilitySkipFinding {
    CapabilitySkipFinding {
        runtime: runtime.clone(),
        entity_id: entity_id.clone(),
        entity_type: entity.entity_type,
        fallback,
        locations: entity.locations.clone(),
    }
}

fn capability_fallback(
    config: &AgentmeshConfig,
    runtime: &RuntimeName,
    entity_type: EntityType,
) -> CapabilityFallback {
    config
        .fallbacks
        .get(runtime)
        .and_then(|fallbacks| fallbacks.get(entity_type.as_str()))
        .copied()
        .unwrap_or(CapabilityFallback::Warn)
}

fn capability_fallback_name(fallback: CapabilityFallback) -> &'static str {
    match fallback {
        CapabilityFallback::Skip => "skip",
        CapabilityFallback::Warn => "warn",
        CapabilityFallback::RenderAsDoc => "render-as-doc",
        CapabilityFallback::Fail => "fail",
    }
}

fn capability_skip_finding_message(finding: &CapabilitySkipFinding) -> String {
    let locations = if finding.locations.is_empty() {
        "none".to_string()
    } else {
        finding
            .locations
            .iter()
            .map(|(location, path)| format!("{}:{}", location.as_str(), path.display()))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "capability_skip: runtime={} entity={} type={} fallback={} locations={}",
        finding.runtime.as_str(),
        finding.entity_id.as_str(),
        finding.entity_type.as_str(),
        capability_fallback_name(finding.fallback),
        locations
    )
}

fn build_sync_plan(
    repo_root: &Path,
    previous: Lockfile,
    cache: &CacheLayout,
    options: PlanOptions,
    adapters: &dyn AdapterRegistry,
) -> Result<SyncPlan> {
    let markers = detect_runtime_markers(repo_root, adapters)?;
    let scanned = scan_entities(repo_root, &previous, adapters)?;
    let mut lockfile = Lockfile::empty();
    lockfile.adapters = adapter_declarations(&markers)?;

    let mut entities = Vec::new();
    for (entity_id, views) in scanned.views {
        let Some(first_view) = views.values().next() else {
            continue;
        };
        let entity_type = first_view.entity_type;
        let previous_entry = previous.entities.get(&entity_id);
        let canonical = resolve_canonical_contents(
            cache,
            &entity_id,
            entity_type,
            &views,
            previous_entry,
            options.canonical_instructions,
            options.preserve_conflicts,
        )?;
        let canonical_contents = normalize_canonical_contents(entity_type, canonical.contents)?;
        let canonical_files =
            normalize_canonical_files(entity_type, canonical.files, &canonical_contents)?;
        let output_slug = if let Some(renames) = scanned.renames.get(&entity_id) {
            renames
                .last()
                .and_then(|rename| slug_from_lockfile_path(entity_type, &rename.to))
                .unwrap_or_else(|| entity_slug(&entity_id).to_string())
        } else {
            entity_slug(&entity_id).to_string()
        };
        let writes = planned_writes(
            repo_root,
            &markers,
            &output_slug,
            entity_type,
            canonical_files.clone(),
        )?;
        let mut locations = BTreeMap::new();
        let mut emitted = BTreeMap::new();

        for write in &writes {
            if write.primary {
                locations.insert(write.location.clone(), write.lockfile_path.clone());
            }
        }
        for (location, hash) in planned_location_hashes(entity_type, &writes)? {
            emitted.insert(location, hash);
        }
        for view in views.values() {
            locations
                .entry(view.location.clone())
                .or_insert_with(|| view.lockfile_path.clone());
            emitted
                .entry(view.location.clone())
                .or_insert_with(|| view.native_hash.clone());
        }

        let mut rename_history = previous_entry
            .map(|entry| entry.rename_history.clone())
            .unwrap_or_default();
        if let Some(renames) = scanned.renames.get(&entity_id) {
            rename_history.extend(renames.clone());
        }

        let id_pin = views
            .values()
            .find_map(|view| view.id_pin.clone())
            .or_else(|| previous_entry.and_then(|entry| entry.id_pin.clone()));
        let lockfile_entity = LockfileEntity {
            entity_type,
            entity_schema: entity_schema_for_type(entity_type),
            scope: views
                .values()
                .find_map(|view| view.scope.clone())
                .or_else(|| previous_entry.and_then(|entry| entry.scope.clone()))
                .or_else(|| instruction_scope(&entity_id, entity_type)),
            locations,
            canonical_sha256: hash_entity_payload(
                entity_type,
                &canonical_files,
                &canonical_contents,
            )?,
            emitted_native_sha256: emitted,
            lineage: previous_entry
                .map(|entry| entry.lineage.clone())
                .unwrap_or_default(),
            pending_conflict_resolution: if canonical.pending_conflict_resolution
                || previous_entry
                    .and_then(|entry| entry.pending_conflict_resolution)
                    .unwrap_or(false)
            {
                Some(true)
            } else {
                None
            },
            rename_history,
            id_pin,
        };

        let mut entity_overrides = previous
            .overrides
            .get(&entity_id)
            .cloned()
            .unwrap_or_default();
        let mut current_override_runtimes = BTreeSet::new();
        for view in views.values() {
            if let Some(runtime) = runtime_override_location(&view.location)? {
                current_override_runtimes.insert(runtime);
            }
        }
        entity_overrides.retain(|runtime, _| current_override_runtimes.contains(runtime));
        for view in views.values() {
            if let Some(runtime) = runtime_override_location(&view.location)? {
                if view.runtime_overrides.is_empty() {
                    entity_overrides.remove(&runtime);
                } else {
                    entity_overrides.insert(runtime, OverrideEntry(view.runtime_overrides.clone()));
                }
            }
        }
        if !entity_overrides.is_empty() {
            lockfile
                .overrides
                .insert(entity_id.clone(), entity_overrides);
        }

        lockfile
            .entities
            .insert(entity_id.clone(), lockfile_entity.clone());
        entities.push(EntityPlan {
            id: entity_id,
            entity_type,
            writes,
        });
    }

    Ok(SyncPlan { lockfile, entities })
}

fn scan_entities(
    repo_root: &Path,
    previous: &Lockfile,
    adapters: &dyn AdapterRegistry,
) -> Result<ScannedEntities> {
    let mut raw_views = Vec::new();
    for candidate in entity_candidates(repo_root)? {
        let base_entity_id = derive_entity_id(&candidate.relative_path)?;
        let location = candidate.location_key.clone();
        let absolute_path = repo_root.join(&candidate.relative_path);
        let contents = read_text_file(&absolute_path)?;
        let id_pin = parse_pin_marker(&contents)?;
        let canonical_contents =
            canonicalize_for_candidate(candidate.entity_type, &candidate.relative_path, &contents)?;
        let files = entity_files_for_candidate(repo_root, &candidate, canonical_contents.clone())?;
        let native_hash = hash_entity_payload(candidate.entity_type, &files, &canonical_contents)?;
        let mtime = fs::metadata(&absolute_path)
            .and_then(|metadata| metadata.modified())
            .map_err(|source| PipelineError::Io {
                action: "read file metadata",
                path: absolute_path.clone(),
                source,
            })?;
        let view = EntityView {
            entity_type: candidate.entity_type,
            scope: None,
            location: location.clone(),
            relative_path: candidate.relative_path,
            lockfile_path: candidate.lockfile_path,
            canonical_contents,
            files,
            runtime_overrides: BTreeMap::new(),
            native_hash,
            mtime,
            id_pin,
        };
        raw_views.push((base_entity_id, view));
    }
    for runtime in detected_adapter_runtimes(repo_root, adapters)? {
        let response = import_runtime_entities_hot(repo_root, &runtime, adapters)?;
        validate_import_skips(&runtime, &response.skipped)?;
        for imported in response.entities {
            let (base_entity_id, view) = entity_view_from_import(repo_root, &runtime, imported)?;
            raw_views.push((base_entity_id, view));
        }
    }

    let mut entities: BTreeMap<EntityId, BTreeMap<LocationKey, EntityView>> = BTreeMap::new();
    let mut renames: BTreeMap<EntityId, Vec<RenameRecord>> = BTreeMap::new();
    let mut occupied = BTreeSet::new();
    for existing in previous.entities.keys() {
        occupied.insert(existing.clone());
    }

    for (base_entity_id, view) in raw_views {
        let mut entity_id = view
            .id_pin
            .clone()
            .or_else(|| previous_id_for_view(previous, &view))
            .or_else(|| {
                detect_previous_rename(repo_root, previous, &view).map(|detected| {
                    renames
                        .entry(detected.entity_id.clone())
                        .or_default()
                        .push(detected.record);
                    detected.entity_id
                })
            })
            .unwrap_or(base_entity_id);

        if is_duplicate_shared_view(&entities, &entity_id, &view) {
            continue;
        }
        if entity_requires_collision_resolution(&entities, &entity_id, &view) {
            entity_id = resolve_collision(&entity_id, &occupied)?;
        }
        occupied.insert(entity_id.clone());
        entities
            .entry(entity_id)
            .or_default()
            .insert(view.location.clone(), view);
    }

    Ok(ScannedEntities {
        views: entities,
        renames,
    })
}

fn is_duplicate_shared_view(
    entities: &BTreeMap<EntityId, BTreeMap<LocationKey, EntityView>>,
    entity_id: &EntityId,
    view: &EntityView,
) -> bool {
    if view.location.as_str() != ".agents" {
        return false;
    }
    entities
        .get(entity_id)
        .and_then(|views| views.get(&view.location))
        .is_some_and(|existing| {
            existing.entity_type == view.entity_type
                && existing.relative_path == view.relative_path
                && existing.lockfile_path == view.lockfile_path
                && existing.canonical_contents == view.canonical_contents
                && existing.files == view.files
        })
}

fn detected_adapter_runtimes(
    repo_root: &Path,
    adapters: &dyn AdapterRegistry,
) -> Result<Vec<RuntimeName>> {
    let mut runtimes = Vec::new();
    for runtime in [
        runtime_name("claude")?,
        runtime_name("codex")?,
        runtime_name("copilot")?,
        runtime_name("cursor")?,
        runtime_name("gemini")?,
    ] {
        let present = adapters.detect(&runtime, repo_root)?.present;
        if present {
            runtimes.push(runtime);
        }
    }
    Ok(runtimes)
}

fn import_runtime_entities_hot(
    repo_root: &Path,
    runtime: &RuntimeName,
    adapters: &dyn AdapterRegistry,
) -> Result<ImportResponse> {
    adapters.import(
        runtime,
        repo_root,
        ImportRequest {
            canonical_dir: repo_root.join(".ai"),
            runtime_dir: runtime_dir(repo_root, runtime),
            filter: None,
        },
    )
}

fn validate_import_skips(
    runtime: &RuntimeName,
    skipped: &[agentmesh_protocol::SkippedPath],
) -> Result<()> {
    for skipped in skipped {
        match runtime.as_str() {
            "cursor" if skipped.path.starts_with(".cursor/rules") => {
                return Err(PipelineError::EntityFormat {
                    path: skipped.path.clone(),
                    message: skipped.reason.clone(),
                });
            }
            "copilot"
                if skipped.path == Path::new(".github/copilot-instructions.md")
                    || skipped.path.starts_with(".github/instructions")
                    || skipped.path.starts_with(".github/prompts")
                    || skipped.path.starts_with(".github/skills")
                    || skipped.path.starts_with(".github/agents")
                    || skipped.path.starts_with(".agents/skills") =>
            {
                return Err(PipelineError::EntityFormat {
                    path: skipped.path.clone(),
                    message: skipped.reason.clone(),
                });
            }
            "gemini"
                if skipped.path == Path::new("GEMINI.md")
                    || (skipped.path.file_name().and_then(|name| name.to_str())
                        == Some("GEMINI.md")
                        && gemini_repo_relative_context_path(&skipped.path))
                    || skipped.path.starts_with(".gemini/skills")
                    || skipped.path.starts_with(".gemini/commands")
                    || skipped.path.starts_with(".agents/skills") =>
            {
                return Err(PipelineError::EntityFormat {
                    path: skipped.path.clone(),
                    message: skipped.reason.clone(),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

fn entity_view_from_import(
    repo_root: &Path,
    runtime: &RuntimeName,
    imported: ImportedEntity,
) -> Result<(EntityId, EntityView)> {
    let base_entity_id = EntityId::new(imported.id.clone())?;
    let location = location_for_import(runtime, &imported)?;
    let absolute_path = repo_root.join(&imported.source_path);
    let mut canonical_text = imported_primary_content(&imported)?;
    canonical_text = match imported.entity_type {
        EntityType::Instructions => strip_empty_frontmatter(&canonical_text).to_string(),
        EntityType::Skill
        | EntityType::Subagent
        | EntityType::Rule
        | EntityType::Prompt
        | EntityType::Command
            if is_markdown_path(&imported.source_path) =>
        {
            crate::merge::canonicalize_markdown(&canonical_text)?
        }
        EntityType::Rule
        | EntityType::Prompt
        | EntityType::Command
        | EntityType::Hook
        | EntityType::McpBinding
        | EntityType::PermissionPolicy
        | EntityType::Skill
        | EntityType::Subagent => canonical_text,
    };
    let mut files = files_from_imported_entity(&imported)?;
    if let Some(primary_key) = primary_file_key(imported.entity_type, &files) {
        files.insert(primary_key, canonical_text.clone().into_bytes());
    }
    let canonical_contents = canonical_text.into_bytes();
    let id_pin = parse_pin_marker(&String::from_utf8_lossy(&canonical_contents))?;
    let native_hash = imported_native_hash(repo_root, &imported, &files)?;
    let mtime = fs::metadata(&absolute_path)
        .and_then(|metadata| metadata.modified())
        .map_err(|source| PipelineError::Io {
            action: "read file metadata",
            path: absolute_path,
            source,
        })?;
    let lockfile_path = lockfile_path_for_import(&location, &imported.source_path);
    let runtime_overrides = runtime_overrides_for_import(runtime, &imported);
    let view = EntityView {
        entity_type: imported.entity_type,
        scope: imported.scope,
        location,
        relative_path: imported.source_path,
        lockfile_path,
        canonical_contents,
        files,
        runtime_overrides,
        native_hash,
        mtime,
        id_pin,
    };
    Ok((base_entity_id, view))
}

fn runtime_overrides_for_import(
    runtime: &RuntimeName,
    imported: &ImportedEntity,
) -> BTreeMap<String, Value> {
    match runtime.as_str() {
        "cursor" if imported.source_path.starts_with(".cursor/rules") => imported
            .frontmatter
            .iter()
            .filter(|(key, _)| key.as_str() != "description")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        "copilot" if imported.source_path.starts_with(".github/instructions") => imported
            .frontmatter
            .iter()
            .filter(|(key, _)| key.as_str() != "description")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        "copilot" if imported.source_path.starts_with(".github/prompts") => imported
            .frontmatter
            .iter()
            .filter(|(key, _)| key.as_str() != "description")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        "copilot" if imported.source_path.starts_with(".github/skills") => imported
            .frontmatter
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "name" | "description"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        "copilot" if imported.source_path.starts_with(".github/agents") => imported
            .frontmatter
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "name" | "description"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        "gemini" if imported.source_path.starts_with(".gemini/skills") => imported
            .frontmatter
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "name" | "description"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        "gemini" if imported.source_path.starts_with(".gemini/commands") => imported
            .frontmatter
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "description" | "prompt"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        _ => BTreeMap::new(),
    }
}

fn strip_empty_frontmatter(contents: &str) -> &str {
    contents
        .strip_prefix("---\n{}\n---\n")
        .or_else(|| contents.strip_prefix("---\n---\n"))
        .unwrap_or(contents)
}

fn location_for_import(runtime: &RuntimeName, imported: &ImportedEntity) -> Result<LocationKey> {
    if imported.source_path.starts_with(".agents") {
        return location_key(".agents").map_err(Into::into);
    }
    location_key(&format!(".{}", runtime.as_str())).map_err(Into::into)
}

fn runtime_override_location(location: &LocationKey) -> Result<Option<RuntimeName>> {
    match location.as_str() {
        ".ai" | ".agents" => Ok(None),
        value => value
            .strip_prefix('.')
            .map(RuntimeName::new)
            .transpose()
            .map_err(Into::into),
    }
}

fn lockfile_path_for_import(location: &LocationKey, source_path: &Path) -> PathBuf {
    if !source_path.starts_with(location.as_str()) {
        return PathBuf::from("..").join(source_path);
    }
    source_path
        .strip_prefix(location.as_str())
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| source_path.to_path_buf())
}

fn imported_primary_content(imported: &ImportedEntity) -> Result<String> {
    let file = imported
        .files
        .get(Path::new("SKILL.md"))
        .or_else(|| imported.files.get(Path::new("AGENTS.md")))
        .or_else(|| imported.files.get(Path::new("CLAUDE.md")))
        .or_else(|| imported.files.values().next())
        .ok_or_else(|| PipelineError::EntityFormat {
            path: imported.source_path.clone(),
            message: "imported entity has no files".to_string(),
        })?;
    match file.encoding {
        agentmesh_protocol::EntityFileEncoding::Utf8 => Ok(file.content.clone()),
        agentmesh_protocol::EntityFileEncoding::Base64 => Err(PipelineError::EntityFormat {
            path: imported.source_path.clone(),
            message: "imported primary file must be UTF-8 text".to_string(),
        }),
    }
}

fn files_from_imported_entity(imported: &ImportedEntity) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for (path, file) in &imported.files {
        if !is_safe_entity_file_path(path) {
            return Err(PipelineError::EntityFormat {
                path: imported.source_path.clone(),
                message: format!("unsafe imported entity file path {}", path.display()),
            });
        }
        let contents = file
            .decode_bytes()
            .map_err(|source| PipelineError::EntityFormat {
                path: imported.source_path.clone(),
                message: format!(
                    "failed to decode imported file {}: {source}",
                    path.display()
                ),
            })?;
        files.insert(path.clone(), contents);
    }
    Ok(files)
}

fn imported_native_hash(
    repo_root: &Path,
    imported: &ImportedEntity,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Result<Hash> {
    if imported.entity_type == EntityType::Skill {
        return hash_entity_files(files);
    }
    sha256_file(&repo_root.join(&imported.source_path)).map_err(Into::into)
}

fn primary_file_key(
    entity_type: EntityType,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Option<PathBuf> {
    match entity_type {
        EntityType::Instructions => files
            .get_key_value(Path::new("AGENTS.md"))
            .or_else(|| files.get_key_value(Path::new("CLAUDE.md")))
            .or_else(|| files.get_key_value(Path::new("GEMINI.md")))
            .map(|(path, _)| path.clone())
            .or_else(|| files.keys().next().cloned()),
        EntityType::Skill => files
            .get_key_value(Path::new("SKILL.md"))
            .map(|(path, _)| path.clone()),
        EntityType::Rule
        | EntityType::Prompt
        | EntityType::Command
        | EntityType::Hook
        | EntityType::McpBinding
        | EntityType::PermissionPolicy
        | EntityType::Subagent => files.keys().next().cloned(),
    }
}

fn primary_file_contents(
    entity_type: EntityType,
    files: &BTreeMap<PathBuf, Vec<u8>>,
) -> Option<Vec<u8>> {
    primary_file_key(entity_type, files).and_then(|key| files.get(&key).cloned())
}

fn hash_entity_payload(
    entity_type: EntityType,
    files: &BTreeMap<PathBuf, Vec<u8>>,
    primary_contents: &[u8],
) -> Result<Hash> {
    if entity_type == EntityType::Skill {
        return hash_entity_files(files);
    }
    sha256_bytes(primary_contents).map_err(Into::into)
}

fn hash_entity_files(files: &BTreeMap<PathBuf, Vec<u8>>) -> Result<Hash> {
    let mut bytes = Vec::new();
    for (path, contents) in files {
        bytes.extend_from_slice(path.as_os_str().as_encoded_bytes());
        bytes.push(0);
        bytes.extend_from_slice(contents);
        bytes.push(0);
    }
    sha256_bytes(&bytes).map_err(Into::into)
}

fn is_safe_entity_file_path(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn previous_id_for_view(previous: &Lockfile, view: &EntityView) -> Option<EntityId> {
    previous
        .entities
        .iter()
        .find(|(_, entity)| {
            entity.entity_type == view.entity_type
                && entity
                    .locations
                    .get(&view.location)
                    .map(|path| path == &view.lockfile_path)
                    .unwrap_or(false)
        })
        .map(|(entity_id, _)| entity_id.clone())
}

fn detect_previous_rename(
    repo_root: &Path,
    previous: &Lockfile,
    view: &EntityView,
) -> Option<DetectedPreviousRename> {
    for (entity_id, entity) in &previous.entities {
        if entity.entity_type != view.entity_type {
            continue;
        }
        let Some(previous_path) = entity.locations.get(&view.location) else {
            continue;
        };
        if previous_path == &view.lockfile_path {
            continue;
        }
        let Ok(previous_absolute) = path_from_lockfile(repo_root, &view.location, previous_path)
        else {
            continue;
        };
        if previous_absolute.exists() {
            continue;
        }
        let Some(previous_hash) = entity.emitted_native_sha256.get(&view.location) else {
            continue;
        };
        let candidate = RenameCandidate {
            path: view.relative_path.clone(),
            hash: view.native_hash.clone(),
            contents: Some(String::from_utf8_lossy(&view.canonical_contents).to_string()),
        };
        let detected = detect_rename(previous_path, previous_hash, None, &[candidate], 0.8)?;

        return Some(DetectedPreviousRename {
            entity_id: entity_id.clone(),
            record: RenameRecord {
                from: detected.from,
                to: view.lockfile_path.clone(),
                at: timestamp_now(),
            },
        });
    }

    None
}

fn entity_requires_collision_resolution(
    entities: &BTreeMap<EntityId, BTreeMap<LocationKey, EntityView>>,
    entity_id: &EntityId,
    view: &EntityView,
) -> bool {
    let Some(existing_views) = entities.get(entity_id) else {
        return false;
    };

    existing_views.contains_key(&view.location)
        || existing_views
            .values()
            .any(|existing| existing.entity_type != view.entity_type)
}

fn resolve_canonical_contents(
    cache: &CacheLayout,
    entity_id: &EntityId,
    entity_type: EntityType,
    views: &BTreeMap<LocationKey, EntityView>,
    previous_entry: Option<&LockfileEntity>,
    canonical_instructions: Option<CanonicalInstructions>,
    preserve_conflicts: bool,
) -> Result<CanonicalDecision> {
    if entity_type == EntityType::Instructions && previous_entry.is_none() {
        if let Some(preferred) = preferred_instruction_view(views, canonical_instructions) {
            return Ok(CanonicalDecision {
                contents: preferred.canonical_contents.clone(),
                files: preferred.files.clone(),
                pending_conflict_resolution: false,
            });
        }
    }

    if all_canonical_payloads_equal(views) {
        let Some(chosen) = choose_canonical_view(views) else {
            return Ok(CanonicalDecision {
                contents: Vec::new(),
                files: BTreeMap::new(),
                pending_conflict_resolution: false,
            });
        };
        return Ok(CanonicalDecision {
            contents: chosen.canonical_contents.clone(),
            files: chosen.files.clone(),
            pending_conflict_resolution: false,
        });
    }

    let changed = changed_views(views, previous_entry);
    if changed.is_empty() {
        let Some(chosen) = choose_canonical_view(views) else {
            return Ok(CanonicalDecision {
                contents: Vec::new(),
                files: BTreeMap::new(),
                pending_conflict_resolution: false,
            });
        };
        return Ok(CanonicalDecision {
            contents: chosen.canonical_contents.clone(),
            files: chosen.files.clone(),
            pending_conflict_resolution: false,
        });
    }
    if changed.len() == 1 {
        return Ok(CanonicalDecision {
            contents: changed[0].canonical_contents.clone(),
            files: changed[0].files.clone(),
            pending_conflict_resolution: false,
        });
    }

    if (!entity_type.is_configuration_only() || entity_type == EntityType::Command)
        && entity_type != EntityType::Instructions
        && changed.iter().all(|view| is_markdown_view(view))
    {
        if let Some(ancestor) = unchanged_ancestor_view(views, previous_entry) {
            return merge_changed_views(cache, entity_id, ancestor, &changed, preserve_conflicts);
        }
    }

    tiebreak_changed_views(cache, entity_id, &changed, preserve_conflicts)
}

fn preferred_instruction_view(
    views: &BTreeMap<LocationKey, EntityView>,
    canonical_instructions: Option<CanonicalInstructions>,
) -> Option<&EntityView> {
    let key = match canonical_instructions {
        Some(CanonicalInstructions::AgentsMd) => location_key(".ai").ok()?,
        Some(CanonicalInstructions::ClaudeMd) => location_key(".claude").ok()?,
        None => return None,
    };
    views.get(&key)
}

fn all_canonical_payloads_equal(views: &BTreeMap<LocationKey, EntityView>) -> bool {
    let mut iter = views.values();
    let Some(first) = iter.next() else {
        return true;
    };
    iter.all(|view| {
        view.canonical_contents == first.canonical_contents && view.files == first.files
    })
}

fn changed_views<'a>(
    views: &'a BTreeMap<LocationKey, EntityView>,
    previous_entry: Option<&LockfileEntity>,
) -> Vec<&'a EntityView> {
    views
        .values()
        .filter(|view| {
            previous_entry
                .and_then(|entry| entry.emitted_native_sha256.get(&view.location))
                .map(|hash| hash != &view.native_hash)
                .unwrap_or(true)
        })
        .collect()
}

fn unchanged_ancestor_view<'a>(
    views: &'a BTreeMap<LocationKey, EntityView>,
    previous_entry: Option<&LockfileEntity>,
) -> Option<&'a EntityView> {
    let previous_entry = previous_entry?;
    views.values().find(|view| {
        previous_entry
            .emitted_native_sha256
            .get(&view.location)
            .map(|hash| hash == &view.native_hash)
            .unwrap_or(false)
    })
}

fn merge_changed_views(
    cache: &CacheLayout,
    entity_id: &EntityId,
    ancestor: &EntityView,
    changed: &[&EntityView],
    preserve_conflicts: bool,
) -> Result<CanonicalDecision> {
    let mut current = changed[0];
    let ancestor_text = canonical_text(ancestor)?;
    let mut merged_text = canonical_text(current)?;
    let mut pending_conflict_resolution = false;

    for incoming in changed.iter().skip(1) {
        let current_text = merged_text.clone();
        let incoming_text = canonical_text(incoming)?;
        let merge = merge_markdown(
            &ancestor_text,
            &current_text,
            &incoming_text,
            current.mtime,
            incoming.mtime,
        )?;

        if let MergeStatus::Tiebreaker { winner, loser } = merge.status {
            pending_conflict_resolution = true;
            let (losing_runtime, losing_contents) = match loser {
                MergeSide::Current => (runtime_for_location(&current.location)?, current_text),
                MergeSide::Incoming => (runtime_for_location(&incoming.location)?, incoming_text),
            };
            if preserve_conflicts {
                preserve_losing_version(
                    &cache.conflicts_dir,
                    entity_id,
                    &losing_runtime,
                    &timestamp_now(),
                    &losing_contents,
                )?;
            }
            if winner == MergeSide::Incoming {
                current = incoming;
            }
        }

        merged_text = merge.merged;
    }
    let mut files = current.files.clone();
    replace_primary_file(
        current.entity_type,
        current,
        &mut files,
        merged_text.as_bytes().to_vec(),
    );

    Ok(CanonicalDecision {
        contents: merged_text.into_bytes(),
        files,
        pending_conflict_resolution,
    })
}

fn tiebreak_changed_views(
    cache: &CacheLayout,
    entity_id: &EntityId,
    changed: &[&EntityView],
    preserve_conflicts: bool,
) -> Result<CanonicalDecision> {
    let Some(winner) = changed.iter().max_by_key(|view| view.mtime).copied() else {
        return Ok(CanonicalDecision {
            contents: Vec::new(),
            files: BTreeMap::new(),
            pending_conflict_resolution: false,
        });
    };

    if preserve_conflicts {
        for loser in changed
            .iter()
            .copied()
            .filter(|view| !std::ptr::eq(*view, winner))
        {
            let runtime = runtime_for_location(&loser.location)?;
            preserve_losing_version(
                &cache.conflicts_dir,
                entity_id,
                &runtime,
                &timestamp_now(),
                &canonical_text(loser)?,
            )?;
        }
    }

    Ok(CanonicalDecision {
        contents: winner.canonical_contents.clone(),
        files: winner.files.clone(),
        pending_conflict_resolution: changed.len() > 1,
    })
}

fn replace_primary_file(
    entity_type: EntityType,
    view: &EntityView,
    files: &mut BTreeMap<PathBuf, Vec<u8>>,
    contents: Vec<u8>,
) {
    let key = primary_file_key(entity_type, files)
        .or_else(|| view.files.keys().next().cloned())
        .unwrap_or_else(|| PathBuf::from("SKILL.md"));
    files.insert(key, contents);
}

fn canonical_text(view: &EntityView) -> Result<String> {
    String::from_utf8(view.canonical_contents.clone()).map_err(|source| PipelineError::Io {
        action: "decode canonical entity",
        path: view.relative_path.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
    })
}

fn is_markdown_view(view: &EntityView) -> bool {
    is_markdown_path(&view.relative_path)
}

fn is_markdown_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("md" | "mdc")
    ) || path.file_name().and_then(|file_name| file_name.to_str()) == Some("SKILL.md")
}

fn runtime_for_location(location: &LocationKey) -> Result<RuntimeName> {
    RuntimeName::new(location.as_str().trim_start_matches('.')).map_err(Into::into)
}

fn slug_from_lockfile_path(entity_type: EntityType, lockfile_path: &Path) -> Option<String> {
    let relative_path = match entity_type {
        EntityType::Instructions if lockfile_path.starts_with("instructions") => {
            PathBuf::from(".ai").join(lockfile_path)
        }
        EntityType::Instructions => PathBuf::from("AGENTS.md"),
        EntityType::Skill => PathBuf::from(".ai").join(lockfile_path),
        EntityType::Rule => PathBuf::from(".ai").join(lockfile_path),
        EntityType::Prompt => PathBuf::from(".ai").join(lockfile_path),
        EntityType::Command => PathBuf::from(".ai").join(lockfile_path),
        EntityType::Hook => PathBuf::from(".ai").join(lockfile_path),
        EntityType::McpBinding => PathBuf::from(".ai").join(lockfile_path),
        EntityType::PermissionPolicy => PathBuf::from(".ai").join(lockfile_path),
        EntityType::Subagent if lockfile_path.starts_with("subagents") => {
            PathBuf::from(".ai").join(lockfile_path)
        }
        EntityType::Subagent => PathBuf::from(".claude").join(lockfile_path),
    };
    let entity_id = derive_entity_id(&relative_path).ok()?;
    Some(entity_slug(&entity_id).to_string())
}

#[derive(Debug, Clone)]
struct EntityCandidate {
    entity_type: EntityType,
    location_key: LocationKey,
    relative_path: PathBuf,
    lockfile_path: PathBuf,
}

struct EntityFileTreeScan<'a> {
    root: &'a Path,
    location_key: LocationKey,
    relative_root: &'a Path,
    lockfile_root: &'a Path,
    entity_type: EntityType,
    extensions: &'a [&'a str],
}

fn entity_candidates(repo_root: &Path) -> Result<Vec<EntityCandidate>> {
    let mut candidates = Vec::new();
    if is_regular_file_path(&repo_root.join("AGENTS.md"))? {
        candidates.push(EntityCandidate {
            entity_type: EntityType::Instructions,
            location_key: location_key(".ai")?,
            relative_path: PathBuf::from("AGENTS.md"),
            lockfile_path: PathBuf::from("../AGENTS.md"),
        });
    }

    scan_entity_file_tree(
        repo_root,
        ".ai",
        ".ai/instructions",
        "instructions",
        EntityType::Instructions,
        &["md"],
        &mut candidates,
    )?;
    scan_entity_file_tree(
        repo_root,
        ".ai",
        ".ai/rules",
        "rules",
        EntityType::Rule,
        &["md", "mdc"],
        &mut candidates,
    )?;
    scan_entity_file_tree(
        repo_root,
        ".ai",
        ".ai/prompts",
        "prompts",
        EntityType::Prompt,
        &["md"],
        &mut candidates,
    )?;
    scan_entity_file_tree(
        repo_root,
        ".ai",
        ".ai/commands",
        "commands",
        EntityType::Command,
        &["md", "json", "toml"],
        &mut candidates,
    )?;
    scan_entity_file_tree(
        repo_root,
        ".ai",
        ".ai/hooks",
        "hooks",
        EntityType::Hook,
        &["json", "toml", "yaml", "yml"],
        &mut candidates,
    )?;
    scan_entity_file_tree(
        repo_root,
        ".ai",
        ".ai/mcp-bindings",
        "mcp-bindings",
        EntityType::McpBinding,
        &["json", "toml", "yaml", "yml"],
        &mut candidates,
    )?;
    scan_entity_file_tree(
        repo_root,
        ".ai",
        ".ai/permission-policies",
        "permission-policies",
        EntityType::PermissionPolicy,
        &["json", "toml", "yaml", "yml"],
        &mut candidates,
    )?;
    scan_skill_dir(repo_root, ".ai", ".ai/skills", "skills", &mut candidates)?;
    scan_skill_dir(
        repo_root,
        ".agents",
        ".agents/skills",
        "skills",
        &mut candidates,
    )?;
    scan_subagent_dir(
        repo_root,
        ".ai",
        ".ai/subagents",
        "subagents",
        "md",
        &mut candidates,
    )?;

    Ok(candidates)
}

fn scan_entity_file_tree(
    repo_root: &Path,
    location: &str,
    relative_dir: &str,
    lockfile_root: &str,
    entity_type: EntityType,
    extensions: &[&str],
    candidates: &mut Vec<EntityCandidate>,
) -> Result<()> {
    let root = repo_root.join(relative_dir);
    if !is_regular_dir_path(&root)? {
        return Ok(());
    }
    let scan = EntityFileTreeScan {
        root: &root,
        location_key: location_key(location)?,
        relative_root: Path::new(relative_dir),
        lockfile_root: Path::new(lockfile_root),
        entity_type,
        extensions,
    };
    scan_entity_file_tree_inner(&scan, &root, candidates)
}

fn scan_entity_file_tree_inner(
    scan: &EntityFileTreeScan<'_>,
    dir: &Path,
    candidates: &mut Vec<EntityCandidate>,
) -> Result<()> {
    for entry in read_dir_sorted(dir)? {
        if is_regular_dir_path(&entry)? {
            scan_entity_file_tree_inner(scan, &entry, candidates)?;
            continue;
        }
        if !is_regular_file_path(&entry)? {
            continue;
        }
        let Some(extension) = entry.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        if !scan.extensions.contains(&extension) {
            continue;
        }
        let relative_file =
            entry
                .strip_prefix(scan.root)
                .map_err(|source| PipelineError::EntityFormat {
                    path: entry.clone(),
                    message: source.to_string(),
                })?;
        candidates.push(EntityCandidate {
            entity_type: scan.entity_type,
            location_key: scan.location_key.clone(),
            relative_path: scan.relative_root.join(relative_file),
            lockfile_path: scan.lockfile_root.join(relative_file),
        });
    }

    Ok(())
}

fn scan_skill_dir(
    repo_root: &Path,
    location: &str,
    relative_dir: &str,
    lockfile_root: &str,
    candidates: &mut Vec<EntityCandidate>,
) -> Result<()> {
    let dir = repo_root.join(relative_dir);
    if !is_regular_dir_path(&dir)? {
        return Ok(());
    }

    for entry in read_dir_sorted(&dir)? {
        if !is_regular_dir_path(&entry)? {
            continue;
        }
        let skill_md = entry.join("SKILL.md");
        if !is_regular_file_path(&skill_md)? {
            continue;
        }
        let Some(name) = entry.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        candidates.push(EntityCandidate {
            entity_type: EntityType::Skill,
            location_key: location_key(location)?,
            relative_path: PathBuf::from(relative_dir).join(name).join("SKILL.md"),
            lockfile_path: PathBuf::from(lockfile_root).join(name).join("SKILL.md"),
        });
    }

    Ok(())
}

fn scan_subagent_dir(
    repo_root: &Path,
    location: &str,
    relative_dir: &str,
    lockfile_root: &str,
    extension: &str,
    candidates: &mut Vec<EntityCandidate>,
) -> Result<()> {
    let dir = repo_root.join(relative_dir);
    if !is_regular_dir_path(&dir)? {
        return Ok(());
    }

    for entry in read_dir_sorted(&dir)? {
        if !is_regular_file_path(&entry)? {
            continue;
        }
        if entry.extension().and_then(|value| value.to_str()) != Some(extension) {
            continue;
        }
        let Some(file_name) = entry.file_name() else {
            continue;
        };
        candidates.push(EntityCandidate {
            entity_type: EntityType::Subagent,
            location_key: location_key(location)?,
            relative_path: PathBuf::from(relative_dir).join(file_name),
            lockfile_path: PathBuf::from(lockfile_root).join(file_name),
        });
    }

    Ok(())
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir).map_err(|source| PipelineError::Io {
        action: "read directory",
        path: dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| PipelineError::Io {
            action: "read directory entry",
            path: dir.to_path_buf(),
            source,
        })?;
        entries.push(entry.path());
    }
    entries.sort();
    Ok(entries)
}

fn is_regular_file_path(path: &Path) -> Result<bool> {
    Ok(safe_path_metadata(path)?.is_some_and(|metadata| metadata.is_file()))
}

fn is_regular_dir_path(path: &Path) -> Result<bool> {
    Ok(safe_path_metadata(path)?.is_some_and(|metadata| metadata.is_dir()))
}

fn safe_path_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read metadata",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(symlink_entity_error(path));
    }
    Ok(Some(metadata))
}

fn entity_files_for_candidate(
    repo_root: &Path,
    candidate: &EntityCandidate,
    primary_contents: Vec<u8>,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    match candidate.entity_type {
        EntityType::Skill => {
            let root = repo_root
                .join(&candidate.relative_path)
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| PipelineError::EntityFormat {
                    path: candidate.relative_path.clone(),
                    message: "skill path has no parent directory".to_string(),
                })?;
            let mut files = collect_entity_text_files(&root, &root)?;
            files.insert(PathBuf::from("SKILL.md"), primary_contents);
            Ok(files)
        }
        EntityType::Instructions
        | EntityType::Rule
        | EntityType::Prompt
        | EntityType::Command
        | EntityType::Hook
        | EntityType::McpBinding
        | EntityType::PermissionPolicy
        | EntityType::Subagent => {
            let key = candidate
                .relative_path
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| primary_entity_file_path(candidate.entity_type));
            Ok(BTreeMap::from([(key, primary_contents)]))
        }
    }
}

fn collect_entity_text_files(root: &Path, dir: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut total_bytes = 0;
    collect_entity_text_files_inner(root, dir, 0, &mut files, &mut total_bytes)?;
    Ok(files)
}

fn collect_entity_text_files_inner(
    root: &Path,
    dir: &Path,
    depth: usize,
    files: &mut BTreeMap<PathBuf, Vec<u8>>,
    total_bytes: &mut u64,
) -> Result<()> {
    if depth > MAX_ENTITY_TREE_DEPTH {
        return Err(entity_limit_error(
            dir,
            format!("entity directory depth exceeds {MAX_ENTITY_TREE_DEPTH}"),
        ));
    }

    let dir_metadata = fs::symlink_metadata(dir).map_err(|source| PipelineError::Io {
        action: "read metadata",
        path: dir.to_path_buf(),
        source,
    })?;
    if dir_metadata.file_type().is_symlink() {
        return Err(symlink_entity_error(dir));
    }

    for path in read_dir_sorted(dir)? {
        let metadata = fs::symlink_metadata(&path).map_err(|source| PipelineError::Io {
            action: "read metadata",
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(symlink_entity_error(&path));
        }
        if metadata.is_dir() {
            collect_entity_text_files_inner(root, &path, depth + 1, files, total_bytes)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if files.len() >= MAX_ENTITY_FILE_COUNT {
            return Err(entity_limit_error(
                &path,
                format!("entity file count exceeds {MAX_ENTITY_FILE_COUNT}"),
            ));
        }
        let projected_bytes = total_bytes
            .checked_add(metadata.len())
            .ok_or_else(|| entity_limit_error(&path, "entity byte count overflowed"))?;
        if projected_bytes > MAX_ENTITY_TOTAL_BYTES {
            return Err(entity_limit_error(
                &path,
                format!("entity byte size exceeds {MAX_ENTITY_TOTAL_BYTES}"),
            ));
        }
        let relative = path
            .strip_prefix(root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.clone());
        let contents = fs::read(&path).map_err(|source| PipelineError::Io {
            action: "read file",
            path: path.clone(),
            source,
        })?;
        *total_bytes = total_bytes
            .checked_add(u64::try_from(contents.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| entity_limit_error(&path, "entity byte count overflowed"))?;
        if *total_bytes > MAX_ENTITY_TOTAL_BYTES {
            return Err(entity_limit_error(
                &path,
                format!("entity byte size exceeds {MAX_ENTITY_TOTAL_BYTES}"),
            ));
        }
        files.insert(relative, contents);
    }
    Ok(())
}

fn symlink_entity_error(path: &Path) -> PipelineError {
    PipelineError::EntityFormat {
        path: path.to_path_buf(),
        message: "symlinked entity path is not supported".to_string(),
    }
}

fn entity_limit_error(path: &Path, message: impl Into<String>) -> PipelineError {
    PipelineError::EntityFormat {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn canonicalize_for_candidate(
    entity_type: EntityType,
    relative_path: &Path,
    contents: &str,
) -> Result<Vec<u8>> {
    match entity_type {
        EntityType::Instructions => Ok(contents.as_bytes().to_vec()),
        EntityType::Skill | EntityType::Rule | EntityType::Prompt => {
            Ok(crate::merge::canonicalize_markdown(contents)?.into_bytes())
        }
        EntityType::Command if is_markdown_path(relative_path) => {
            Ok(crate::merge::canonicalize_markdown(contents)?.into_bytes())
        }
        EntityType::Command
        | EntityType::Hook
        | EntityType::McpBinding
        | EntityType::PermissionPolicy => Ok(contents.as_bytes().to_vec()),
        EntityType::Subagent
            if relative_path.extension().and_then(|value| value.to_str()) == Some("toml") =>
        {
            Ok(canonicalize_codex_toml_subagent(contents, relative_path)?.into_bytes())
        }
        EntityType::Subagent => Ok(crate::merge::canonicalize_markdown(contents)?.into_bytes()),
    }
}

fn canonicalize_codex_toml_subagent(contents: &str, relative_path: &Path) -> Result<String> {
    let value = contents
        .parse::<toml::Value>()
        .map_err(|source| PipelineError::EntityFormat {
            path: relative_path.to_path_buf(),
            message: source.to_string(),
        })?;
    let Some(table) = value.as_table() else {
        return Err(PipelineError::EntityFormat {
            path: relative_path.to_path_buf(),
            message: "TOML root must be a table".to_string(),
        });
    };

    let mut frontmatter = serde_norway::Mapping::new();
    let mut body = String::new();
    for (key, value) in table {
        if (key == "instructions" || key == "prompt") && body.is_empty() {
            if let Some(text) = value.as_str() {
                body.push_str(text);
                if !body.ends_with('\n') {
                    body.push('\n');
                }
            }
            continue;
        }
        let yaml_value =
            serde_norway::to_value(value).map_err(|source| PipelineError::EntityFormat {
                path: relative_path.to_path_buf(),
                message: source.to_string(),
            })?;
        frontmatter.insert(serde_norway::Value::String(key.clone()), yaml_value);
    }

    let rendered =
        serde_norway::to_string(&serde_norway::Value::Mapping(frontmatter)).map_err(|source| {
            PipelineError::EntityFormat {
                path: relative_path.to_path_buf(),
                message: source.to_string(),
            }
        })?;
    let rendered = rendered.strip_prefix("---\n").unwrap_or(&rendered);
    let rendered = rendered.strip_suffix("...\n").unwrap_or(rendered);
    Ok(format!("---\n{rendered}---\n{body}"))
}

fn split_canonical_markdown(contents: &str) -> Result<(serde_norway::Mapping, String)> {
    let Some(rest) = contents.strip_prefix("---\n") else {
        return Ok((serde_norway::Mapping::new(), contents.to_string()));
    };
    let Some(end) = rest.find("\n---\n") else {
        return Ok((serde_norway::Mapping::new(), contents.to_string()));
    };
    let frontmatter = &rest[..end];
    let body = rest[end + "\n---\n".len()..].to_string();
    let mapping = crate::merge::parse_frontmatter_mapping(frontmatter).map_err(|source| {
        PipelineError::EntityFormat {
            path: PathBuf::from("<canonical>"),
            message: source.to_string(),
        }
    })?;
    Ok((mapping, body))
}

fn choose_canonical_view(views: &BTreeMap<LocationKey, EntityView>) -> Option<&EntityView> {
    for key in [".ai", ".agents", ".claude", ".codex", ".copilot", ".cursor"] {
        let key = match location_key(key) {
            Ok(key) => key,
            Err(_) => continue,
        };
        if let Some(view) = views.get(&key) {
            return Some(view);
        }
    }
    views.values().next()
}

fn planned_writes(
    repo_root: &Path,
    _markers: &RuntimeMarkers,
    slug: &str,
    entity_type: EntityType,
    canonical_files: BTreeMap<PathBuf, Vec<u8>>,
) -> Result<Vec<PlannedWrite>> {
    let mut writes = Vec::new();
    match entity_type {
        EntityType::Instructions
        | EntityType::Rule
        | EntityType::Prompt
        | EntityType::Command
        | EntityType::Hook
        | EntityType::McpBinding
        | EntityType::PermissionPolicy
        | EntityType::Subagent => {
            let (entity_file_path, contents) =
                canonical_files.into_iter().next().unwrap_or_else(|| {
                    (
                        primary_entity_file_path_for_slug(entity_type, slug),
                        Vec::new(),
                    )
                });
            writes.push(planned_write(
                repo_root,
                ".ai",
                canonical_lockfile_path_for_file(entity_type, slug, &entity_file_path),
                canonical_repo_path_for_file(entity_type, slug, &entity_file_path),
                entity_file_path,
                contents,
                true,
            )?);
        }
        EntityType::Skill => {
            for (file_path, contents) in canonical_files {
                let primary = file_path == Path::new("SKILL.md");
                writes.push(planned_write(
                    repo_root,
                    ".ai",
                    PathBuf::from("skills").join(slug).join(&file_path),
                    PathBuf::from(".ai")
                        .join("skills")
                        .join(slug)
                        .join(&file_path),
                    file_path,
                    contents,
                    primary,
                )?);
            }
        }
    }

    Ok(writes)
}

fn planned_write(
    repo_root: &Path,
    location: &str,
    lockfile_path: PathBuf,
    repo_path: PathBuf,
    entity_file_path: PathBuf,
    contents: Vec<u8>,
    primary: bool,
) -> Result<PlannedWrite> {
    Ok(PlannedWrite {
        location: location_key(location)?,
        lockfile_path,
        entity_file_path,
        absolute_path: repo_root.join(repo_path),
        contents,
        primary,
    })
}

fn primary_entity_file_path(entity_type: EntityType) -> PathBuf {
    match entity_type {
        EntityType::Instructions => PathBuf::from("AGENTS.md"),
        EntityType::Rule => PathBuf::from("rule.md"),
        EntityType::Prompt => PathBuf::from("prompt.md"),
        EntityType::Command => PathBuf::from("command.md"),
        EntityType::Hook => PathBuf::from("hook.json"),
        EntityType::McpBinding => PathBuf::from("mcp-binding.json"),
        EntityType::PermissionPolicy => PathBuf::from("permission-policy.json"),
        EntityType::Skill => PathBuf::from("SKILL.md"),
        EntityType::Subagent => PathBuf::from("subagent.md"),
    }
}

fn primary_entity_file_path_for_slug(entity_type: EntityType, slug: &str) -> PathBuf {
    match entity_type {
        EntityType::Instructions if slug != "root" => scoped_instruction_file_name(slug),
        EntityType::Rule => PathBuf::from(format!("{slug}.md")),
        EntityType::Prompt => PathBuf::from(format!("{slug}.md")),
        EntityType::Command => command_file_name(slug, "md"),
        EntityType::Hook => PathBuf::from(format!("{slug}.json")),
        EntityType::McpBinding => PathBuf::from(format!("{slug}.json")),
        EntityType::PermissionPolicy => PathBuf::from(format!("{slug}.json")),
        _ => primary_entity_file_path(entity_type),
    }
}

fn planned_file_changes(plan: &SyncPlan) -> Result<BTreeSet<EntityId>> {
    let mut changed = BTreeSet::new();
    for entity in &plan.entities {
        for write in &entity.writes {
            if needs_write(&write.absolute_path, &write.contents)? {
                changed.insert(entity.id.clone());
            }
        }
    }
    Ok(changed)
}

fn planned_location_hashes(
    entity_type: EntityType,
    writes: &[PlannedWrite],
) -> Result<BTreeMap<LocationKey, Hash>> {
    let mut by_location: BTreeMap<LocationKey, BTreeMap<PathBuf, Vec<u8>>> = BTreeMap::new();
    for write in writes {
        by_location
            .entry(write.location.clone())
            .or_default()
            .insert(write.entity_file_path.clone(), write.contents.clone());
    }

    let mut hashes = BTreeMap::new();
    for (location, files) in by_location {
        let primary = primary_file_contents(entity_type, &files).unwrap_or_default();
        hashes.insert(
            location,
            hash_entity_payload(entity_type, &files, &primary)?,
        );
    }
    Ok(hashes)
}

fn lockfile_entity_changes(previous: &Lockfile, current: &Lockfile) -> BTreeSet<EntityId> {
    current
        .entities
        .iter()
        .filter(|(entity_id, entity)| previous.entities.get(entity_id) != Some(*entity))
        .map(|(entity_id, _)| entity_id.clone())
        .collect()
}

fn entity_has_change(entity: &EntityPlan, changed: &BTreeSet<EntityId>) -> bool {
    changed.contains(&entity.id)
}

fn needs_write(path: &Path, contents: &[u8]) -> Result<bool> {
    match fs::read(path) {
        Ok(existing) => Ok(existing != contents),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(source) => Err(PipelineError::Io {
            action: "read file",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn enqueue_changed_entities(
    repo_root: &Path,
    queue: &PendingQueue,
    plan: &SyncPlan,
    changed: &BTreeSet<EntityId>,
    opts: &SyncOptions,
) -> Result<usize> {
    let mut count = 0;
    for entity in &plan.entities {
        if !changed.contains(&entity.id) {
            continue;
        }
        let Some(first_write) = entity
            .writes
            .iter()
            .find(|write| write.primary)
            .or_else(|| entity.writes.first())
        else {
            continue;
        };
        let changed_paths = entity
            .writes
            .iter()
            .map(|write| relative_to(repo_root, &write.absolute_path))
            .collect::<Vec<_>>();
        let mut content_hashes = BTreeMap::new();
        for write in &entity.writes {
            content_hashes.insert(
                relative_to(repo_root, &write.absolute_path),
                sha256_bytes(&write.contents)?,
            );
        }
        let record = PendingSyncRecord {
            pending_id: PendingQueue::new_pending_id(),
            source_runtime: source_runtime_for_trigger(opts)?,
            action: PendingAction::Write,
            entity_type: Some(entity.entity_type),
            entity_root: pending_entity_root(repo_root, entity, first_write),
            changed_paths,
            rename_from: None,
            content_hashes,
            mtime: timestamp_now(),
            trigger: opts
                .trigger
                .clone()
                .unwrap_or_else(|| "core-sync".to_string()),
            created_at: timestamp_now(),
            attempts: 0,
            last_error: None,
        };
        queue.enqueue(&record)?;
        count += 1;
    }

    Ok(count)
}

fn pending_entity_root(
    repo_root: &Path,
    entity: &EntityPlan,
    primary_write: &PlannedWrite,
) -> PathBuf {
    if entity.entity_type == EntityType::Skill {
        return primary_write
            .absolute_path
            .parent()
            .map(|path| relative_to(repo_root, path))
            .unwrap_or_else(|| relative_to(repo_root, &primary_write.absolute_path));
    }
    relative_to(repo_root, &primary_write.absolute_path)
}

fn source_runtime_for_trigger(opts: &SyncOptions) -> Result<RuntimeName> {
    match opts.trigger.as_deref() {
        Some("claude-hook") => runtime_name("claude"),
        Some("codex-hook") => runtime_name("codex"),
        _ => runtime_name("core"),
    }
}

fn drain_pending_records(
    repo_root: &Path,
    cache: &CacheLayout,
    queue: &PendingQueue,
    adapters: &dyn AdapterRegistry,
) -> Result<crate::drainer::DrainSummary> {
    let worker = AgentmeshMutex::new(&cache.worker_lock);
    drain_pending(queue, &worker, &mut |record: &PendingSyncRecord| {
        process_pending_record(repo_root, cache, record, adapters)
    })
    .map_err(Into::into)
}

fn process_pending_record(
    repo_root: &Path,
    cache: &CacheLayout,
    record: &PendingSyncRecord,
    adapters: &dyn AdapterRegistry,
) -> crate::drainer::ProcessResult {
    process_pending_record_inner(repo_root, cache, record, adapters)
        .map_err(|source| DrainerProcessError::new(source.to_string()))
}

fn process_pending_record_inner(
    repo_root: &Path,
    cache: &CacheLayout,
    record: &PendingSyncRecord,
    adapters: &dyn AdapterRegistry,
) -> Result<()> {
    validate_pending_record(repo_root, record)?;
    let mut lockfile = read_lockfile(repo_root)?;
    let config = load_config(repo_root)?.config;
    let entity_ids = affected_entity_ids(repo_root, &lockfile, record);
    if entity_ids.is_empty() {
        return Err(PipelineError::EntityFormat {
            path: record.entity_root.clone(),
            message: "pending record did not match a lockfile entity".to_string(),
        });
    }

    for entity_id in entity_ids {
        let targets = emit_targets(&lockfile, record, &entity_id)?;
        for runtime in targets {
            emit_entity_to_runtime(
                repo_root,
                &mut lockfile,
                &config,
                &entity_id,
                &runtime,
                adapters,
            )?;
        }
    }

    let state_mutex = AgentmeshMutex::new(&cache.state_lock);
    let _state_guard = state_mutex.acquire()?;
    write_lockfile(repo_root, &lockfile)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct EmitOutcome {
    files_written: usize,
    capability_skipped: usize,
}

fn affected_entity_ids(
    repo_root: &Path,
    lockfile: &Lockfile,
    record: &PendingSyncRecord,
) -> BTreeSet<EntityId> {
    lockfile
        .entities
        .iter()
        .filter(|(_, entity)| {
            if let Some(record_type) = record.entity_type {
                if entity.entity_type != record_type {
                    return false;
                }
            }
            entity.locations.iter().any(|(location, lockfile_path)| {
                let Ok(absolute_path) = path_from_lockfile(repo_root, location, lockfile_path)
                else {
                    return false;
                };
                let relative = relative_to(repo_root, &absolute_path);
                let root = entity_root_from_location(entity.entity_type, &relative);
                record.changed_paths.iter().any(|changed| {
                    changed == &relative
                        || changed.starts_with(&relative)
                        || relative.starts_with(changed)
                        || changed == &root
                        || changed.starts_with(&root)
                        || root.starts_with(changed)
                })
            })
        })
        .map(|(entity_id, _)| entity_id.clone())
        .collect()
}

fn entity_root_from_location(entity_type: EntityType, path: &Path) -> PathBuf {
    if entity_type == EntityType::Skill {
        return path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf());
    }
    path.to_path_buf()
}

fn emit_targets(
    lockfile: &Lockfile,
    record: &PendingSyncRecord,
    entity_id: &EntityId,
) -> Result<Vec<RuntimeName>> {
    if !lockfile.entities.contains_key(entity_id) {
        return Err(PipelineError::EntityNotFound {
            entity_id: entity_id.clone(),
        });
    }
    let mut targets = Vec::new();
    for runtime in lockfile.adapters.keys() {
        if record.source_runtime.as_str() != "core" && runtime == &record.source_runtime {
            continue;
        }
        targets.push(runtime.clone());
    }
    Ok(targets)
}

fn emit_entity_to_runtime(
    repo_root: &Path,
    lockfile: &mut Lockfile,
    config: &AgentmeshConfig,
    entity_id: &EntityId,
    runtime: &RuntimeName,
    adapters: &dyn AdapterRegistry,
) -> Result<EmitOutcome> {
    let Some(entity) = lockfile.entities.get(entity_id) else {
        return Err(PipelineError::EntityNotFound {
            entity_id: entity_id.clone(),
        });
    };
    let Some(adapter) = lockfile.adapters.get(runtime) else {
        return Ok(EmitOutcome::default());
    };
    if !adapter.entities.contains(&entity.entity_type) {
        return match capability_fallback(config, runtime, entity.entity_type) {
            CapabilityFallback::Skip => Ok(EmitOutcome::default()),
            CapabilityFallback::Warn => Ok(EmitOutcome {
                files_written: 0,
                capability_skipped: 1,
            }),
            CapabilityFallback::RenderAsDoc => Err(PipelineError::RenderAsDocUnsupported {
                runtime: runtime.clone(),
                entity_id: entity_id.clone(),
                entity_type: entity.entity_type,
            }),
            CapabilityFallback::Fail => Err(PipelineError::CapabilityMismatch {
                runtime: runtime.clone(),
                entity_id: entity_id.clone(),
                entity_type: entity.entity_type,
            }),
        };
    }
    let Some(mode) = protocol_runtime_mode(config, runtime) else {
        return Ok(EmitOutcome::default());
    };

    let request = EmitRequest {
        runtime_dir: runtime_dir(repo_root, runtime),
        entities: vec![build_emit_entity(repo_root, lockfile, entity_id, runtime)?],
        mode,
    };
    let response = adapters.emit(runtime, repo_root, request)?;
    let files_written = response.files_written.len();
    update_emitted_native_hashes(
        repo_root,
        lockfile,
        entity_id,
        runtime,
        &response.files_written,
    )?;
    Ok(EmitOutcome {
        files_written,
        capability_skipped: 0,
    })
}

fn protocol_runtime_mode(
    config: &AgentmeshConfig,
    runtime: &RuntimeName,
) -> Option<ProtocolRuntimeMode> {
    match config
        .runtimes
        .get(runtime)
        .map(|runtime_config| runtime_config.mode)
        .unwrap_or(ConfigRuntimeMode::Bidirectional)
    {
        ConfigRuntimeMode::Bidirectional => Some(ProtocolRuntimeMode::Bidirectional),
        ConfigRuntimeMode::Merge => Some(ProtocolRuntimeMode::Merge),
        ConfigRuntimeMode::ReadOnly => Some(ProtocolRuntimeMode::ReadOnly),
        ConfigRuntimeMode::Managed => Some(ProtocolRuntimeMode::Managed),
        ConfigRuntimeMode::Disabled => None,
    }
}

fn build_emit_entity(
    repo_root: &Path,
    lockfile: &Lockfile,
    entity_id: &EntityId,
    runtime: &RuntimeName,
) -> Result<EmitEntity> {
    let Some(entity) = lockfile.entities.get(entity_id) else {
        return Err(PipelineError::EntityNotFound {
            entity_id: entity_id.clone(),
        });
    };
    let canonical_location = location_key(".ai")?;
    let Some(canonical_path) = entity.locations.get(&canonical_location) else {
        return Err(PipelineError::MissingCanonicalLocation {
            entity_id: entity_id.clone(),
        });
    };
    let absolute_path = path_from_lockfile(repo_root, &canonical_location, canonical_path)?;
    let files = canonical_entity_files_for_emit(repo_root, entity.entity_type, &absolute_path)?;
    let primary_key = primary_file_key(entity.entity_type, &files)
        .unwrap_or_else(|| emit_file_key(entity.entity_type, canonical_path));
    let contents = files
        .get(&primary_key)
        .cloned()
        .ok_or_else(|| PipelineError::EntityFormat {
            path: absolute_path.clone(),
            message: "canonical entity has no primary file".to_string(),
        })?;
    let contents = String::from_utf8(contents).map_err(|source| PipelineError::EntityFormat {
        path: absolute_path.clone(),
        message: source.to_string(),
    })?;
    let (frontmatter, _) = split_canonical_markdown(&contents)?;
    let mut emit_files = BTreeMap::new();
    for (path, contents) in files {
        emit_files.insert(path, EntityFile::from_bytes(contents));
    }
    let overrides = lockfile
        .overrides
        .get(entity_id)
        .and_then(|runtime_overrides| runtime_overrides.get(runtime))
        .map(|entry| entry.0.clone())
        .unwrap_or_default();
    let runtime_location = location_key(&format!(".{}", runtime.as_str()))?;
    let source_path = entity
        .locations
        .get(&runtime_location)
        .map(|path| workspace_path_from_lockfile(&runtime_location, path))
        .transpose()?
        .or_else(|| {
            entity
                .locations
                .get(&location_key(".agents").ok()?)
                .filter(|_| entity.entity_type == EntityType::Skill)
                .map(|path| PathBuf::from(".agents").join(path))
        });

    Ok(EmitEntity {
        id: entity_id.as_str().to_string(),
        entity_type: entity.entity_type,
        scope: entity.scope.clone(),
        source_path,
        files: emit_files,
        frontmatter: yaml_frontmatter_to_json_map(frontmatter)?,
        overrides,
    })
}

fn canonical_entity_files_for_emit(
    repo_root: &Path,
    entity_type: EntityType,
    primary_path: &Path,
) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    if entity_type == EntityType::Skill {
        let root = primary_path
            .parent()
            .ok_or_else(|| PipelineError::EntityFormat {
                path: primary_path.to_path_buf(),
                message: "skill path has no parent directory".to_string(),
            })?;
        return collect_entity_text_files(root, root);
    }
    let contents = fs::read(primary_path).map_err(|source| PipelineError::Io {
        action: "read file",
        path: primary_path.to_path_buf(),
        source,
    })?;
    let key = primary_path
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| primary_entity_file_path(entity_type));
    let _ = repo_root;
    Ok(BTreeMap::from([(key, contents)]))
}

fn emit_file_key(entity_type: EntityType, canonical_path: &Path) -> PathBuf {
    match entity_type {
        EntityType::Instructions => PathBuf::from("AGENTS.md"),
        EntityType::Skill => PathBuf::from("SKILL.md"),
        EntityType::Rule
        | EntityType::Prompt
        | EntityType::Command
        | EntityType::Hook
        | EntityType::McpBinding
        | EntityType::PermissionPolicy
        | EntityType::Subagent => canonical_path
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| primary_entity_file_path(entity_type)),
    }
}

fn yaml_frontmatter_to_json_map(
    frontmatter: serde_norway::Mapping,
) -> Result<BTreeMap<String, serde_json::Value>> {
    let value =
        serde_json::to_value(serde_norway::Value::Mapping(frontmatter)).map_err(|source| {
            PipelineError::EntityFormat {
                path: PathBuf::from("<canonical>"),
                message: source.to_string(),
            }
        })?;
    let Some(object) = value.as_object() else {
        return Ok(BTreeMap::new());
    };
    Ok(object
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect())
}

fn update_emitted_native_hashes(
    repo_root: &Path,
    lockfile: &mut Lockfile,
    entity_id: &EntityId,
    runtime: &RuntimeName,
    files_written: &[PathBuf],
) -> Result<()> {
    if files_written.is_empty() {
        return Ok(());
    }
    let entity_type = lockfile
        .entities
        .get(entity_id)
        .map(|entity| entity.entity_type)
        .ok_or_else(|| PipelineError::EntityNotFound {
            entity_id: entity_id.clone(),
        })?;
    let primary_path = primary_written_path(entity_type, files_written)
        .cloned()
        .unwrap_or_else(|| files_written[0].clone());
    let location = if primary_path.starts_with(".agents") {
        location_key(".agents")?
    } else {
        location_key(&format!(".{}", runtime.as_str()))?
    };
    let hash = emitted_runtime_hash(repo_root, entity_type, &primary_path)?;
    if !lockfile.entities.contains_key(entity_id) {
        return Err(PipelineError::EntityNotFound {
            entity_id: entity_id.clone(),
        });
    }
    let updated_path = if location.as_str() == ".agents" {
        primary_path
            .strip_prefix(".agents")
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| primary_path.clone())
    } else {
        lockfile_path_from_workspace(runtime, &primary_path)
    };
    for sibling in lockfile.entities.values_mut() {
        if sibling
            .locations
            .get(&location)
            .is_some_and(|path| path == &updated_path)
        {
            sibling
                .emitted_native_sha256
                .insert(location.clone(), hash.clone());
        }
    }
    let Some(entity) = lockfile.entities.get_mut(entity_id) else {
        return Err(PipelineError::EntityNotFound {
            entity_id: entity_id.clone(),
        });
    };
    entity.locations.insert(location.clone(), updated_path);
    entity.emitted_native_sha256.insert(location, hash);
    Ok(())
}

fn primary_written_path(entity_type: EntityType, files_written: &[PathBuf]) -> Option<&PathBuf> {
    match entity_type {
        EntityType::Instructions => files_written.iter().find(|path| {
            path.file_name().and_then(|name| name.to_str()) == Some("AGENTS.md")
                || path.file_name().and_then(|name| name.to_str()) == Some("CLAUDE.md")
        }),
        EntityType::Skill => files_written
            .iter()
            .find(|path| path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md")),
        EntityType::Rule
        | EntityType::Prompt
        | EntityType::Command
        | EntityType::Hook
        | EntityType::McpBinding
        | EntityType::PermissionPolicy
        | EntityType::Subagent => files_written.first(),
    }
}

fn emitted_runtime_hash(
    repo_root: &Path,
    entity_type: EntityType,
    primary_path: &Path,
) -> Result<Hash> {
    if entity_type != EntityType::Skill {
        return sha256_file(&repo_root.join(primary_path)).map_err(Into::into);
    }
    let primary = repo_root.join(primary_path);
    let Some(root) = primary.parent() else {
        return Err(PipelineError::EntityFormat {
            path: primary_path.to_path_buf(),
            message: "skill path has no parent directory".to_string(),
        });
    };
    let files = collect_entity_text_files(root, root)?;
    hash_entity_files(&files)
}

fn lockfile_path_from_workspace(runtime: &RuntimeName, relative_path: &Path) -> PathBuf {
    let runtime_root = format!(".{}", runtime.as_str());
    if let Ok(stripped) = relative_path.strip_prefix(&runtime_root) {
        return stripped.to_path_buf();
    }
    PathBuf::from("..").join(relative_path)
}

fn workspace_path_from_lockfile(location: &LocationKey, lockfile_path: &Path) -> Result<PathBuf> {
    if let Some(root_relative) = repo_relative_lockfile_path(location, lockfile_path)? {
        return Ok(root_relative);
    }
    Ok(PathBuf::from(location.as_str()).join(lockfile_path))
}

fn call_adapter_subprocess<T, P>(
    repo_root: &Path,
    runtime: &RuntimeName,
    method: &str,
    params: P,
) -> Result<T>
where
    T: DeserializeOwned,
    P: Serialize,
{
    let current_exe = std::env::current_exe().map_err(|source| PipelineError::Io {
        action: "resolve current executable",
        path: repo_root.to_path_buf(),
        source,
    })?;
    let mut child = Command::new(current_exe)
        .arg("__adapter")
        .arg(runtime.as_str())
        .arg("--stdio")
        .current_dir(repo_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| PipelineError::Io {
            action: "spawn adapter subprocess",
            path: repo_root.to_path_buf(),
            source,
        })?;
    let mut stdin = child.stdin.take().ok_or_else(|| PipelineError::Adapter {
        runtime: runtime.clone(),
        message: "adapter stdin was not available".to_string(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| PipelineError::Adapter {
        runtime: runtime.clone(),
        message: "adapter stdout was not available".to_string(),
    })?;
    let mut reader = BufReader::new(stdout);

    let _: agentmesh_protocol::InitializeResponse = adapter_rpc_call(
        runtime,
        &mut stdin,
        &mut reader,
        1,
        "initialize",
        InitializeRequest {
            workspace_root: repo_root.to_path_buf(),
            protocol_version: PROTOCOL_VERSION,
            config: None,
        },
    )?;
    let response = adapter_rpc_call(runtime, &mut stdin, &mut reader, 2, method, params)?;
    let _: OkResponse = adapter_rpc_call(
        runtime,
        &mut stdin,
        &mut reader,
        3,
        "shutdown",
        serde_json::json!({}),
    )?;
    drop(stdin);

    let status = child.wait().map_err(|source| PipelineError::Io {
        action: "wait for adapter subprocess",
        path: repo_root.to_path_buf(),
        source,
    })?;
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut stream) = child.stderr.take() {
            let _ = stream.read_to_string(&mut stderr);
        }
        return Err(PipelineError::Adapter {
            runtime: runtime.clone(),
            message: if stderr.trim().is_empty() {
                format!("adapter subprocess exited with {status}")
            } else {
                stderr
            },
        });
    }

    Ok(response)
}

fn adapter_rpc_call<T, P>(
    runtime: &RuntimeName,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    id: i64,
    method: &str,
    params: P,
) -> Result<T>
where
    T: DeserializeOwned,
    P: Serialize,
{
    let request = JsonRpcRequest::new(id, method, params)?;
    write_json_frame(writer, &request)?;
    let response = read_json_frame::<JsonRpcResponse>(reader)?;
    if let Some(error) = response.error {
        return Err(PipelineError::Adapter {
            runtime: runtime.clone(),
            message: error.message,
        });
    }
    let Some(result) = response.result else {
        return Err(PipelineError::Adapter {
            runtime: runtime.clone(),
            message: "adapter response did not contain a result".to_string(),
        });
    };
    serde_json::from_value(result).map_err(|source| PipelineError::Adapter {
        runtime: runtime.clone(),
        message: source.to_string(),
    })
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
struct BundledAdapterRegistry;

#[cfg(test)]
impl AdapterRegistry for BundledAdapterRegistry {
    fn detect(&self, runtime: &RuntimeName, repo_root: &Path) -> Result<DetectResponse> {
        with_bundled_adapter(runtime, |adapter| adapter.detect(repo_root))
    }

    fn import(
        &self,
        runtime: &RuntimeName,
        _repo_root: &Path,
        request: ImportRequest,
    ) -> Result<ImportResponse> {
        with_bundled_adapter(runtime, |adapter| adapter.import(request))
    }

    fn emit(
        &self,
        runtime: &RuntimeName,
        _repo_root: &Path,
        request: EmitRequest,
    ) -> Result<EmitResponse> {
        with_bundled_adapter(runtime, |adapter| adapter.emit(request))
    }
}

#[cfg(test)]
fn with_bundled_adapter<T>(
    runtime: &RuntimeName,
    call: impl FnOnce(&mut dyn Adapter) -> std::result::Result<T, AdapterError>,
) -> Result<T> {
    match runtime.as_str() {
        "claude" => {
            let mut adapter = agentmesh_adapter_claude::ClaudeAdapter;
            call(&mut adapter).map_err(|source| adapter_error(runtime, source))
        }
        "codex" => {
            let mut adapter = agentmesh_adapter_codex::CodexAdapter;
            call(&mut adapter).map_err(|source| adapter_error(runtime, source))
        }
        "copilot" => {
            let mut adapter = agentmesh_adapter_copilot::CopilotAdapter;
            call(&mut adapter).map_err(|source| adapter_error(runtime, source))
        }
        "cursor" => {
            let mut adapter = agentmesh_adapter_cursor::CursorAdapter;
            call(&mut adapter).map_err(|source| adapter_error(runtime, source))
        }
        "gemini" => {
            let mut adapter = agentmesh_adapter_gemini::GeminiAdapter;
            call(&mut adapter).map_err(|source| adapter_error(runtime, source))
        }
        _ => Err(PipelineError::Adapter {
            runtime: runtime.clone(),
            message: "unknown bundled adapter".to_string(),
        }),
    }
}

#[cfg(test)]
fn adapter_error(runtime: &RuntimeName, source: AdapterError) -> PipelineError {
    PipelineError::Adapter {
        runtime: runtime.clone(),
        message: source.to_string(),
    }
}

fn validate_pending_record(repo_root: &Path, record: &PendingSyncRecord) -> Result<()> {
    for (relative_path, expected_hash) in &record.content_hashes {
        let actual_hash = sha256_file(&repo_root.join(relative_path))?;
        if &actual_hash != expected_hash {
            return Err(PipelineError::EntityFormat {
                path: relative_path.clone(),
                message: "pending content hash mismatch".to_string(),
            });
        }
    }

    if record.content_hashes.is_empty() {
        for relative_path in &record.changed_paths {
            if !repo_root.join(relative_path).exists() {
                return Err(PipelineError::EntityFormat {
                    path: relative_path.clone(),
                    message: "pending path is not present".to_string(),
                });
            }
        }
    }

    Ok(())
}

fn restore_with_cache(
    repo_root: &Path,
    cache: &CacheLayout,
    entity_id: &EntityId,
    from: RuntimeName,
    at: Option<&str>,
    dry_run: bool,
    adapters: &dyn AdapterRegistry,
) -> Result<RestoreSummary> {
    let mut lockfile = read_lockfile(repo_root)?;
    let Some(entity) = lockfile.entities.get(entity_id) else {
        return Err(PipelineError::EntityNotFound {
            entity_id: entity_id.clone(),
        });
    };
    let entity_type = entity.entity_type;
    let Some(canonical_path) = entity.locations.get(&location_key(".ai")?) else {
        return Err(PipelineError::MissingCanonicalLocation {
            entity_id: entity_id.clone(),
        });
    };
    let canonical_path = canonical_path.clone();
    let preserved = selected_preserved_version(cache, entity_id, &from, at)?;
    let contents = fs::read(&preserved).map_err(|source| PipelineError::Io {
        action: "read file",
        path: preserved.clone(),
        source,
    })?;
    let absolute_path = path_from_lockfile(repo_root, &location_key(".ai")?, &canonical_path)?;
    if dry_run {
        return Ok(RestoreSummary {
            changed: false,
            preserved_version: preserved,
            files_written: 0,
        });
    }
    write_atomic(&absolute_path, &contents)?;
    let contents_hash = entity_location_hash(
        repo_root,
        entity_type,
        &location_key(".ai")?,
        &canonical_path,
    )?
    .ok_or_else(|| PipelineError::EntityFormat {
        path: canonical_path.clone(),
        message: "restored canonical entity was not readable".to_string(),
    })?;
    let canonical_location = location_key(".ai")?;
    let Some(entity) = lockfile.entities.get_mut(entity_id) else {
        return Err(PipelineError::EntityNotFound {
            entity_id: entity_id.clone(),
        });
    };
    entity.canonical_sha256 = contents_hash.clone();
    entity
        .emitted_native_sha256
        .insert(canonical_location, contents_hash);
    entity.pending_conflict_resolution = None;

    let config = load_config(repo_root)?.config;
    let targets = lockfile.adapters.keys().cloned().collect::<Vec<_>>();
    let mut files_written = 1;
    for runtime in targets {
        let outcome = emit_entity_to_runtime(
            repo_root,
            &mut lockfile,
            &config,
            entity_id,
            &runtime,
            adapters,
        )?;
        files_written += outcome.files_written;
    }

    write_lockfile(repo_root, &lockfile)?;
    Ok(RestoreSummary {
        changed: true,
        preserved_version: preserved,
        files_written,
    })
}

fn selected_preserved_version(
    cache: &CacheLayout,
    entity_id: &EntityId,
    runtime: &RuntimeName,
    at: Option<&str>,
) -> Result<PathBuf> {
    if let Some(timestamp) = at {
        let dir = conflict_entity_dir(&cache.conflicts_dir, entity_id);
        let path = dir.join(conflict_version_file_name(runtime, timestamp));
        if path.is_file() {
            return Ok(path);
        }
        let legacy_path = dir.join(format!("{}-{timestamp}.md", runtime.as_str()));
        if legacy_path != path && legacy_path.is_file() {
            return Ok(legacy_path);
        }
        return Err(PipelineError::PreservedVersionNotFound {
            entity_id: entity_id.clone(),
            runtime: runtime.clone(),
        });
    }
    latest_preserved_version(cache, entity_id, runtime)
}

fn latest_preserved_version(
    cache: &CacheLayout,
    entity_id: &EntityId,
    runtime: &RuntimeName,
) -> Result<PathBuf> {
    let dir = conflict_entity_dir(&cache.conflicts_dir, entity_id);
    let mut candidates = Vec::new();
    match fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|source| PipelineError::Io {
                    action: "read directory entry",
                    path: dir.clone(),
                    source,
                })?;
                let path = entry.path();
                if let Some(candidate) = preserved_version_candidate(path, runtime) {
                    candidates.push(candidate);
                }
            }
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read directory",
                path: dir,
                source,
            });
        }
    }
    candidates.sort_by(compare_preserved_versions);
    candidates
        .pop()
        .map(|candidate| candidate.path)
        .ok_or_else(|| PipelineError::PreservedVersionNotFound {
            entity_id: entity_id.clone(),
            runtime: runtime.clone(),
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreservedVersionCandidate {
    path: PathBuf,
    timestamp: String,
    unix_seconds: Option<u64>,
}

fn preserved_version_candidate(
    path: PathBuf,
    runtime: &RuntimeName,
) -> Option<PreservedVersionCandidate> {
    let file_name = path.file_name()?.to_str()?;
    let timestamp = file_name
        .strip_prefix(&format!("{}-", runtime.as_str()))?
        .strip_suffix(".md")?
        .to_string();
    let unix_seconds = timestamp
        .strip_prefix("unix-")
        .and_then(|value| value.parse::<u64>().ok());
    Some(PreservedVersionCandidate {
        path,
        timestamp,
        unix_seconds,
    })
}

fn compare_preserved_versions(
    left: &PreservedVersionCandidate,
    right: &PreservedVersionCandidate,
) -> std::cmp::Ordering {
    match (left.unix_seconds, right.unix_seconds) {
        (Some(left_seconds), Some(right_seconds)) => left_seconds
            .cmp(&right_seconds)
            .then_with(|| left.timestamp.cmp(&right.timestamp)),
        _ => left.timestamp.cmp(&right.timestamp),
    }
    .then_with(|| left.path.cmp(&right.path))
}

fn read_lockfile_or_empty(repo_root: &Path) -> Result<Lockfile> {
    match read_lockfile(repo_root) {
        Ok(lockfile) => Ok(lockfile),
        Err(LockfileError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(Lockfile::empty())
        }
        Err(error) => Err(error.into()),
    }
}

fn detect_runtime_markers(
    repo_root: &Path,
    adapters: &dyn AdapterRegistry,
) -> Result<RuntimeMarkers> {
    Ok(RuntimeMarkers {
        claude: adapters
            .detect(&runtime_name("claude")?, repo_root)?
            .present,
        codex: adapters.detect(&runtime_name("codex")?, repo_root)?.present,
        copilot: adapters
            .detect(&runtime_name("copilot")?, repo_root)?
            .present,
        cursor: adapters
            .detect(&runtime_name("cursor")?, repo_root)?
            .present,
        gemini: adapters
            .detect(&runtime_name("gemini")?, repo_root)?
            .present,
    })
}

fn adapter_declarations(
    markers: &RuntimeMarkers,
) -> Result<BTreeMap<RuntimeName, AdapterDeclaration>> {
    let mut adapters = BTreeMap::new();
    if markers.claude {
        adapters.insert(RuntimeName::new("claude")?, claude_adapter_declaration());
    }
    if markers.codex {
        adapters.insert(RuntimeName::new("codex")?, codex_adapter_declaration());
    }
    if markers.copilot {
        adapters.insert(RuntimeName::new("copilot")?, copilot_adapter_declaration());
    }
    if markers.cursor {
        adapters.insert(RuntimeName::new("cursor")?, cursor_adapter_declaration());
    }
    if markers.gemini {
        adapters.insert(RuntimeName::new("gemini")?, gemini_adapter_declaration());
    }
    Ok(adapters)
}

fn claude_adapter_declaration() -> AdapterDeclaration {
    AdapterDeclaration {
        mode: AdapterMode::Bundled,
        protocol_version: PROTOCOL_VERSION,
        entities: vec![
            EntityType::Instructions,
            EntityType::Rule,
            EntityType::Command,
            EntityType::Hook,
            EntityType::McpBinding,
            EntityType::PermissionPolicy,
            EntityType::Skill,
            EntityType::Subagent,
        ],
        hooks: vec![HookKind::PostToolUse],
    }
}

fn codex_adapter_declaration() -> AdapterDeclaration {
    AdapterDeclaration {
        mode: AdapterMode::Bundled,
        protocol_version: PROTOCOL_VERSION,
        entities: vec![
            EntityType::Instructions,
            EntityType::Hook,
            EntityType::McpBinding,
            EntityType::PermissionPolicy,
            EntityType::Skill,
            EntityType::Subagent,
        ],
        hooks: vec![HookKind::PostToolUse],
    }
}

fn cursor_adapter_declaration() -> AdapterDeclaration {
    AdapterDeclaration {
        mode: AdapterMode::Bundled,
        protocol_version: PROTOCOL_VERSION,
        entities: vec![EntityType::Instructions, EntityType::Rule],
        hooks: Vec::new(),
    }
}

fn copilot_adapter_declaration() -> AdapterDeclaration {
    AdapterDeclaration {
        mode: AdapterMode::Bundled,
        protocol_version: PROTOCOL_VERSION,
        entities: vec![
            EntityType::Instructions,
            EntityType::Prompt,
            EntityType::Skill,
            EntityType::Subagent,
        ],
        hooks: Vec::new(),
    }
}

fn gemini_adapter_declaration() -> AdapterDeclaration {
    AdapterDeclaration {
        mode: AdapterMode::Bundled,
        protocol_version: PROTOCOL_VERSION,
        entities: vec![
            EntityType::Instructions,
            EntityType::Command,
            EntityType::Skill,
        ],
        hooks: Vec::new(),
    }
}

fn entity_slug(entity_id: &EntityId) -> &str {
    entity_id
        .as_str()
        .split_once(':')
        .map(|(_, slug)| slug)
        .unwrap_or("root")
}

fn instruction_scope(entity_id: &EntityId, entity_type: EntityType) -> Option<String> {
    if entity_type != EntityType::Instructions {
        return None;
    }
    match entity_id.as_str() {
        "instructions:root" => Some("root".to_string()),
        value => value
            .strip_prefix("instructions:scoped:")
            .map(std::string::ToString::to_string),
    }
}

fn canonical_lockfile_path_for_file(
    entity_type: EntityType,
    slug: &str,
    entity_file_path: &Path,
) -> PathBuf {
    match entity_type {
        EntityType::Instructions if slug == "root" => PathBuf::from("../AGENTS.md"),
        EntityType::Instructions => PathBuf::from("instructions").join(file_name_for_slug(
            scoped_slug(slug),
            entity_file_path,
            "md",
        )),
        EntityType::Rule => {
            PathBuf::from("rules").join(file_name_for_slug(slug, entity_file_path, "md"))
        }
        EntityType::Prompt => {
            PathBuf::from("prompts").join(file_name_for_slug(slug, entity_file_path, "md"))
        }
        EntityType::Command => PathBuf::from("commands").join(command_file_name(
            slug,
            &extension_for_file(entity_file_path, "md"),
        )),
        EntityType::Hook => {
            PathBuf::from("hooks").join(file_name_for_slug(slug, entity_file_path, "json"))
        }
        EntityType::McpBinding => {
            PathBuf::from("mcp-bindings").join(file_name_for_slug(slug, entity_file_path, "json"))
        }
        EntityType::PermissionPolicy => PathBuf::from("permission-policies")
            .join(file_name_for_slug(slug, entity_file_path, "json")),
        EntityType::Skill => PathBuf::from("skills").join(slug).join("SKILL.md"),
        EntityType::Subagent => PathBuf::from("subagents").join(format!("{slug}.md")),
    }
}

fn canonical_repo_path_for_file(
    entity_type: EntityType,
    slug: &str,
    entity_file_path: &Path,
) -> PathBuf {
    match entity_type {
        EntityType::Instructions if slug == "root" => PathBuf::from("AGENTS.md"),
        EntityType::Instructions => {
            PathBuf::from(".ai")
                .join("instructions")
                .join(file_name_for_slug(
                    scoped_slug(slug),
                    entity_file_path,
                    "md",
                ))
        }
        EntityType::Rule => PathBuf::from(".ai").join("rules").join(file_name_for_slug(
            slug,
            entity_file_path,
            "md",
        )),
        EntityType::Prompt => PathBuf::from(".ai")
            .join("prompts")
            .join(file_name_for_slug(slug, entity_file_path, "md")),
        EntityType::Command => PathBuf::from(".ai")
            .join("commands")
            .join(command_file_name(
                slug,
                &extension_for_file(entity_file_path, "md"),
            )),
        EntityType::Hook => PathBuf::from(".ai").join("hooks").join(file_name_for_slug(
            slug,
            entity_file_path,
            "json",
        )),
        EntityType::McpBinding => PathBuf::from(".ai")
            .join("mcp-bindings")
            .join(file_name_for_slug(slug, entity_file_path, "json")),
        EntityType::PermissionPolicy => PathBuf::from(".ai")
            .join("permission-policies")
            .join(file_name_for_slug(slug, entity_file_path, "json")),
        EntityType::Skill => PathBuf::from(".ai")
            .join("skills")
            .join(slug)
            .join("SKILL.md"),
        EntityType::Subagent => PathBuf::from(".ai")
            .join("subagents")
            .join(format!("{slug}.md")),
    }
}

fn scoped_instruction_file_name(slug: &str) -> PathBuf {
    PathBuf::from(format!("{}.md", scoped_slug(slug)))
}

fn scoped_slug(slug: &str) -> &str {
    slug.strip_prefix("scoped:").unwrap_or(slug)
}

fn file_name_for_slug(slug: &str, entity_file_path: &Path, default_extension: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}.{}",
        slug,
        extension_for_file(entity_file_path, default_extension)
    ))
}

fn extension_for_file(entity_file_path: &Path, default_extension: &str) -> String {
    entity_file_path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or(default_extension)
        .to_string()
}

fn command_file_name(slug: &str, extension: &str) -> PathBuf {
    let mut parts = slug.split(':').peekable();
    let mut path = PathBuf::new();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            path.push(part);
        } else {
            path.push(format!("{part}.{extension}"));
        }
    }
    path
}

fn path_from_lockfile(
    repo_root: &Path,
    location: &LocationKey,
    lockfile_path: &Path,
) -> Result<PathBuf> {
    if let Some(root_relative) = repo_relative_lockfile_path(location, lockfile_path)? {
        return Ok(repo_root.join(root_relative));
    }

    Ok(repo_root.join(location.as_str()).join(lockfile_path))
}

fn repo_relative_lockfile_path(
    location: &LocationKey,
    lockfile_path: &Path,
) -> Result<Option<PathBuf>> {
    if lockfile_path.as_os_str().is_empty() || lockfile_path.is_absolute() {
        return Err(PipelineError::EntityFormat {
            path: lockfile_path.to_path_buf(),
            message: "unsafe lockfile path".to_string(),
        });
    }

    let mut components = lockfile_path.components();
    let Some(first) = components.next() else {
        return Err(PipelineError::EntityFormat {
            path: lockfile_path.to_path_buf(),
            message: "unsafe lockfile path".to_string(),
        });
    };

    if first == Component::ParentDir {
        let mut root_relative = PathBuf::new();
        for component in components {
            let Component::Normal(part) = component else {
                return Err(PipelineError::EntityFormat {
                    path: lockfile_path.to_path_buf(),
                    message: "unsafe repo-relative lockfile path".to_string(),
                });
            };
            root_relative.push(part);
        }
        if root_relative.as_os_str().is_empty() {
            return Err(PipelineError::EntityFormat {
                path: lockfile_path.to_path_buf(),
                message: "unsafe repo-relative lockfile path".to_string(),
            });
        }
        if !repo_relative_path_allowed(location, &root_relative) {
            return Err(PipelineError::EntityFormat {
                path: lockfile_path.to_path_buf(),
                message: "repo-relative lockfile path is not allowed for this location".to_string(),
            });
        }
        return Ok(Some(root_relative));
    }

    if !matches!(first, Component::Normal(_)) {
        return Err(PipelineError::EntityFormat {
            path: lockfile_path.to_path_buf(),
            message: "unsafe lockfile path".to_string(),
        });
    }
    for component in components {
        if !matches!(component, Component::Normal(_)) {
            return Err(PipelineError::EntityFormat {
                path: lockfile_path.to_path_buf(),
                message: "unsafe lockfile path".to_string(),
            });
        }
    }
    Ok(None)
}

fn repo_relative_path_allowed(location: &LocationKey, root_relative: &Path) -> bool {
    match location.as_str() {
        ".ai" => root_relative == Path::new("AGENTS.md"),
        ".claude" => {
            root_relative == Path::new("CLAUDE.md") || root_relative == Path::new(".mcp.json")
        }
        ".codex" => codex_repo_relative_instruction_path(root_relative),
        ".gemini" => gemini_repo_relative_context_path(root_relative),
        ".copilot" => {
            root_relative == Path::new(".github/copilot-instructions.md")
                || root_relative.starts_with(".github/instructions")
                || root_relative.starts_with(".github/prompts")
                || root_relative.starts_with(".github/skills")
                || root_relative.starts_with(".github/agents")
        }
        _ => false,
    }
}

fn gemini_repo_relative_context_path(root_relative: &Path) -> bool {
    if root_relative == Path::new("GEMINI.md") {
        return true;
    }
    if root_relative.file_name().and_then(|name| name.to_str()) != Some("GEMINI.md") {
        return false;
    }
    safe_nested_repo_root(root_relative)
}

fn codex_repo_relative_instruction_path(root_relative: &Path) -> bool {
    if root_relative == Path::new("AGENTS.md") {
        return true;
    }
    if root_relative.file_name().and_then(|name| name.to_str()) != Some("AGENTS.md") {
        return false;
    }
    safe_nested_repo_root(root_relative)
}

fn safe_nested_repo_root(root_relative: &Path) -> bool {
    let Some(parent) = root_relative.parent() else {
        return false;
    };
    parent.components().all(|component| {
        let Component::Normal(part) = component else {
            return false;
        };
        part.to_str()
            .is_some_and(|part| !part.starts_with('.') && part != "target")
    })
}

fn read_text_file(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|source| PipelineError::Io {
        action: "read file",
        path: path.to_path_buf(),
        source,
    })
}

fn relative_to(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn write_integrity(repo_root: &Path, cache: &CacheLayout) -> Result<()> {
    let pin = current_integrity_pin(repo_root)?;
    write_integrity_pin(&cache.integrity_json, &pin)?;
    Ok(())
}

fn verify_integrity(repo_root: &Path, cache: &CacheLayout) -> Result<()> {
    let pinned = match read_integrity_pin(&cache.integrity_json) {
        Ok(pin) => pin,
        Err(StateError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            let pin = current_integrity_pin(repo_root)?;
            write_integrity_pin(&cache.integrity_json, &pin)?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let current = current_integrity_pin(repo_root)?;
    if pinned.binary_path != current.binary_path || pinned.binary_sha256 != current.binary_sha256 {
        return Err(PipelineError::IntegrityMismatch {
            pinned_path: pinned.binary_path,
            pinned_hash: pinned.binary_sha256,
            current_path: current.binary_path,
            current_hash: current.binary_sha256,
        });
    }

    Ok(())
}

fn current_integrity_pin(repo_root: &Path) -> Result<IntegrityPin> {
    let current_exe = std::env::current_exe().map_err(|source| PipelineError::Io {
        action: "resolve current executable",
        path: repo_root.to_path_buf(),
        source,
    })?;
    Ok(IntegrityPin {
        binary_sha256: sha256_file(&current_exe)?,
        binary_path: current_exe,
        binary_version: VERSION.to_string(),
        pinned_at: timestamp_now(),
    })
}

fn default_cache_root() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("AGENTMESH_CACHE_DIR") {
        return Ok(PathBuf::from(value));
    }

    if let Some(value) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(value).join("agentmesh"));
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return Ok(PathBuf::from(home).join("Library/Caches/agentmesh"));
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            return Ok(PathBuf::from(local_app_data).join("agentmesh"));
        }
    }

    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".cache/agentmesh"));
    }

    Err(PipelineError::Io {
        action: "resolve cache root",
        path: PathBuf::from("agentmesh-cache"),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "cache root not available"),
    })
}

fn timestamp_now() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("unix-{}", duration.as_secs()),
        Err(_) => "unix-0".to_string(),
    }
}

fn location_key(value: &str) -> std::result::Result<LocationKey, TypeError> {
    LocationKey::new(value)
}

fn runtime_name(value: &str) -> Result<RuntimeName> {
    RuntimeName::new(value).map_err(Into::into)
}

fn runtime_dir(repo_root: &Path, runtime: &RuntimeName) -> PathBuf {
    if runtime.as_str() == "copilot" {
        return repo_root.join(".github");
    }
    repo_root.join(format!(".{}", runtime.as_str()))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use super::{
        CapabilityReportMode, PipelineError, PlanOptions, capability_skip_count_for_lockfile,
        capability_skip_report_for_lockfile, path_from_lockfile,
    };
    use crate::config::{AgentmeshConfig, CapabilityFallback};
    use crate::lockfile::{
        AdapterDeclaration, AdapterMode, HookKind, Lockfile, LockfileEntity, OverrideEntry,
        entity_schema_for_type, read_lockfile, write_lockfile,
    };
    use crate::merge::preserve_losing_version;
    use crate::pending_queue::PendingQueue;
    use crate::state::{CacheLayout, PendingAction, PendingSyncRecord};
    use crate::types::{EntityId, Hash, LocationKey, RuntimeName};
    use crate::{EntityType, SyncOptions, ack};
    use agentmesh_protocol::{
        DetectResponse, EmitRequest, EmitResponse, EntityFile, ImportRequest, ImportResponse,
        ImportedEntity,
    };
    use proptest::prelude::*;

    fn run_sync(
        repo_root: &std::path::Path,
        opts: SyncOptions,
        cache: &CacheLayout,
    ) -> super::Result<super::SyncSummary> {
        super::run_sync(repo_root, opts, cache, &super::BundledAdapterRegistry)
    }

    fn build_sync_plan(
        repo_root: &std::path::Path,
        previous: Lockfile,
        cache: &CacheLayout,
        options: PlanOptions,
    ) -> super::Result<super::SyncPlan> {
        super::build_sync_plan(
            repo_root,
            previous,
            cache,
            options,
            &super::BundledAdapterRegistry,
        )
    }

    fn restore_with_cache(
        repo_root: &std::path::Path,
        cache: &CacheLayout,
        entity_id: &EntityId,
        from: RuntimeName,
        at: Option<&str>,
        dry_run: bool,
    ) -> super::Result<crate::RestoreSummary> {
        super::restore_with_cache(
            repo_root,
            cache,
            entity_id,
            from,
            at,
            dry_run,
            &super::BundledAdapterRegistry,
        )
    }

    fn doctor(repo_root: &std::path::Path) -> crate::Result<crate::DoctorReport> {
        super::doctor_with_adapter_registry(repo_root, &super::BundledAdapterRegistry)
            .map_err(Into::into)
    }

    fn reconcile_lock(repo_root: &std::path::Path) -> crate::Result<crate::ReconcileSummary> {
        super::reconcile_lock_with_adapter_registry(repo_root, &super::BundledAdapterRegistry)
            .map_err(Into::into)
    }

    fn entity_id(value: &str) -> EntityId {
        match EntityId::new(value) {
            Ok(entity_id) => entity_id,
            Err(error) => panic!("test entity id should be valid: {error}"),
        }
    }

    fn runtime_name(value: &str) -> RuntimeName {
        match RuntimeName::new(value) {
            Ok(runtime) => runtime,
            Err(error) => panic!("test runtime name should be valid: {error}"),
        }
    }

    fn location_key(value: &str) -> LocationKey {
        match LocationKey::new(value) {
            Ok(location) => location,
            Err(error) => panic!("test location key should be valid: {error}"),
        }
    }

    fn hash(value: &str) -> Hash {
        match Hash::new(value) {
            Ok(hash) => hash,
            Err(error) => panic!("test hash should be valid: {error}"),
        }
    }

    proptest! {
        #[test]
        fn entity_ids_accept_generated_lowercase_skill_slugs(slug in "[a-z][a-z0-9]{0,24}") {
            let value = format!("skill:{slug}");
            let parsed = EntityId::new(value.clone());
            prop_assert!(parsed.is_ok());
            if let Ok(entity_id) = parsed {
                prop_assert_eq!(entity_id.as_str(), value);
            }
        }
    }

    #[test]
    fn empty_lockfile_snapshot_is_stable() {
        let yaml = match serde_norway::to_string(&Lockfile::empty()) {
            Ok(yaml) => yaml,
            Err(error) => panic!("lockfile should serialize: {error}"),
        };
        insta::assert_snapshot!(yaml, @r"
version: 1
schema: 2
");
    }

    fn entity_entry(entity_type: EntityType, canonical_hash: Hash) -> LockfileEntity {
        LockfileEntity {
            entity_type,
            entity_schema: entity_schema_for_type(entity_type),
            scope: if entity_type == EntityType::Instructions {
                Some("root".to_string())
            } else {
                None
            },
            locations: std::collections::BTreeMap::new(),
            canonical_sha256: canonical_hash,
            emitted_native_sha256: std::collections::BTreeMap::new(),
            lineage: Vec::new(),
            pending_conflict_resolution: None,
            rename_history: Vec::new(),
            id_pin: None,
        }
    }

    fn cache_for(temp: &tempfile::TempDir, repo_root: &std::path::Path) -> CacheLayout {
        match CacheLayout::new(&temp.path().join("cache"), repo_root) {
            Ok(cache) => cache,
            Err(error) => panic!("cache layout should build: {error}"),
        }
    }

    #[derive(Default)]
    struct TestAdapterRegistry {
        imports: Vec<ImportedEntity>,
        emitted: RefCell<Vec<EmitRequest>>,
        present: bool,
        fail_emit: bool,
    }

    impl super::AdapterRegistry for TestAdapterRegistry {
        fn detect(
            &self,
            _runtime: &RuntimeName,
            _repo_root: &std::path::Path,
        ) -> super::Result<DetectResponse> {
            Ok(DetectResponse {
                present: self.present,
                version: None,
                files: Vec::new(),
            })
        }

        fn import(
            &self,
            _runtime: &RuntimeName,
            _repo_root: &std::path::Path,
            _request: ImportRequest,
        ) -> super::Result<ImportResponse> {
            Ok(ImportResponse {
                entities: self.imports.clone(),
                skipped: Vec::new(),
            })
        }

        fn emit(
            &self,
            runtime: &RuntimeName,
            _repo_root: &std::path::Path,
            request: EmitRequest,
        ) -> super::Result<EmitResponse> {
            self.emitted.borrow_mut().push(request);
            if self.fail_emit {
                return Err(PipelineError::Adapter {
                    runtime: runtime.clone(),
                    message: "forced emit failure".to_string(),
                });
            }
            Ok(EmitResponse {
                files_written: Vec::new(),
                skipped: Vec::new(),
                partial_fidelity: Vec::new(),
            })
        }
    }

    fn test_view(
        entity_type: EntityType,
        location: &str,
        relative_path: &str,
        contents: &str,
        native_hash: &str,
        mtime_seconds: u64,
    ) -> super::EntityView {
        let path = PathBuf::from(relative_path);
        let file_key = path
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| super::primary_entity_file_path(entity_type));
        super::EntityView {
            entity_type,
            scope: None,
            location: location_key(location),
            relative_path: path.clone(),
            lockfile_path: path,
            canonical_contents: contents.as_bytes().to_vec(),
            files: BTreeMap::from([(file_key, contents.as_bytes().to_vec())]),
            runtime_overrides: BTreeMap::new(),
            native_hash: hash(native_hash),
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_seconds),
            id_pin: None,
        }
    }

    fn entity_file_text(file: &EntityFile) -> String {
        let bytes = match file.decode_bytes() {
            Ok(bytes) => bytes,
            Err(error) => panic!("entity file should decode: {error}"),
        };
        match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => panic!("entity file should be UTF-8: {error}"),
        }
    }

    #[test]
    fn init_style_sync_imports_and_emits_skills() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".claude/skills/security-review")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::create_dir_all(repo.join(".codex")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".claude/skills/security-review/SKILL.md"),
            "---\ndescription: Audit code\nname: security-review\n---\nBody\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::create_dir_all(repo.join(".claude/skills/security-review/assets")) {
            panic!("fixture asset dir should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".claude/skills/security-review/assets/icon.bin"),
            [0, 159, 146, 150],
        ) {
            panic!("fixture asset should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let summary = match run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("sync should succeed: {error}"),
        };

        assert!(summary.changed);
        assert!(repo.join(".ai/skills/security-review/SKILL.md").exists());
        assert_eq!(
            fs::read(repo.join(".ai/skills/security-review/assets/icon.bin"))
                .unwrap_or_else(|error| panic!("canonical asset should be readable: {error}")),
            vec![0, 159, 146, 150]
        );
        assert!(repo.join(".codex/skills/security-review/SKILL.md").exists());
        assert_eq!(
            fs::read(repo.join(".codex/skills/security-review/assets/icon.bin"))
                .unwrap_or_else(|error| panic!("codex asset should be readable: {error}")),
            vec![0, 159, 146, 150]
        );
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should be readable: {error}"),
        };
        assert!(
            lockfile
                .entities
                .contains_key(&entity_id("skill:security-review"))
        );
        let entity = &lockfile.entities[&entity_id("skill:security-review")];
        assert!(entity.locations.contains_key(&location_key(".codex")));
        let emitted = match entity.emitted_native_sha256.get(&location_key(".codex")) {
            Some(hash) => hash,
            None => panic!("codex emitted hash should be recorded"),
        };
        let actual_files = match super::collect_entity_text_files(
            &repo.join(".codex/skills/security-review"),
            &repo.join(".codex/skills/security-review"),
        ) {
            Ok(files) => files,
            Err(error) => panic!("emitted skill files should hash: {error}"),
        };
        let actual = match super::hash_entity_files(&actual_files) {
            Ok(hash) => hash,
            Err(error) => panic!("emitted skill should hash: {error}"),
        };
        assert_eq!(emitted, &actual);
    }

    #[cfg(unix)]
    #[test]
    fn entity_file_collection_rejects_symlinked_paths() {
        use std::os::unix::fs::symlink;

        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path().join("skill");
        if let Err(error) = fs::create_dir_all(&root) {
            panic!("skill directory should be created: {error}");
        }
        if let Err(error) = fs::write(root.join("SKILL.md"), "content") {
            panic!("skill file should be written: {error}");
        }
        if let Err(error) = fs::write(temp.path().join("outside.txt"), "outside") {
            panic!("outside file should be written: {error}");
        }
        if let Err(error) = symlink(temp.path().join("outside.txt"), root.join("outside.txt")) {
            panic!("symlink should be created: {error}");
        }

        let error = match super::collect_entity_text_files(&root, &root) {
            Ok(_) => panic!("symlinked entity path should fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("symlinked entity path"));
    }

    #[test]
    fn pin_marker_controls_imported_identity() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".claude/skills/source-name")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".claude/skills/source-name/SKILL.md"),
            "<!-- agentmesh:id=skill:pinned-review -->\n---\nname: source-name\n---\nBody\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("sync should succeed: {error}");
        }

        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should be readable: {error}"),
        };
        let id = entity_id("skill:pinned-review");
        assert!(lockfile.entities.contains_key(&id));
        assert_eq!(lockfile.entities[&id].id_pin, Some(id));
    }

    #[test]
    fn codex_toml_subagents_import_through_adapter() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".codex/agents")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".codex/agents/reviewer.toml"),
            "name = \"reviewer\"\ninstructions = \"Check edge cases\"\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("sync should import codex subagent: {error}");
        }

        let canonical = match fs::read_to_string(repo.join(".ai/subagents/reviewer.md")) {
            Ok(contents) => contents,
            Err(error) => panic!("canonical subagent should read: {error}"),
        };
        assert!(canonical.contains("name: reviewer"));
        assert!(canonical.contains("Check edge cases"));
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should be readable: {error}"),
        };
        let entity = &lockfile.entities[&entity_id("subagent:reviewer")];
        assert_eq!(
            entity.locations[&location_key(".codex")],
            std::path::PathBuf::from("agents/reviewer.toml")
        );
    }

    #[test]
    fn duplicate_runtime_slugs_are_disambiguated() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        for name in ["security-review", "security_review"] {
            if let Err(error) = fs::create_dir_all(repo.join(".claude/skills").join(name)) {
                panic!("fixture dirs should be created: {error}");
            }
            if let Err(error) = fs::write(
                repo.join(".claude/skills").join(name).join("SKILL.md"),
                format!("---\nname: {name}\n---\nBody\n"),
            ) {
                panic!("fixture should be written: {error}");
            }
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("sync should succeed: {error}");
        }

        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should be readable: {error}"),
        };
        assert!(
            lockfile
                .entities
                .contains_key(&entity_id("skill:security-review"))
        );
        assert!(
            lockfile
                .entities
                .contains_key(&entity_id("skill:security-review-2"))
        );
    }

    #[test]
    fn exact_hash_rename_keeps_identity_and_updates_location() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".claude/skills/old-name")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".claude/skills/old-name/SKILL.md"),
            "---\nname: old-name\n---\nBody\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);
        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("initial sync should succeed: {error}");
        }
        if let Err(error) = fs::rename(
            repo.join(".claude/skills/old-name"),
            repo.join(".claude/skills/new-name"),
        ) {
            panic!("fixture should rename: {error}");
        }

        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("rename sync should succeed: {error}");
        }

        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should be readable: {error}"),
        };
        let id = entity_id("skill:old-name");
        let entity = &lockfile.entities[&id];
        assert_eq!(
            entity.locations[&location_key(".claude")],
            std::path::PathBuf::from("skills/new-name/SKILL.md")
        );
        assert_eq!(entity.rename_history.len(), 1);
        assert!(repo.join(".ai/skills/new-name/SKILL.md").exists());
    }

    #[test]
    fn sync_check_reports_drift_without_writing() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".claude")) {
            panic!("repo should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "Instructions\n") {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let summary = match run_sync(
            &repo,
            SyncOptions {
                check: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("check should succeed: {error}"),
        };

        assert!(summary.changed);
        assert!(!repo.join("agentmesh.lock").exists());
    }

    #[test]
    fn capability_skips_are_reported_from_adapter_declarations() {
        let mut lockfile = Lockfile::empty();
        lockfile.entities.insert(
            entity_id("subagent:reviewer"),
            entity_entry(
                EntityType::Subagent,
                hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ),
        );
        lockfile.adapters.insert(
            runtime_name("codex"),
            AdapterDeclaration {
                mode: AdapterMode::Bundled,
                protocol_version: 1,
                entities: vec![EntityType::Skill],
                hooks: Vec::new(),
            },
        );

        let skipped = match capability_skip_count_for_lockfile(
            &lockfile,
            &crate::config::AgentmeshConfig::default(),
        ) {
            Ok(skipped) => skipped,
            Err(error) => panic!("capability skip count should succeed: {error}"),
        };

        assert_eq!(skipped, 1);
    }

    #[test]
    fn sync_check_reports_capability_skips_for_mixed_runtime_support() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".claude/commands")) {
            panic!("claude command dir should be created: {error}");
        }
        if let Err(error) = fs::create_dir_all(repo.join(".codex")) {
            panic!("codex marker should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".claude/commands/review.md"),
            "---\ndescription: Review changes\n---\nReview the current diff.\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let summary = match run_sync(
            &repo,
            SyncOptions {
                check: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("sync check should succeed: {error}"),
        };

        assert!(summary.changed);
        assert_eq!(summary.capability_skipped, 1);
        assert_eq!(summary.capability_skips.len(), 1);
        let finding = &summary.capability_skips[0];
        assert_eq!(finding.runtime, runtime_name("codex"));
        assert_eq!(finding.entity_id, entity_id("command:review"));
        assert_eq!(finding.entity_type, EntityType::Command);
        assert_eq!(finding.fallback, CapabilityFallback::Warn);
        assert_eq!(
            finding.locations[&location_key(".claude")],
            PathBuf::from("commands/review.md")
        );
        assert!(!repo.join("agentmesh.lock").exists());
    }

    #[test]
    fn cursor_scoped_rule_syncs_to_claude_rule_paths() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".cursor/rules")) {
            panic!("cursor rules dir should be created: {error}");
        }
        if let Err(error) = fs::create_dir_all(repo.join(".claude")) {
            panic!("claude marker should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".cursor/rules/api.mdc"),
            "---\ndescription: API guidance\nglobs:\n  - crates/api/**\n---\nUse API conventions.\n",
        ) {
            panic!("cursor rule should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let summary = match run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("sync should succeed: {error}"),
        };

        assert!(summary.changed);
        assert_eq!(summary.pending_enqueued, 1, "summary: {summary:?}");
        if summary.pending_drained != 1 {
            let queue = PendingQueue::new(&cache.pending_syncs_dir);
            let remaining = queue
                .read_ready()
                .unwrap_or_else(|error| panic!("pending queue should read: {error}"));
            panic!("summary: {summary:?}; remaining: {remaining:?}");
        }
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should read: {error}"),
        };
        assert!(
            lockfile.adapters.contains_key(&runtime_name("claude")),
            "adapters: {:?}",
            lockfile.adapters.keys().collect::<Vec<_>>()
        );
        let id = entity_id("instructions:scoped:api");
        let entity = lockfile
            .entities
            .get(&id)
            .unwrap_or_else(|| panic!("scoped Cursor rule should be tracked"));
        assert_eq!(entity.entity_type, EntityType::Instructions);
        assert_eq!(
            entity.locations[&location_key(".cursor")],
            PathBuf::from("rules/api.mdc")
        );
        let claude_rule = match fs::read_to_string(repo.join(".claude/rules/api.md")) {
            Ok(contents) => contents,
            Err(error) => panic!("claude rule should be emitted: {error}"),
        };
        assert!(claude_rule.contains("paths:"));
        assert!(claude_rule.contains("crates/api/**"));
        assert!(!claude_rule.contains("globs:"));
        assert_eq!(
            entity.locations.get(&location_key(".claude")),
            Some(&PathBuf::from("rules/api.md")),
            "locations: {:?}",
            entity.locations
        );
    }

    #[test]
    fn nested_cursor_rule_uses_valid_flattened_id() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".cursor/rules/team")) {
            panic!("cursor rules dir should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".cursor/rules/team/security.mdc"),
            "---\ndescription: Security\n---\nSecurity body.\n",
        ) {
            panic!("cursor rule should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let summary = match run_sync(
            &repo,
            SyncOptions {
                check: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("sync check should succeed: {error}"),
        };
        let plan = match build_sync_plan(&repo, Lockfile::empty(), &cache, PlanOptions::default()) {
            Ok(plan) => plan,
            Err(error) => panic!("plan should build: {error}"),
        };

        assert!(summary.changed);
        assert!(
            plan.lockfile
                .entities
                .contains_key(&entity_id("rule:team-security"))
        );
    }

    #[test]
    fn invalid_cursor_rule_fails_sync_check_and_reports_in_doctor() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".cursor/rules")) {
            panic!("cursor rules dir should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".cursor/rules/broken.mdc"),
            "---\ndescription: \"unterminated\n---\nBroken\n",
        ) {
            panic!("cursor rule should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".cursor/rules/empty-globs.mdc"),
            "---\nglobs: []\n---\nEmpty\n",
        ) {
            panic!("cursor rule should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let error = run_sync(
            &repo,
            SyncOptions {
                check: true,
                ..SyncOptions::default()
            },
            &cache,
        )
        .expect_err("invalid Cursor rule should fail sync check");
        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should succeed: {error}"),
        };

        assert!(matches!(error, PipelineError::EntityFormat { .. }));
        assert!(error.to_string().contains(".cursor/rules/broken.mdc"));
        assert!(report.findings.iter().any(|finding| {
            finding.starts_with("cursor_rule_invalid: .cursor/rules/broken.mdc")
        }));
        assert!(report.findings.iter().any(|finding| {
            finding
                == "cursor_rule_invalid: .cursor/rules/empty-globs.mdc: Cursor rule globs must contain one non-empty string scope"
        }));
    }

    #[cfg(unix)]
    #[test]
    fn doctor_reports_symlinked_cursor_rules_without_aborting() {
        use std::os::unix::fs::symlink;

        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        if let Err(error) = fs::create_dir_all(&outside) {
            panic!("outside dir should be created: {error}");
        }
        if let Err(error) = fs::create_dir_all(repo.join(".cursor")) {
            panic!("cursor dir should be created: {error}");
        }
        if let Err(error) = symlink(&outside, repo.join(".cursor/rules")) {
            panic!("cursor rules symlink should be created: {error}");
        }

        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should continue through adapter detect errors: {error}"),
        };

        assert!(
            report
                .findings
                .iter()
                .any(|finding| { finding.starts_with("adapter_cursor_detect_error:") })
        );
        assert!(report.findings.iter().any(|finding| {
            finding == "cursor_rule_invalid: .cursor/rules: symlinked path is not supported"
        }));
    }

    #[test]
    fn cursor_frontmatter_stays_cursor_only_across_runtime_emit() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".cursor/rules")) {
            panic!("cursor rules dir should be created: {error}");
        }
        if let Err(error) = fs::create_dir_all(repo.join(".claude")) {
            panic!("claude marker should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".cursor/rules/security.mdc"),
            "---\ndescription: Security\nalwaysApply: true\ncursorPriority: high\n---\nSecurity body.\n",
        ) {
            panic!("cursor rule should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            panic!("initial sync should succeed: {error}");
        }
        let claude_rule = match fs::read_to_string(repo.join(".claude/rules/security.md")) {
            Ok(contents) => contents,
            Err(error) => panic!("claude rule should be emitted: {error}"),
        };
        assert!(claude_rule.contains("description: Security"));
        assert!(!claude_rule.contains("alwaysApply"));
        assert!(!claude_rule.contains("cursorPriority"));

        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should read: {error}"),
        };
        let overrides = &lockfile.overrides[&entity_id("rule:security")][&runtime_name("cursor")].0;
        assert_eq!(
            overrides.get("cursorPriority"),
            Some(&serde_json::json!("high"))
        );

        if let Err(error) = fs::write(
            repo.join(".cursor/rules/security.mdc"),
            "---\ndescription: Security\n---\nSecurity body.\n",
        ) {
            panic!("cursor rule metadata should be removed: {error}");
        }
        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("metadata removal sync should succeed: {error}");
        }
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should read: {error}"),
        };
        assert!(
            !lockfile
                .overrides
                .get(&entity_id("rule:security"))
                .is_some_and(|overrides| overrides.contains_key(&runtime_name("cursor")))
        );

        if let Err(error) = fs::write(repo.join(".ai/rules/security.mdc"), "# Updated\n") {
            panic!("canonical rule should be updated: {error}");
        }
        if let Err(error) = run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            panic!("second sync should succeed: {error}");
        }
        let cursor_rule = match fs::read_to_string(repo.join(".cursor/rules/security.mdc")) {
            Ok(contents) => contents,
            Err(error) => panic!("cursor rule should be emitted: {error}"),
        };
        assert!(!cursor_rule.contains("cursorPriority"));
        assert!(!cursor_rule.contains("alwaysApply"));
        assert!(cursor_rule.contains("# Updated"));
    }

    #[test]
    fn cursor_overrides_clear_when_cursor_rule_view_disappears() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".cursor/rules")) {
            panic!("cursor rules dir should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".cursor/rules/security.mdc"),
            "---\ndescription: Security\nalwaysApply: true\ncursorPriority: high\n---\nSecurity body.\n",
        ) {
            panic!("cursor rule should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("initial sync should succeed: {error}");
        }
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should read: {error}"),
        };
        assert!(
            lockfile
                .overrides
                .get(&entity_id("rule:security"))
                .is_some_and(|overrides| overrides.contains_key(&runtime_name("cursor")))
        );

        if let Err(error) = fs::remove_file(repo.join(".cursor/rules/security.mdc")) {
            panic!("cursor rule should be deleted: {error}");
        }
        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("delete sync should succeed: {error}");
        }
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should read: {error}"),
        };
        assert!(
            !lockfile
                .overrides
                .get(&entity_id("rule:security"))
                .is_some_and(|overrides| overrides.contains_key(&runtime_name("cursor")))
        );

        if let Err(error) = fs::write(repo.join(".ai/rules/security.mdc"), "# Updated\n") {
            panic!("canonical rule should be updated: {error}");
        }
        if let Err(error) = run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            panic!("canonical edit sync should succeed: {error}");
        }
        let cursor_rule = match fs::read_to_string(repo.join(".cursor/rules/security.mdc")) {
            Ok(contents) => contents,
            Err(error) => panic!("cursor rule should be emitted: {error}"),
        };
        assert!(!cursor_rule.contains("cursorPriority"));
        assert!(!cursor_rule.contains("alwaysApply"));
        assert!(cursor_rule.contains("# Updated"));
    }

    #[test]
    fn cursor_runtime_does_not_emit_duplicate_root_instructions() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".cursor/rules")) {
            panic!("cursor rules dir should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "Root instructions.\n") {
            panic!("root instructions should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let summary = match run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("sync should succeed: {error}"),
        };

        assert!(summary.changed);
        assert!(!repo.join(".cursor/rules/root.mdc").exists());
        assert!(!repo.join(".cursor/rules/instructions-root.mdc").exists());
    }

    #[test]
    fn copilot_scoped_instruction_preserves_native_metadata_in_overrides() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".github/instructions")) {
            panic!("copilot instructions dir should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".github/instructions/api.instructions.md"),
            "---\ndescription: API guidance\napplyTo:\n  - crates/api/**\nexcludeAgent: false\ncustomRouting: reviewer\n---\n# API\n",
        ) {
            panic!("copilot instruction should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("initial sync should succeed: {error}");
        }
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should read: {error}"),
        };
        let id = entity_id("instructions:scoped:api");
        let entity = lockfile
            .entities
            .get(&id)
            .unwrap_or_else(|| panic!("Copilot scoped instructions should be tracked"));
        assert_eq!(entity.scope.as_deref(), Some("crates/api/**"));
        assert_eq!(
            entity.locations[&location_key(".copilot")],
            PathBuf::from("../.github/instructions/api.instructions.md")
        );
        let canonical = match fs::read_to_string(repo.join(".ai/instructions/api.md")) {
            Ok(contents) => contents,
            Err(error) => panic!("canonical instruction should read: {error}"),
        };
        assert!(!canonical.contains("applyTo"));
        assert!(!canonical.contains("customRouting"));
        let overrides = &lockfile.overrides[&id][&runtime_name("copilot")].0;
        assert!(overrides.contains_key("applyTo"));
        assert_eq!(
            overrides.get("customRouting"),
            Some(&serde_json::json!("reviewer"))
        );

        if let Err(error) = fs::write(repo.join(".ai/instructions/api.md"), "# Updated API\n") {
            panic!("canonical instruction should be updated: {error}");
        }
        if let Err(error) = run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            panic!("canonical edit sync should succeed: {error}");
        }
        let copilot =
            match fs::read_to_string(repo.join(".github/instructions/api.instructions.md")) {
                Ok(contents) => contents,
                Err(error) => panic!("copilot instruction should read: {error}"),
            };
        assert!(copilot.contains("applyTo:"));
        assert!(copilot.contains("crates/api/**"));
        assert!(copilot.contains("customRouting: reviewer"));
        assert!(copilot.contains("# Updated API"));
        assert!(!repo.join(".copilot").exists());
    }

    #[test]
    fn gemini_context_commands_and_shared_skills_sync_through_core() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        for dir in [
            "packages/api",
            ".gemini/commands/git",
            ".agents/skills/shared-analysis",
        ] {
            if let Err(error) = fs::create_dir_all(repo.join(dir)) {
                panic!("fixture dirs should be created: {error}");
            }
        }
        if let Err(error) = fs::write(repo.join("GEMINI.md"), "# Root\n@./docs/style.md\n") {
            panic!("Gemini root context should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join("packages/api/GEMINI.md"), "# API\n") {
            panic!("Gemini scoped context should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".gemini/commands/git/commit.toml"),
            "description = \"Commit\"\nmode = \"builtin\"\nprompt = \"Draft commit for {{args}}.\"\n",
        ) {
            panic!("Gemini command should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".agents/skills/shared-analysis/SKILL.md"),
            "---\nname: shared-analysis\ndescription: Shared\nowner: platform\n---\n# Shared\n",
        ) {
            panic!("shared skill should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            panic!("Gemini sync should succeed: {error}");
        }
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should read: {error}"),
        };

        let scoped_id = entity_id("instructions:scoped:packages-api");
        let scoped = lockfile
            .entities
            .get(&scoped_id)
            .unwrap_or_else(|| panic!("Gemini scoped context should be tracked"));
        assert_eq!(scoped.scope.as_deref(), Some("packages/api/**"));
        assert_eq!(
            scoped.locations[&location_key(".gemini")],
            PathBuf::from("../packages/api/GEMINI.md")
        );
        let command_id = entity_id("command:git:commit");
        let command = lockfile
            .entities
            .get(&command_id)
            .unwrap_or_else(|| panic!("Gemini command should be tracked"));
        assert_eq!(
            command.locations[&location_key(".gemini")],
            PathBuf::from("commands/git/commit.toml")
        );
        let overrides = &lockfile.overrides[&command_id][&runtime_name("gemini")].0;
        assert_eq!(overrides.get("mode"), Some(&serde_json::json!("builtin")));
        let canonical_command = match fs::read_to_string(repo.join(".ai/commands/git/commit.md")) {
            Ok(contents) => contents,
            Err(error) => panic!("canonical command should read: {error}"),
        };
        assert!(canonical_command.contains("Draft commit for {{args}}."));
        assert!(!canonical_command.contains("mode"));
        assert!(
            lockfile
                .entities
                .contains_key(&entity_id("skill:shared-analysis"))
        );
        assert!(
            !lockfile
                .entities
                .contains_key(&entity_id("skill:shared-analysis-2"))
        );
        assert!(
            repo.join(".gemini/skills/shared-analysis/SKILL.md")
                .is_file()
        );

        if let Err(error) = fs::write(
            repo.join(".ai/instructions/packages-api.md"),
            "# Updated API\n",
        ) {
            panic!("canonical scoped context should be updated: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".ai/commands/git/commit.md"),
            "---\ndescription: Commit\n---\nUpdated commit prompt.\n",
        ) {
            panic!("canonical command should be updated: {error}");
        }
        if let Err(error) = run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            panic!("canonical edit sync should succeed: {error}");
        }
        let scoped_context = match fs::read_to_string(repo.join("packages/api/GEMINI.md")) {
            Ok(contents) => contents,
            Err(error) => panic!("Gemini scoped context should read: {error}"),
        };
        assert!(scoped_context.contains("# Updated API"));
        let command = match fs::read_to_string(repo.join(".gemini/commands/git/commit.toml")) {
            Ok(contents) => contents,
            Err(error) => panic!("Gemini command should read: {error}"),
        };
        assert!(command.contains("mode = \"builtin\""));
        assert!(command.contains("prompt = \"Updated commit prompt.\\n\""));
    }

    #[test]
    fn gemini_settings_do_not_enable_runtime_emit() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".gemini")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "# Root\n") {
            panic!("root instructions should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".gemini/settings.json"),
            r#"{"mcpServers":{"repo":{"command":"repo-tools"}}}"#,
        ) {
            panic!("Gemini settings should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            panic!("sync should succeed: {error}");
        }
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should read: {error}"),
        };

        assert!(!lockfile.adapters.contains_key(&runtime_name("gemini")));
        assert!(!repo.join("GEMINI.md").exists());
        assert!(!repo.join(".gemini/commands").exists());
        assert!(!repo.join(".gemini/skills").exists());
    }

    #[test]
    fn gemini_emits_canonical_toml_commands_without_wrapping() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".ai/commands")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("GEMINI.md"), "# Root\n") {
            panic!("Gemini context should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".ai/commands/deploy.toml"),
            "description = \"Deploy\"\nprompt = \"Deploy {{args}}\"\n",
        ) {
            panic!("canonical command should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            panic!("sync should succeed: {error}");
        }
        let command = match fs::read_to_string(repo.join(".gemini/commands/deploy.toml")) {
            Ok(contents) => contents,
            Err(error) => panic!("Gemini command should read: {error}"),
        };

        assert!(command.contains("description = \"Deploy\""));
        assert!(command.contains("prompt = \"Deploy {{args}}\""));
        assert!(!command.contains("prompt = \"prompt ="));
    }

    #[test]
    fn deferred_gemini_context_named_files_do_not_fail_sync_check() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".gemini/extensions/sample")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("GEMINI.md"), "# Root\n") {
            panic!("Gemini context should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".gemini/extensions/sample/GEMINI.md"),
            "# Deferred\n",
        ) {
            panic!("deferred context-named file should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let summary = match run_sync(
            &repo,
            SyncOptions {
                check: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("sync check should not fail on deferred diagnostics: {error}"),
        };

        assert!(summary.changed);
        assert!(!repo.join("agentmesh.lock").exists());
    }

    #[test]
    fn invalid_gemini_write_enabled_files_fail_sync_check_and_report_in_doctor() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        for dir in [".gemini/skills/broken", ".gemini/commands"] {
            if let Err(error) = fs::create_dir_all(repo.join(dir)) {
                panic!("fixture dirs should be created: {error}");
            }
        }
        if let Err(error) = fs::write(
            repo.join(".gemini/skills/broken/SKILL.md"),
            "---\nname: broken\ndescription: \"unterminated\n---\nBroken\n",
        ) {
            panic!("invalid skill should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".gemini/commands/missing-prompt.toml"),
            "description = \"Missing\"\n",
        ) {
            panic!("invalid command should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let error = run_sync(
            &repo,
            SyncOptions {
                check: true,
                ..SyncOptions::default()
            },
            &cache,
        )
        .expect_err("invalid Gemini files should fail sync check");
        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should succeed: {error}"),
        };

        assert!(matches!(error, PipelineError::EntityFormat { .. }));
        assert!(
            error.to_string().contains(".gemini/skills/broken/SKILL.md")
                || error
                    .to_string()
                    .contains(".gemini/commands/missing-prompt.toml")
        );
        assert!(report.findings.iter().any(|finding| {
            finding.starts_with("gemini_skill_invalid: .gemini/skills/broken/SKILL.md")
        }));
        assert!(report.findings.iter().any(|finding| {
            finding
                == "gemini_command_invalid: .gemini/commands/missing-prompt.toml: missing required prompt field"
        }));
    }

    #[test]
    fn invalid_copilot_write_enabled_files_fail_sync_check_and_report_in_doctor() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".github/instructions")) {
            panic!("copilot instructions dir should be created: {error}");
        }
        if let Err(error) = fs::create_dir_all(repo.join(".github/prompts")) {
            panic!("copilot prompts dir should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".github/instructions/missing.instructions.md"),
            "---\ndescription: Missing\n---\n# Missing\n",
        ) {
            panic!("invalid instruction should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".github/prompts/broken.prompt.md"),
            "---\ndescription: \"unterminated\n---\nBroken\n",
        ) {
            panic!("invalid prompt should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".github/instructions/multi.instructions.md"),
            "---\napplyTo:\n  - crates/api/**\n  - adapters/api/**\n---\n# Multi\n",
        ) {
            panic!("invalid multi-scope instruction should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let error = run_sync(
            &repo,
            SyncOptions {
                check: true,
                ..SyncOptions::default()
            },
            &cache,
        )
        .expect_err("invalid Copilot files should fail sync check");
        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should succeed: {error}"),
        };

        assert!(matches!(error, PipelineError::EntityFormat { .. }));
        assert!(
            error
                .to_string()
                .contains(".github/instructions/missing.instructions.md")
        );
        assert!(report.findings.iter().any(|finding| {
            finding
                == "copilot_instruction_invalid: .github/instructions/missing.instructions.md: missing applyTo frontmatter"
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.starts_with("copilot_prompt_invalid: .github/prompts/broken.prompt.md")
        }));
        assert!(report.findings.iter().any(|finding| {
            finding
                == "copilot_instruction_invalid: .github/instructions/multi.instructions.md: Copilot applyTo with multiple scopes cannot be represented losslessly"
        }));
    }

    #[test]
    fn doctor_reports_unsupported_capability_skip_details() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }

        let mut lockfile = Lockfile::empty();
        lockfile.entities.insert(
            entity_id("permission-policy:project"),
            entity_entry(
                EntityType::PermissionPolicy,
                hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ),
        );
        lockfile.adapters.insert(
            runtime_name("cursor"),
            AdapterDeclaration {
                mode: AdapterMode::Bundled,
                protocol_version: 2,
                entities: vec![EntityType::Instructions, EntityType::Rule],
                hooks: Vec::new(),
            },
        );
        if let Err(error) = write_lockfile(&repo, &lockfile) {
            panic!("lockfile should be written: {error}");
        }

        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should succeed: {error}"),
        };

        assert_eq!(report.health.capability_skips, 1);
        assert!(report.findings.iter().any(|finding| {
            finding
                == "capability_skip: runtime=cursor entity=permission-policy:project type=permission-policy fallback=warn locations=none"
        }));
    }

    #[test]
    fn doctor_reports_fail_fallback_without_enforcing_it() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join("agentmesh.config.yaml"),
            "fallbacks:\n  cursor:\n    permission-policy: fail\n",
        ) {
            panic!("config should be written: {error}");
        }

        let mut lockfile = Lockfile::empty();
        lockfile.entities.insert(
            entity_id("permission-policy:project"),
            entity_entry(
                EntityType::PermissionPolicy,
                hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ),
        );
        lockfile.adapters.insert(
            runtime_name("cursor"),
            AdapterDeclaration {
                mode: AdapterMode::Bundled,
                protocol_version: 2,
                entities: vec![EntityType::Instructions, EntityType::Rule],
                hooks: Vec::new(),
            },
        );
        if let Err(error) = write_lockfile(&repo, &lockfile) {
            panic!("lockfile should be written: {error}");
        }

        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should report strict fallback without failing: {error}"),
        };

        assert_eq!(report.health.capability_skips, 1);
        assert!(report.findings.iter().any(|finding| {
            finding
                == "capability_skip: runtime=cursor entity=permission-policy:project type=permission-policy fallback=fail locations=none"
        }));
    }

    #[test]
    fn no_drift_after_apply() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".claude")) {
            panic!("repo should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "Instructions\n") {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            panic!("sync should apply: {error}");
        }
        let check = match run_sync(
            &repo,
            SyncOptions {
                check: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("check should succeed: {error}"),
        };

        assert!(!check.changed);
    }

    #[test]
    fn pending_records_are_retained_when_drain_is_not_requested() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".claude")) {
            panic!("repo should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "Instructions\n") {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let summary = match run_sync(&repo, SyncOptions::default(), &cache) {
            Ok(summary) => summary,
            Err(error) => panic!("sync should apply: {error}"),
        };

        assert!(summary.pending_enqueued > 0);
        assert_eq!(summary.pending_drained, 0);
        let queue = PendingQueue::new(&cache.pending_syncs_dir);
        let pending = match queue.read_ready() {
            Ok(pending) => pending,
            Err(error) => panic!("pending queue should read: {error}"),
        };
        assert!(!pending.is_empty());
    }

    #[test]
    fn multi_file_skill_enqueues_one_directory_record() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".claude/skills/security-review/assets")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::create_dir_all(repo.join(".codex")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".claude/skills/security-review/SKILL.md"),
            "---\nname: security-review\n---\nBody\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".claude/skills/security-review/assets/prompt.md"),
            "Prompt\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let summary = match run_sync(
            &repo,
            SyncOptions {
                trigger: Some("claude-hook".to_string()),
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("sync should succeed: {error}"),
        };

        assert_eq!(summary.pending_enqueued, 1);
        let queue = PendingQueue::new(&cache.pending_syncs_dir);
        let pending = match queue.read_ready() {
            Ok(pending) => pending,
            Err(error) => panic!("pending queue should read: {error}"),
        };
        assert_eq!(pending.len(), 1);
        let record = &pending[0].record;
        assert_eq!(
            record.entity_root,
            std::path::PathBuf::from(".ai/skills/security-review")
        );
        assert!(record.changed_paths.contains(&std::path::PathBuf::from(
            ".ai/skills/security-review/SKILL.md"
        )));
        assert!(record.changed_paths.contains(&std::path::PathBuf::from(
            ".ai/skills/security-review/assets/prompt.md"
        )));
    }

    #[test]
    fn await_drain_deletes_only_verified_records() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".claude")) {
            panic!("repo should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "Instructions\n") {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        let summary = match run_sync(
            &repo,
            SyncOptions {
                await_drain: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("sync should apply: {error}"),
        };

        assert!(summary.pending_enqueued > 0);
        assert!(summary.pending_drained > 0);
        let queue = PendingQueue::new(&cache.pending_syncs_dir);
        let pending = match queue.read_ready() {
            Ok(pending) => pending,
            Err(error) => panic!("pending queue should read: {error}"),
        };
        assert!(pending.is_empty());
    }

    #[test]
    fn drain_keeps_unverified_records_queued() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "Instructions\n") {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);
        let queue = PendingQueue::new(&cache.pending_syncs_dir);
        let record = PendingSyncRecord {
            pending_id: PendingQueue::new_pending_id(),
            source_runtime: runtime_name("core"),
            action: PendingAction::Write,
            entity_type: Some(EntityType::Instructions),
            entity_root: std::path::PathBuf::from("AGENTS.md"),
            changed_paths: vec![std::path::PathBuf::from("AGENTS.md")],
            rename_from: None,
            content_hashes: std::collections::BTreeMap::from([(
                std::path::PathBuf::from("AGENTS.md"),
                hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            )]),
            mtime: "unix-1".to_string(),
            trigger: "test".to_string(),
            created_at: "unix-1".to_string(),
            attempts: 0,
            last_error: None,
        };
        if let Err(error) = queue.enqueue(&record) {
            panic!("pending record should enqueue: {error}");
        }

        let summary = match run_sync(
            &repo,
            SyncOptions {
                drain_pending: true,
                ..SyncOptions::default()
            },
            &cache,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("drain should complete: {error}"),
        };

        assert_eq!(summary.pending_drained, 0);
        let pending = match queue.read_ready() {
            Ok(pending) => pending,
            Err(error) => panic!("pending queue should read: {error}"),
        };
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].record.attempts, 1);
    }

    #[test]
    fn drain_keeps_records_queued_when_adapter_emit_fails() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".ai/skills/demo")) {
            panic!("fixture dirs should be created: {error}");
        }
        let canonical_path = repo.join(".ai/skills/demo/SKILL.md");
        if let Err(error) = fs::write(&canonical_path, "---\nname: demo\n---\nBody\n") {
            panic!("fixture should be written: {error}");
        }
        let canonical_hash = match crate::state::sha256_file(&canonical_path) {
            Ok(hash) => hash,
            Err(error) => panic!("canonical file should hash: {error}"),
        };
        let id = entity_id("skill:demo");
        let mut lockfile = Lockfile::empty();
        let mut entity = entity_entry(EntityType::Skill, canonical_hash.clone());
        entity.locations.insert(
            location_key(".ai"),
            std::path::PathBuf::from("skills/demo/SKILL.md"),
        );
        entity
            .emitted_native_sha256
            .insert(location_key(".ai"), canonical_hash.clone());
        lockfile.entities.insert(id.clone(), entity);
        lockfile.adapters.insert(
            runtime_name("failing"),
            AdapterDeclaration {
                mode: AdapterMode::Bundled,
                protocol_version: 1,
                entities: vec![EntityType::Skill],
                hooks: vec![HookKind::PostToolUse],
            },
        );
        if let Err(error) = write_lockfile(&repo, &lockfile) {
            panic!("lockfile should write: {error}");
        }
        let cache = cache_for(&temp, &repo);
        let queue = PendingQueue::new(&cache.pending_syncs_dir);
        let record = PendingSyncRecord {
            pending_id: PendingQueue::new_pending_id(),
            source_runtime: runtime_name("core"),
            action: PendingAction::Write,
            entity_type: Some(EntityType::Skill),
            entity_root: std::path::PathBuf::from(".ai/skills/demo"),
            changed_paths: vec![std::path::PathBuf::from(".ai/skills/demo/SKILL.md")],
            rename_from: None,
            content_hashes: std::collections::BTreeMap::from([(
                std::path::PathBuf::from(".ai/skills/demo/SKILL.md"),
                canonical_hash,
            )]),
            mtime: "unix-1".to_string(),
            trigger: "test".to_string(),
            created_at: "unix-1".to_string(),
            attempts: 0,
            last_error: None,
        };
        if let Err(error) = queue.enqueue(&record) {
            panic!("pending record should enqueue: {error}");
        }
        let registry = TestAdapterRegistry {
            fail_emit: true,
            ..TestAdapterRegistry::default()
        };

        let summary = match super::run_sync(
            &repo,
            SyncOptions {
                drain_pending: true,
                ..SyncOptions::default()
            },
            &cache,
            &registry,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("drain should complete: {error}"),
        };

        assert_eq!(summary.pending_drained, 0);
        let pending = match queue.read_ready() {
            Ok(pending) => pending,
            Err(error) => panic!("pending queue should read: {error}"),
        };
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].record.attempts, 1);
    }

    #[test]
    fn divergent_markdown_changes_merge_against_unchanged_runtime() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        for dir in [
            ".ai/skills/merge-demo",
            ".claude/skills/merge-demo",
            ".codex/skills/merge-demo",
        ] {
            if let Err(error) = fs::create_dir_all(repo.join(dir)) {
                panic!("fixture dirs should be created: {error}");
            }
        }
        let ancestor = "---\nname: merge-demo\ndescription: Base\n---\nLine one\nLine two\n";
        for path in [
            ".ai/skills/merge-demo/SKILL.md",
            ".claude/skills/merge-demo/SKILL.md",
            ".codex/skills/merge-demo/SKILL.md",
        ] {
            if let Err(error) = fs::write(repo.join(path), ancestor) {
                panic!("fixture should be written: {error}");
            }
        }
        let cache = cache_for(&temp, &repo);
        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("initial sync should succeed: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".ai/skills/merge-demo/SKILL.md"),
            "---\nname: merge-demo\ndescription: Updated\n---\nLine one\nLine two\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".claude/skills/merge-demo/SKILL.md"),
            "---\nname: merge-demo\ndescription: Base\n---\nLine one\nLine two\nLine three\n",
        ) {
            panic!("fixture should be written: {error}");
        }

        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("merge sync should succeed: {error}");
        }

        let merged = match fs::read_to_string(repo.join(".ai/skills/merge-demo/SKILL.md")) {
            Ok(contents) => contents,
            Err(error) => panic!("merged file should read: {error}"),
        };
        assert!(merged.contains("description: Updated"));
        assert!(merged.contains("Line three"));
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should be readable: {error}"),
        };
        assert_eq!(
            lockfile.entities[&entity_id("skill:merge-demo")].pending_conflict_resolution,
            None
        );
    }

    #[test]
    fn overlapping_markdown_changes_preserve_losing_version() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        for dir in [
            ".ai/skills/conflict-demo",
            ".claude/skills/conflict-demo",
            ".codex/skills/conflict-demo",
        ] {
            if let Err(error) = fs::create_dir_all(repo.join(dir)) {
                panic!("fixture dirs should be created: {error}");
            }
        }
        let ancestor = "---\nname: conflict-demo\n---\nReview carefully.\n";
        for path in [
            ".ai/skills/conflict-demo/SKILL.md",
            ".claude/skills/conflict-demo/SKILL.md",
            ".codex/skills/conflict-demo/SKILL.md",
        ] {
            if let Err(error) = fs::write(repo.join(path), ancestor) {
                panic!("fixture should be written: {error}");
            }
        }
        let cache = cache_for(&temp, &repo);
        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("initial sync should succeed: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".ai/skills/conflict-demo/SKILL.md"),
            "---\nname: conflict-demo\n---\nReview for reliability.\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".claude/skills/conflict-demo/SKILL.md"),
            "---\nname: conflict-demo\n---\nReview for security.\n",
        ) {
            panic!("fixture should be written: {error}");
        }

        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("conflict sync should succeed: {error}");
        }

        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should be readable: {error}"),
        };
        assert_eq!(
            lockfile.entities[&entity_id("skill:conflict-demo")].pending_conflict_resolution,
            Some(true)
        );
        let conflict_dir = crate::state::conflict_entity_dir(
            &cache.conflicts_dir,
            &entity_id("skill:conflict-demo"),
        );
        let preserved = match fs::read_dir(&conflict_dir) {
            Ok(entries) => entries.count(),
            Err(error) => panic!("conflict dir should be readable: {error}"),
        };
        assert!(preserved > 0);
    }

    #[test]
    fn markdown_backed_expanded_entities_merge_against_unchanged_view() {
        let cases = [
            (EntityType::Rule, "rule:security", ".ai/rules/security.md"),
            (
                EntityType::Prompt,
                "prompt:release-notes",
                ".ai/prompts/release-notes.md",
            ),
            (
                EntityType::Command,
                "command:git:commit",
                ".ai/commands/git/commit.md",
            ),
        ];

        for (entity_type, id, path) in cases {
            let temp = match tempfile::tempdir() {
                Ok(temp) => temp,
                Err(error) => panic!("tempdir should be available: {error}"),
            };
            let repo = temp.path().join("repo");
            let cache = cache_for(&temp, &repo);
            let old_hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let left_hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
            let right_hash = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
            let ancestor = "---\ndescription: Base\n---\nLine one\nLine two\n";
            let mut previous = entity_entry(entity_type, hash(old_hash));
            previous.emitted_native_sha256 = BTreeMap::from([
                (location_key(".ai"), hash(old_hash)),
                (location_key(".claude"), hash(old_hash)),
                (location_key(".codex"), hash(old_hash)),
            ]);
            let views = BTreeMap::from([
                (
                    location_key(".ai"),
                    test_view(
                        entity_type,
                        ".ai",
                        path,
                        "---\ndescription: Updated\n---\nLine one\nLine two\n",
                        left_hash,
                        2,
                    ),
                ),
                (
                    location_key(".claude"),
                    test_view(
                        entity_type,
                        ".claude",
                        path,
                        "---\ndescription: Base\n---\nLine one\nLine two\nLine three\n",
                        right_hash,
                        3,
                    ),
                ),
                (
                    location_key(".codex"),
                    test_view(entity_type, ".codex", path, ancestor, old_hash, 1),
                ),
            ]);

            let decision = match super::resolve_canonical_contents(
                &cache,
                &entity_id(id),
                entity_type,
                &views,
                Some(&previous),
                None,
                true,
            ) {
                Ok(decision) => decision,
                Err(error) => panic!("expanded markdown entity should merge: {error}"),
            };
            let merged = match String::from_utf8(decision.contents) {
                Ok(merged) => merged,
                Err(error) => panic!("merged contents should be UTF-8: {error}"),
            };

            assert!(merged.contains("description: Updated"));
            assert!(merged.contains("Line three"));
            assert!(!decision.pending_conflict_resolution);
        }
    }

    #[test]
    fn config_backed_entities_tiebreak_and_preserve_losing_versions() {
        let cases = [
            (
                EntityType::Command,
                "command:deploy",
                ".ai/commands/deploy.toml",
            ),
            (
                EntityType::Hook,
                "hook:gemini-project",
                ".ai/hooks/gemini-project.json",
            ),
            (
                EntityType::McpBinding,
                "mcp-binding:gemini-project",
                ".ai/mcp-bindings/gemini-project.json",
            ),
            (
                EntityType::PermissionPolicy,
                "permission-policy:gemini-project",
                ".ai/permission-policies/gemini-project.json",
            ),
        ];

        for (entity_type, id, path) in cases {
            let temp = match tempfile::tempdir() {
                Ok(temp) => temp,
                Err(error) => panic!("tempdir should be available: {error}"),
            };
            let repo = temp.path().join("repo");
            let cache = cache_for(&temp, &repo);
            let old_hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
            let left_hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
            let right_hash = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
            let mut previous = entity_entry(entity_type, hash(old_hash));
            previous.emitted_native_sha256 = BTreeMap::from([
                (location_key(".ai"), hash(old_hash)),
                (location_key(".claude"), hash(old_hash)),
                (location_key(".codex"), hash(old_hash)),
            ]);
            let views = BTreeMap::from([
                (
                    location_key(".ai"),
                    test_view(
                        entity_type,
                        ".ai",
                        path,
                        "{\"value\":\"left\"}\n",
                        left_hash,
                        2,
                    ),
                ),
                (
                    location_key(".claude"),
                    test_view(
                        entity_type,
                        ".claude",
                        path,
                        "{\"value\":\"right\"}\n",
                        right_hash,
                        4,
                    ),
                ),
                (
                    location_key(".codex"),
                    test_view(
                        entity_type,
                        ".codex",
                        path,
                        "{\"value\":\"old\"}\n",
                        old_hash,
                        1,
                    ),
                ),
            ]);

            let decision = match super::resolve_canonical_contents(
                &cache,
                &entity_id(id),
                entity_type,
                &views,
                Some(&previous),
                None,
                true,
            ) {
                Ok(decision) => decision,
                Err(error) => panic!("config entity should tiebreak: {error}"),
            };
            let chosen = match String::from_utf8(decision.contents) {
                Ok(chosen) => chosen,
                Err(error) => panic!("chosen contents should be UTF-8: {error}"),
            };
            let conflict_dir =
                crate::state::conflict_entity_dir(&cache.conflicts_dir, &entity_id(id));
            let preserved = match fs::read_dir(&conflict_dir) {
                Ok(entries) => entries.count(),
                Err(error) => panic!("conflict dir should be readable: {error}"),
            };

            assert!(chosen.contains("right"));
            assert!(decision.pending_conflict_resolution);
            assert!(preserved > 0);
        }
    }

    #[test]
    fn config_backed_command_keeps_canonical_extension() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".ai/commands")) {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".ai/commands/deploy.toml"),
            "command = \"deploy\"\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);

        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("sync should preserve command extension: {error}");
        }

        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should be readable: {error}"),
        };
        assert_eq!(
            lockfile.entities[&entity_id("command:deploy")].locations[&location_key(".ai")],
            PathBuf::from("commands/deploy.toml")
        );
        assert!(!repo.join(".ai/commands/deploy.md").exists());
    }

    #[test]
    fn chooses_available_view_for_unlisted_runtime_locations() {
        let view = test_view(
            EntityType::Command,
            ".gemini",
            ".gemini/commands/review.toml",
            "prompt = \"review\"\n",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            1,
        );
        let views = BTreeMap::from([(location_key(".gemini"), view)]);

        let chosen = match super::choose_canonical_view(&views) {
            Some(chosen) => chosen,
            None => panic!("available view should be chosen"),
        };

        assert_eq!(chosen.location, location_key(".gemini"));
        assert_eq!(chosen.canonical_contents, b"prompt = \"review\"\n");
    }

    #[test]
    fn deduplicates_shared_skill_imports() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) =
            fs::create_dir_all(repo.join(".agents/skills/shared-analysis/references"))
        {
            panic!("fixture dirs should be created: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".agents/skills/shared-analysis/SKILL.md"),
            "---\nname: shared-analysis\n---\nShared body\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".agents/skills/shared-analysis/references/notes.md"),
            "Notes\n",
        ) {
            panic!("support file should be written: {error}");
        }
        let imported = ImportedEntity {
            id: "skill:shared-analysis".to_string(),
            entity_type: EntityType::Skill,
            scope: None,
            canonical_path: PathBuf::from(".ai/skills/shared-analysis/SKILL.md"),
            files: BTreeMap::from([
                (
                    PathBuf::from("SKILL.md"),
                    EntityFile::utf8("---\nname: shared-analysis\n---\nShared body\n"),
                ),
                (
                    PathBuf::from("references/notes.md"),
                    EntityFile::utf8("Notes\n"),
                ),
            ]),
            frontmatter: BTreeMap::new(),
            canonical_sha256: "unused".to_string(),
            source_path: PathBuf::from(".agents/skills/shared-analysis/SKILL.md"),
            source_mtime: "unix-1".to_string(),
        };
        let registry = TestAdapterRegistry {
            imports: vec![imported],
            emitted: RefCell::default(),
            present: true,
            fail_emit: false,
        };
        let cache = cache_for(&temp, &repo);

        let summary = match super::run_sync(
            &repo,
            SyncOptions {
                trigger: Some("test".to_string()),
                ..SyncOptions::default()
            },
            &cache,
            &registry,
        ) {
            Ok(summary) => summary,
            Err(error) => panic!("sync should deduplicate shared skill: {error}"),
        };

        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should be readable: {error}"),
        };
        let id = entity_id("skill:shared-analysis");
        assert!(lockfile.entities.contains_key(&id));
        assert!(
            !lockfile
                .entities
                .contains_key(&entity_id("skill:shared-analysis-2"))
        );
        assert_eq!(lockfile.entities.len(), 1);
        assert_eq!(
            lockfile.entities[&id].locations[&location_key(".agents")],
            PathBuf::from("skills/shared-analysis/SKILL.md")
        );
        assert!(repo.join(".ai/skills/shared-analysis/SKILL.md").exists());
        assert!(
            repo.join(".ai/skills/shared-analysis/references/notes.md")
                .exists()
        );
        assert_eq!(summary.pending_enqueued, 1);
        let pending = match PendingQueue::new(&cache.pending_syncs_dir).read_ready() {
            Ok(pending) => pending,
            Err(error) => panic!("pending queue should read: {error}"),
        };
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn configuration_entities_emit_payloads_as_data() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        let cache = cache_for(&temp, &repo);
        if let Err(error) = cache.ensure_dirs() {
            panic!("cache dirs should be created: {error}");
        }
        let runtime = runtime_name("gemini");
        let cases = [
            (
                EntityType::Command,
                "command:deploy",
                ".ai/commands/deploy.toml",
                "commands/deploy.toml",
                "command = \"sh -c 'rm -rf target'\"\n",
            ),
            (
                EntityType::Hook,
                "hook:gemini-project",
                ".ai/hooks/gemini-project.json",
                "hooks/gemini-project.json",
                "{\"command\":\"sh -c 'echo hook'\"}\n",
            ),
            (
                EntityType::McpBinding,
                "mcp-binding:gemini-project",
                ".ai/mcp-bindings/gemini-project.json",
                "mcp-bindings/gemini-project.json",
                "{\"server\":{\"command\":\"node server.js\"}}\n",
            ),
            (
                EntityType::PermissionPolicy,
                "permission-policy:gemini-project",
                ".ai/permission-policies/gemini-project.json",
                "permission-policies/gemini-project.json",
                "{\"allow\":[\"shell:*\"]}\n",
            ),
        ];

        for (_, _, repo_path, _, contents) in cases {
            let path = repo.join(repo_path);
            if let Some(parent) = path.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    panic!("fixture dirs should be created: {error}");
                }
            }
            if let Err(error) = fs::write(path, contents) {
                panic!("fixture should be written: {error}");
            }
        }

        let registry = TestAdapterRegistry {
            imports: Vec::new(),
            emitted: RefCell::default(),
            present: false,
            fail_emit: false,
        };
        for (entity_type, id, _, lockfile_path, contents) in cases {
            let mut lockfile = Lockfile::empty();
            let mut entity = entity_entry(
                entity_type,
                hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            );
            entity
                .locations
                .insert(location_key(".ai"), PathBuf::from(lockfile_path));
            lockfile.entities.insert(entity_id(id), entity);
            lockfile.adapters.insert(
                runtime.clone(),
                AdapterDeclaration {
                    mode: AdapterMode::Bundled,
                    protocol_version: 2,
                    entities: vec![entity_type],
                    hooks: Vec::new(),
                },
            );

            let outcome = match super::emit_entity_to_runtime(
                &repo,
                &mut lockfile,
                &AgentmeshConfig::default(),
                &entity_id(id),
                &runtime,
                &registry,
            ) {
                Ok(outcome) => outcome,
                Err(error) => panic!("emit should preserve payload as data: {error}"),
            };

            assert_eq!(outcome.files_written, 0);
            let emitted = registry.emitted.borrow();
            let Some(request) = emitted.last() else {
                panic!("emit request should be recorded");
            };
            let Some(entity) = request.entities.first() else {
                panic!("emit request should contain an entity");
            };
            let Some(file) = entity.files.values().next() else {
                panic!("emit entity should contain a file");
            };
            assert_eq!(entity.entity_type, entity_type);
            assert_eq!(entity_file_text(file), contents);
            assert!(!cache.hook_ownership_json.exists());
        }
    }

    #[test]
    fn fallback_behaviors_cover_configuration_entities() {
        let config_entities = [
            (EntityType::Command, "command:deploy"),
            (EntityType::Hook, "hook:gemini-project"),
            (EntityType::McpBinding, "mcp-binding:gemini-project"),
            (
                EntityType::PermissionPolicy,
                "permission-policy:gemini-project",
            ),
        ];

        for (fallback, expected_diagnostic_skips) in [
            (CapabilityFallback::Skip, 0, false),
            (CapabilityFallback::Warn, config_entities.len(), false),
            (CapabilityFallback::RenderAsDoc, config_entities.len(), true),
            (CapabilityFallback::Fail, config_entities.len(), true),
        ]
        .into_iter()
        .map(|(fallback, expected, _)| (fallback, expected))
        {
            let mut lockfile = Lockfile::empty();
            for (entity_type, id) in config_entities {
                lockfile.entities.insert(
                    entity_id(id),
                    entity_entry(
                        entity_type,
                        hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                    ),
                );
            }
            lockfile.adapters.insert(
                runtime_name("codex"),
                AdapterDeclaration {
                    mode: AdapterMode::Bundled,
                    protocol_version: 2,
                    entities: Vec::new(),
                    hooks: Vec::new(),
                },
            );
            let mut config = AgentmeshConfig::default();
            config.fallbacks.insert(
                runtime_name("codex"),
                config_entities
                    .iter()
                    .map(|(entity_type, _)| (entity_type.as_str().to_string(), fallback))
                    .collect(),
            );

            let enforce = capability_skip_report_for_lockfile(
                &lockfile,
                &config,
                CapabilityReportMode::Enforce,
            );
            match fallback {
                CapabilityFallback::Skip | CapabilityFallback::Warn => {
                    let report =
                        enforce.unwrap_or_else(|error| panic!("fallback should not fail: {error}"));
                    assert_eq!(report.skipped, expected_diagnostic_skips);
                    assert_eq!(report.findings.len(), expected_diagnostic_skips);
                    let skipped = capability_skip_count_for_lockfile(&lockfile, &config)
                        .unwrap_or_else(|error| {
                            panic!("count helper should match report: {error}")
                        });
                    assert_eq!(skipped, expected_diagnostic_skips);
                }
                CapabilityFallback::RenderAsDoc => assert!(matches!(
                    enforce,
                    Err(super::PipelineError::RenderAsDocUnsupported { .. })
                )),
                CapabilityFallback::Fail => assert!(matches!(
                    enforce,
                    Err(super::PipelineError::CapabilityMismatch { .. })
                )),
            }

            let diagnostic = capability_skip_report_for_lockfile(
                &lockfile,
                &config,
                CapabilityReportMode::Diagnostic,
            )
            .unwrap_or_else(|error| panic!("diagnostic report should not fail: {error}"));
            assert_eq!(diagnostic.skipped, expected_diagnostic_skips);
            assert_eq!(diagnostic.findings.len(), expected_diagnostic_skips);
            for finding in &diagnostic.findings {
                assert_eq!(finding.fallback, fallback);
            }
        }
    }

    #[test]
    fn render_as_doc_fallback_rejects_unsupported_emit_without_writing() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        let runtime = runtime_name("codex");
        let id = entity_id("command:deploy");
        let mut lockfile = Lockfile::empty();
        lockfile.entities.insert(
            id.clone(),
            entity_entry(
                EntityType::Command,
                hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ),
        );
        lockfile.adapters.insert(
            runtime.clone(),
            AdapterDeclaration {
                mode: AdapterMode::Bundled,
                protocol_version: 2,
                entities: vec![EntityType::Instructions],
                hooks: Vec::new(),
            },
        );
        let mut config = AgentmeshConfig::default();
        config.fallbacks.insert(
            runtime.clone(),
            BTreeMap::from([(
                EntityType::Command.as_str().to_string(),
                CapabilityFallback::RenderAsDoc,
            )]),
        );
        let registry = TestAdapterRegistry {
            imports: Vec::new(),
            emitted: RefCell::default(),
            present: false,
            fail_emit: false,
        };

        let result =
            super::emit_entity_to_runtime(&repo, &mut lockfile, &config, &id, &runtime, &registry);

        assert!(matches!(
            result,
            Err(super::PipelineError::RenderAsDocUnsupported { .. })
        ));
        assert!(registry.emitted.borrow().is_empty());
    }

    #[test]
    fn integrity_verification_rejects_changed_pin() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        let cache = cache_for(&temp, &repo);
        if let Err(error) = cache.ensure_dirs() {
            panic!("cache dirs should be created: {error}");
        }
        if let Err(error) = super::write_integrity(&repo, &cache) {
            panic!("integrity pin should write: {error}");
        }
        let mut pin = match crate::state::read_integrity_pin(&cache.integrity_json) {
            Ok(pin) => pin,
            Err(error) => panic!("integrity pin should read: {error}"),
        };
        pin.binary_sha256 =
            hash("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        if let Err(error) = crate::state::write_integrity_pin(&cache.integrity_json, &pin) {
            panic!("integrity pin should write: {error}");
        }

        let error = super::verify_integrity(&repo, &cache).err();

        assert!(matches!(
            error,
            Some(super::PipelineError::IntegrityMismatch { .. })
        ));
    }

    #[test]
    fn init_integrity_write_refreshes_changed_pin() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        let cache = cache_for(&temp, &repo);
        if let Err(error) = cache.ensure_dirs() {
            panic!("cache dirs should be created: {error}");
        }
        if let Err(error) = super::write_integrity(&repo, &cache) {
            panic!("integrity pin should write: {error}");
        }
        let mut pin = match crate::state::read_integrity_pin(&cache.integrity_json) {
            Ok(pin) => pin,
            Err(error) => panic!("integrity pin should read: {error}"),
        };
        pin.binary_sha256 =
            hash("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        if let Err(error) = crate::state::write_integrity_pin(&cache.integrity_json, &pin) {
            panic!("integrity pin should write: {error}");
        }

        if let Err(error) = super::write_integrity(&repo, &cache) {
            panic!("integrity pin should refresh: {error}");
        }

        if let Err(error) = super::verify_integrity(&repo, &cache) {
            panic!("refreshed integrity pin should verify: {error}");
        }
    }

    #[test]
    fn integrity_verification_creates_missing_pin() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        let cache = cache_for(&temp, &repo);
        if let Err(error) = cache.ensure_dirs() {
            panic!("cache dirs should be created: {error}");
        }

        if let Err(error) = super::verify_integrity(&repo, &cache) {
            panic!("missing integrity pin should be created: {error}");
        }

        assert!(cache.integrity_json.exists());
    }

    #[test]
    fn ack_clears_pending_conflict_flag() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "Instructions\n") {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);
        let mut plan =
            match build_sync_plan(&repo, Lockfile::empty(), &cache, PlanOptions::default()) {
                Ok(plan) => plan,
                Err(error) => panic!("plan should build: {error}"),
            };
        let id = entity_id("instructions:root");
        if let Some(entity) = plan.lockfile.entities.get_mut(&id) {
            entity.pending_conflict_resolution = Some(true);
        }
        if let Err(error) = crate::lockfile::write_lockfile(&repo, &plan.lockfile) {
            panic!("lockfile should write: {error}");
        }

        if let Err(error) = ack(&repo, &id) {
            panic!("ack should succeed: {error}");
        }

        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should read: {error}"),
        };
        assert_eq!(lockfile.entities[&id].pending_conflict_resolution, None);
        if let Err(error) = cache.ensure_dirs() {
            panic!("cache dirs should be creatable: {error}");
        }
    }

    #[test]
    fn restore_uses_latest_preserved_version() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(repo.join(".claude")) {
            panic!("repo should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "Current\n") {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);
        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("sync should apply: {error}");
        }
        let id = entity_id("instructions:root");
        if let Err(error) = preserve_losing_version(
            &cache.conflicts_dir,
            &id,
            &runtime_name("claude"),
            "unix-9",
            "Older\n",
        ) {
            panic!("preserved version should write: {error}");
        }
        if let Err(error) = preserve_losing_version(
            &cache.conflicts_dir,
            &id,
            &runtime_name("claude"),
            "unix-10",
            "Restored\n",
        ) {
            panic!("preserved version should write: {error}");
        }

        let summary =
            match restore_with_cache(&repo, &cache, &id, runtime_name("claude"), None, false) {
                Ok(summary) => summary,
                Err(error) => panic!("restore should succeed: {error}"),
            };

        let restored = match fs::read_to_string(repo.join("AGENTS.md")) {
            Ok(restored) => restored,
            Err(error) => panic!("restored file should read: {error}"),
        };
        assert_eq!(restored, "Restored\n");
        let emitted = match fs::read_to_string(repo.join("CLAUDE.md")) {
            Ok(restored) => restored,
            Err(error) => panic!("emitted file should read: {error}"),
        };
        assert_eq!(emitted, "Restored\n");
        assert!(summary.preserved_version.ends_with("claude-unix-10.md"));
        assert!(summary.files_written >= 2);
    }

    #[test]
    fn lockfile_repo_relative_paths_reject_nested_parent_components() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }

        let valid = match path_from_lockfile(
            &repo,
            &location_key(".copilot"),
            Path::new("../.github/instructions/api.instructions.md"),
        ) {
            Ok(path) => path,
            Err(error) => panic!("valid repo-relative lockfile path should resolve: {error}"),
        };
        assert_eq!(valid, repo.join(".github/instructions/api.instructions.md"));

        let valid = match path_from_lockfile(&repo, &location_key(".ai"), Path::new("../AGENTS.md"))
        {
            Ok(path) => path,
            Err(error) => panic!("canonical root instruction path should resolve: {error}"),
        };
        assert_eq!(valid, repo.join("AGENTS.md"));

        let valid = match path_from_lockfile(
            &repo,
            &location_key(".codex"),
            Path::new("../packages/api/AGENTS.md"),
        ) {
            Ok(path) => path,
            Err(error) => panic!("Codex nested instruction path should resolve: {error}"),
        };
        assert_eq!(valid, repo.join("packages/api/AGENTS.md"));

        let valid = match path_from_lockfile(
            &repo,
            &location_key(".gemini"),
            Path::new("../packages/api/GEMINI.md"),
        ) {
            Ok(path) => path,
            Err(error) => panic!("Gemini nested context path should resolve: {error}"),
        };
        assert_eq!(valid, repo.join("packages/api/GEMINI.md"));

        let error = path_from_lockfile(
            &repo,
            &location_key(".copilot"),
            Path::new("../../outside.md"),
        )
        .expect_err("nested parent components should be rejected");
        assert!(matches!(error, PipelineError::EntityFormat { .. }));
        assert!(
            error
                .to_string()
                .contains("unsafe repo-relative lockfile path")
        );

        let error = path_from_lockfile(
            &repo,
            &location_key(".ai"),
            Path::new("../.git/hooks/pre-commit"),
        )
        .expect_err("canonical root location should reject arbitrary repo-relative paths");
        assert!(matches!(error, PipelineError::EntityFormat { .. }));
        assert!(
            error
                .to_string()
                .contains("repo-relative lockfile path is not allowed")
        );

        let error = path_from_lockfile(
            &repo,
            &location_key(".codex"),
            Path::new("../.git/hooks/AGENTS.md"),
        )
        .expect_err("hidden repository roots should be rejected");
        assert!(matches!(error, PipelineError::EntityFormat { .. }));
        assert!(
            error
                .to_string()
                .contains("repo-relative lockfile path is not allowed")
        );

        let error = path_from_lockfile(
            &repo,
            &location_key(".codex"),
            Path::new("../packages/.codex/AGENTS.md"),
        )
        .expect_err("nested runtime directories should be rejected for Codex");
        assert!(matches!(error, PipelineError::EntityFormat { .. }));
        assert!(
            error
                .to_string()
                .contains("repo-relative lockfile path is not allowed")
        );

        let error = path_from_lockfile(
            &repo,
            &location_key(".gemini"),
            Path::new("../.git/hooks/GEMINI.md"),
        )
        .expect_err("hidden repository roots should be rejected for Gemini");
        assert!(matches!(error, PipelineError::EntityFormat { .. }));
        assert!(
            error
                .to_string()
                .contains("repo-relative lockfile path is not allowed")
        );

        for path in [
            "../packages/.git/hooks/GEMINI.md",
            "../packages/.gemini/GEMINI.md",
            "../packages/.secrets/GEMINI.md",
        ] {
            let error = path_from_lockfile(&repo, &location_key(".gemini"), Path::new(path))
                .expect_err("nested hidden directories should be rejected for Gemini");
            assert!(matches!(error, PipelineError::EntityFormat { .. }));
            assert!(
                error
                    .to_string()
                    .contains("repo-relative lockfile path is not allowed")
            );
        }

        let error = path_from_lockfile(
            &repo,
            &location_key(".copilot"),
            Path::new("../.git/hooks/pre-commit"),
        )
        .expect_err("arbitrary repo-relative paths should be rejected");
        assert!(matches!(error, PipelineError::EntityFormat { .. }));
        assert!(
            error
                .to_string()
                .contains("repo-relative lockfile path is not allowed")
        );
    }

    #[test]
    fn doctor_reports_core_counts() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "Instructions\n") {
            panic!("fixture should be written: {error}");
        }
        let cache = cache_for(&temp, &repo);
        if let Err(error) = run_sync(&repo, SyncOptions::default(), &cache) {
            panic!("sync should apply: {error}");
        }

        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should succeed: {error}"),
        };

        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding == "entities: 1")
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.starts_with("integrity: "))
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding == "network: disabled")
        );
        assert!(
            report
                .findings
                .iter()
                .any(|finding| finding.starts_with("adapter_coverage_instructions:"))
        );
        assert_eq!(report.health.entities_out_of_sync, 0);
        assert_eq!(report.health.pending_conflicts, 0);
        assert_eq!(report.health.pending_syncs, 0);
        assert_eq!(report.health.failed_pending_syncs, 0);
        assert_eq!(report.health.capability_skips, 0);
        assert_eq!(report.health.lockfile_privacy_warnings, 0);
        assert!(
            report
                .findings
                .iter()
                .all(|finding| !finding.starts_with("lockfile_privacy"))
        );
    }

    #[test]
    fn doctor_reports_codex_read_only_and_deferred_surfaces() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        for dir in [".codex/rules", ".codex/prompts", ".codex/commands"] {
            if let Err(error) = fs::create_dir_all(repo.join(dir)) {
                panic!("fixture dirs should be created: {error}");
            }
        }
        if let Err(error) = fs::write(
            repo.join(".codex/config.toml"),
            "[hooks.PostToolUse]\ncommand = \"npm test\"\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".codex/rules/strict.rules"),
            "deny = [\"network\"]\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join(".codex/prompts/release.md"), "# Release\n") {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join(".codex/commands/review.md"), "# Review\n") {
            panic!("fixture should be written: {error}");
        }

        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should succeed: {error}"),
        };

        for expected in [
            "codex_inline_config_hooks: read-only diagnostic",
            "codex_experimental_rules: read-only diagnostic",
            "codex_custom_prompts: deferred",
            "codex_project_commands: deferred",
        ] {
            assert!(
                report
                    .findings
                    .iter()
                    .any(|finding| finding.starts_with(expected)),
                "missing doctor finding starting with {expected}; findings: {:?}",
                report.findings
            );
        }
    }

    #[test]
    fn doctor_reports_cursor_deferred_surfaces() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        for dir in [
            ".cursor/skills/repo-audit",
            ".cursor/commands",
            ".cursor/agents",
        ] {
            if let Err(error) = fs::create_dir_all(repo.join(dir)) {
                panic!("fixture dirs should be created: {error}");
            }
        }
        if let Err(error) = fs::write(
            repo.join(".cursor/skills/repo-audit/SKILL.md"),
            "# Repo audit\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join(".cursor/hooks.json"), "{}\n") {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join(".cursor/commands/review.md"), "# Review\n") {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join(".cursor/agents/investigator.md"), "# Agent\n") {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join(".cursor/mcp.json"), "{}\n") {
            panic!("fixture should be written: {error}");
        }

        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should succeed: {error}"),
        };

        for expected in [
            "cursor_skills: deferred",
            "cursor_hooks: deferred",
            "cursor_commands: deferred",
            "cursor_subagents: deferred",
            "cursor_mcp: deferred",
        ] {
            assert!(
                report
                    .findings
                    .iter()
                    .any(|finding| finding.starts_with(expected)),
                "missing doctor finding starting with {expected}; findings: {:?}",
                report.findings
            );
        }
    }

    #[test]
    fn doctor_reports_copilot_deferred_surfaces() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        for dir in [".github/hooks", "mcp", ".github/workflows", "environment"] {
            if let Err(error) = fs::create_dir_all(repo.join(dir)) {
                panic!("fixture dirs should be created: {error}");
            }
        }
        if let Err(error) = fs::write(repo.join(".github/hooks/pre-tool.json"), "{}\n") {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join("mcp/repository-mcp-settings.json"), "{}\n") {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join(".github/workflows/copilot-setup-steps.yml"),
            "name: Copilot Setup Steps\n",
        ) {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join("environment/agent-environment.json"), "{}\n") {
            panic!("fixture should be written: {error}");
        }

        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should succeed: {error}"),
        };

        for expected in [
            "copilot_hooks: deferred",
            "copilot_repository_mcp: deferred",
            "copilot_setup_steps: deferred",
            "copilot_agent_environment: deferred",
        ] {
            assert!(
                report
                    .findings
                    .iter()
                    .any(|finding| finding.starts_with(expected)),
                "missing doctor finding starting with {expected}; findings: {:?}",
                report.findings
            );
        }
    }

    #[test]
    fn doctor_reports_gemini_read_only_and_deferred_surfaces() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        for dir in [".gemini", ".gemini/agents", ".gemini/hooks"] {
            if let Err(error) = fs::create_dir_all(repo.join(dir)) {
                panic!("fixture dirs should be created: {error}");
            }
        }
        if let Err(error) = fs::write(
            repo.join(".gemini/settings.json"),
            r#"{"mcpServers":{"repo":{"command":"repo-tools"}},"policyPaths":[".gemini/policies"],"context":{"fileName":["CONTEXT.md","GEMINI.md"]},"hooks":{"BeforeTool":[]}}"#,
        ) {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join(".gemini/agents/investigator.md"), "# Agent\n") {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join(".gemini/hooks/guard.json"), "{}\n") {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(repo.join("gemini-extension.json"), "{}\n") {
            panic!("fixture should be written: {error}");
        }

        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should succeed: {error}"),
        };

        for expected in [
            "gemini_project_mcp: read-only diagnostic",
            "gemini_policy_settings: read-only diagnostic",
            "gemini_custom_context_filenames: deferred",
            "gemini_hooks: deferred",
            "gemini_subagents: deferred",
            "gemini_extensions: deferred",
        ] {
            assert!(
                report
                    .findings
                    .iter()
                    .any(|finding| finding.starts_with(expected)),
                "missing doctor finding starting with {expected}; findings: {:?}",
                report.findings
            );
        }
    }

    #[test]
    fn doctor_warns_about_sensitive_lockfile_metadata() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        let mut lockfile = Lockfile::empty();
        let mut entity = entity_entry(
            EntityType::Subagent,
            hash("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        );
        entity.locations.insert(
            location_key(".ai"),
            std::path::PathBuf::from("subagents/service-token.md"),
        );
        let entity_id = entity_id("subagent:service-token");
        lockfile.entities.insert(entity_id.clone(), entity);
        lockfile.overrides.insert(
            entity_id,
            std::collections::BTreeMap::from([(
                runtime_name("codex"),
                OverrideEntry(std::collections::BTreeMap::from([
                    ("api_token".to_string(), serde_json::json!("redacted")),
                    (
                        "header".to_string(),
                        serde_json::json!("ghp_0123456789abcdefghijklmnopqrstuv"),
                    ),
                ])),
            )]),
        );
        if let Err(error) = write_lockfile(&repo, &lockfile) {
            panic!("lockfile should write: {error}");
        }

        let report = match doctor(&repo) {
            Ok(report) => report,
            Err(error) => panic!("doctor should succeed: {error}"),
        };

        assert!(report.health.lockfile_privacy_warnings >= 2);
        assert!(report.findings.iter().any(|finding| {
            finding.starts_with("lockfile_privacy_warning_")
                && finding.contains("entity id `subagent:service-token`")
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.starts_with("lockfile_privacy_warning_")
                && finding.contains("override key `api_token`")
        }));
        assert!(report.findings.iter().any(|finding| {
            finding.starts_with("lockfile_privacy_warning_")
                && finding.contains("override value `header`")
        }));
        assert!(
            !report
                .findings
                .iter()
                .any(|finding| finding.contains("ghp_0123456789"))
        );
    }

    #[test]
    fn doctor_conflict_details_include_preserved_paths() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        let cache = cache_for(&temp, &repo);
        let id = entity_id("skill:demo");
        let mut lockfile = Lockfile::empty();
        let mut entity = entity_entry(
            EntityType::Skill,
            hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        );
        entity.pending_conflict_resolution = Some(true);
        lockfile.entities.insert(id.clone(), entity);
        if let Err(error) = preserve_losing_version(
            &cache.conflicts_dir,
            &id,
            &runtime_name("claude"),
            "unix-10",
            "---\nname: demo\n---\nBody\n",
        ) {
            panic!("preserved version should write: {error}");
        }

        let findings = match super::doctor::doctor_conflict_findings(&cache, &lockfile) {
            Ok(findings) => findings,
            Err(error) => panic!("conflict findings should build: {error}"),
        };

        assert!(findings.iter().any(|finding| {
            finding.starts_with("conflict_skill:demo:") && finding.contains("claude-unix-10.md")
        }));
    }

    #[test]
    fn hook_triggers_request_background_drain() {
        assert!(super::should_kick_background_drainer(&SyncOptions {
            trigger: Some("claude-hook".to_string()),
            ..SyncOptions::default()
        }));
        assert!(!super::should_kick_background_drainer(&SyncOptions {
            trigger: Some("claude-hook".to_string()),
            await_drain: true,
            ..SyncOptions::default()
        }));
        assert!(!super::should_kick_background_drainer(&SyncOptions {
            trigger: Some("cli".to_string()),
            ..SyncOptions::default()
        }));
    }

    #[test]
    fn reconcile_rebuilds_conflicted_lockfile() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let repo = temp.path().join("repo");
        if let Err(error) = fs::create_dir_all(&repo) {
            panic!("repo should be created: {error}");
        }
        if let Err(error) = fs::write(repo.join("AGENTS.md"), "Instructions\n") {
            panic!("fixture should be written: {error}");
        }
        if let Err(error) = fs::write(
            repo.join("agentmesh.lock"),
            "<<<<<<< ours\nversion: 1\n=======\nversion: 1\n>>>>>>> theirs\n",
        ) {
            panic!("conflicted lockfile should write: {error}");
        }

        let summary = match reconcile_lock(&repo) {
            Ok(summary) => summary,
            Err(error) => panic!("reconcile should succeed: {error}"),
        };

        assert!(summary.changed);
        let lockfile = match read_lockfile(&repo) {
            Ok(lockfile) => lockfile,
            Err(error) => panic!("lockfile should read: {error}"),
        };
        assert!(
            lockfile
                .entities
                .contains_key(&entity_id("instructions:root"))
        );
    }

    #[test]
    fn reconcile_conflict_parser_unions_both_lockfile_sides() {
        let mut left = Lockfile::empty();
        left.entities.insert(
            entity_id("skill:left"),
            entity_entry(
                EntityType::Skill,
                hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            ),
        );
        let mut right = Lockfile::empty();
        right.entities.insert(
            entity_id("skill:right"),
            entity_entry(
                EntityType::Skill,
                hash("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ),
        );
        let left_text = match serde_norway::to_string(&left) {
            Ok(contents) => contents,
            Err(error) => panic!("left side should serialize: {error}"),
        };
        let right_text = match serde_norway::to_string(&right) {
            Ok(contents) => contents,
            Err(error) => panic!("right side should serialize: {error}"),
        };
        let contents = format!("<<<<<<< ours\n{left_text}=======\n{right_text}>>>>>>> theirs\n");

        let (left_side, right_side) = super::split_lockfile_conflict_sides(&contents);
        let parsed_left =
            match super::parse_lockfile_side(std::path::Path::new("agentmesh.lock"), &left_side) {
                Ok(lockfile) => lockfile,
                Err(error) => panic!("left side should parse: {error}"),
            };
        let parsed_right =
            match super::parse_lockfile_side(std::path::Path::new("agentmesh.lock"), &right_side) {
                Ok(lockfile) => lockfile,
                Err(error) => panic!("right side should parse: {error}"),
            };
        let union = super::union_lockfiles(parsed_left, parsed_right);
        let ids = union
            .entities
            .keys()
            .map(EntityId::as_str)
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["skill:left", "skill:right"]);
    }
}
