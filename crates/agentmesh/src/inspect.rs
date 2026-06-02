use std::fs;
use std::path::{Path, PathBuf};

use agentmesh_adapter_sdk_rust::Adapter;
use agentmesh_protocol::ImportRequest;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::*;

#[derive(Debug)]
pub(crate) struct RepoSnapshot {
    pub(crate) repo_root: PathBuf,
    pub(crate) repo_name: String,
    pub(crate) lockfile: LockfileSnapshot,
    pub(crate) integrity: IntegritySnapshot,
    pub(crate) hook_ownership: HookOwnershipSnapshot,
    pub(crate) watcher: WatcherSnapshot,
    pub(crate) pending_syncs: usize,
    pub(crate) runtimes: Vec<RuntimeSnapshot>,
    pub(crate) unknown_runtimes: Vec<PathBuf>,
    pub(crate) core_findings: Vec<String>,
    pub(crate) core_health: Option<agentmesh_core::DoctorHealth>,
}

#[derive(Debug)]
pub(crate) struct LockfileSnapshot {
    pub(crate) status: String,
    pub(crate) schema: Option<u32>,
    pub(crate) entities: usize,
    pub(crate) pending_conflicts: usize,
    pub(crate) pending_conflict_ids: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct IntegritySnapshot {
    pub(crate) status: String,
    pub(crate) cache_root: PathBuf,
    pub(crate) pinned_path: Option<PathBuf>,
    pub(crate) pinned_sha256: Option<String>,
    pub(crate) running_path: Option<PathBuf>,
    pub(crate) running_sha256: Option<String>,
    pub(crate) matches_running_binary: Option<bool>,
}

#[derive(Debug)]
pub(crate) struct HookOwnershipSnapshot {
    pub(crate) status: String,
    pub(crate) path: PathBuf,
    pub(crate) entries: Vec<HookOwnershipRuntimeSnapshot>,
    pub(crate) issues: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct HookOwnershipRuntimeSnapshot {
    pub(crate) runtime: String,
    pub(crate) overlay_file: PathBuf,
    pub(crate) entry_paths: Vec<String>,
    pub(crate) installed_at: String,
    pub(crate) installer_version: String,
    pub(crate) hook_present: bool,
}

#[derive(Debug)]
pub(crate) struct WatcherSnapshot {
    pub(crate) status: String,
    pub(crate) running: bool,
    pub(crate) drain_status: String,
    pub(crate) log_file: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct RuntimeSnapshot {
    pub(crate) name: &'static str,
    pub(crate) present: bool,
    pub(crate) evidence: Vec<PathBuf>,
    pub(crate) entities: Vec<String>,
    pub(crate) import_error: Option<String>,
    pub(crate) hook_overlay: PathBuf,
    pub(crate) hook_installed: bool,
    pub(crate) hook_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReviewedDiffState {
    pub(crate) repo_root: PathBuf,
    pub(crate) created_at: String,
    pub(crate) summary: ReviewedDiffSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReviewedDiffSummary {
    pub(crate) changed: bool,
    pub(crate) entities_changed: usize,
    pub(crate) pending_conflicts: usize,
    pub(crate) capability_skipped: usize,
}

impl From<&agentmesh_core::SyncSummary> for ReviewedDiffSummary {
    fn from(summary: &agentmesh_core::SyncSummary) -> Self {
        Self {
            changed: summary.changed,
            entities_changed: summary.entities_changed,
            pending_conflicts: summary.pending_conflicts,
            capability_skipped: summary.capability_skipped,
        }
    }
}

pub(crate) fn inspect_repo(context: &CliContext) -> Result<RepoSnapshot> {
    inspect_repo_with_options(
        context,
        InspectOptions {
            import_entities: true,
            include_core_findings: true,
            include_unknown_runtimes: true,
        },
    )
}

pub(crate) fn inspect_status_repo(context: &CliContext) -> Result<RepoSnapshot> {
    inspect_repo_with_options(
        context,
        InspectOptions {
            import_entities: false,
            include_core_findings: false,
            include_unknown_runtimes: false,
        },
    )
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InspectOptions {
    pub(crate) import_entities: bool,
    pub(crate) include_core_findings: bool,
    pub(crate) include_unknown_runtimes: bool,
}

fn inspect_repo_with_options(
    context: &CliContext,
    options: InspectOptions,
) -> Result<RepoSnapshot> {
    context.touch();
    let repo_name = context
        .repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .to_string();
    let cache = cache_layout(&context.repo_root)?;
    let runtimes = vec![
        inspect_claude(context, options.import_entities)?,
        inspect_codex(context, options.import_entities)?,
    ];
    let hook_ownership = inspect_hook_ownership(context, &cache, &runtimes)?;
    let (core_findings, core_health) = if options.include_core_findings {
        let report = agentmesh_core::doctor(&context.repo_root).map_err(map_core_error)?;
        (report.findings, Some(report.health))
    } else {
        (Vec::new(), None)
    };
    let unknown_runtimes = if options.include_unknown_runtimes {
        inspect_unknown_runtime_dirs(&context.repo_root)?
    } else {
        Vec::new()
    };

    Ok(RepoSnapshot {
        repo_root: context.repo_root.clone(),
        repo_name,
        lockfile: inspect_lockfile(&context.repo_root),
        integrity: inspect_integrity(&cache),
        hook_ownership,
        watcher: inspect_watcher(&context.repo_root),
        pending_syncs: inspect_pending_syncs(&cache)?,
        runtimes,
        unknown_runtimes,
        core_findings,
        core_health,
    })
}

fn inspect_pending_syncs(cache: &agentmesh_core::state::CacheLayout) -> Result<usize> {
    agentmesh_core::pending_queue::PendingQueue::new(&cache.pending_syncs_dir)
        .read_ready()
        .map(|records| records.len())
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))
}

pub(crate) fn inspect_unknown_runtime_dirs(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let mut unknown = Vec::new();
    let entries = match fs::read_dir(repo_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(unknown),
        Err(error) => return Err(CliError::from_io(error)),
    };
    for entry in entries {
        let entry = entry.map_err(CliError::from_io)?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with('.') || matches!(name, ".ai" | ".claude" | ".codex" | ".git") {
            continue;
        }
        if path.join("skills").is_dir()
            || path.join("agents").is_dir()
            || path.join("rules").is_dir()
            || path.join("hooks.json").is_file()
        {
            unknown.push(PathBuf::from(name));
        }
    }
    unknown.sort();
    Ok(unknown)
}

fn inspect_lockfile(repo_root: &Path) -> LockfileSnapshot {
    match agentmesh_core::lockfile::read_lockfile(repo_root) {
        Ok(lockfile) => {
            let pending_conflict_ids = lockfile
                .entities
                .iter()
                .filter(|(_, entity)| entity.pending_conflict_resolution == Some(true))
                .map(|(entity_id, _)| entity_id.as_str().to_string())
                .collect::<Vec<_>>();
            LockfileSnapshot {
                status: "present".to_string(),
                schema: Some(lockfile.schema),
                pending_conflicts: pending_conflict_ids.len(),
                pending_conflict_ids,
                entities: lockfile.entities.len(),
            }
        }
        Err(error) => LockfileSnapshot {
            status: format!("not ready ({error})"),
            schema: None,
            entities: 0,
            pending_conflicts: 0,
            pending_conflict_ids: Vec::new(),
        },
    }
}

fn inspect_integrity(cache: &agentmesh_core::state::CacheLayout) -> IntegritySnapshot {
    let running = std::env::current_exe().ok().and_then(|path| {
        agentmesh_core::state::sha256_file(&path)
            .ok()
            .map(|hash| (path, hash))
    });

    match agentmesh_core::state::read_integrity_pin(&cache.integrity_json) {
        Ok(pin) => {
            let matches_running_binary = running
                .as_ref()
                .map(|(path, hash)| path == &pin.binary_path && hash == &pin.binary_sha256);
            let status = match matches_running_binary {
                Some(true) => "pinned".to_string(),
                Some(false) => "mismatch".to_string(),
                None => "unknown (could not hash running binary)".to_string(),
            };
            let (running_path, running_sha256) = running
                .map(|(path, hash)| (Some(path), Some(hash.to_string())))
                .unwrap_or((None, None));
            IntegritySnapshot {
                status,
                cache_root: cache.root.clone(),
                pinned_path: Some(pin.binary_path),
                pinned_sha256: Some(pin.binary_sha256.to_string()),
                running_path,
                running_sha256,
                matches_running_binary,
            }
        }
        Err(_) => IntegritySnapshot {
            status: "not pinned".to_string(),
            cache_root: cache.root.clone(),
            pinned_path: None,
            pinned_sha256: None,
            running_path: running.as_ref().map(|(path, _)| path.clone()),
            running_sha256: running.map(|(_, hash)| hash.to_string()),
            matches_running_binary: None,
        },
    }
}

pub(crate) fn snapshot_exit_code(snapshot: &RepoSnapshot) -> AgentmeshExitCode {
    if integrity_exit_code(snapshot) == AgentmeshExitCode::Integrity
        || !snapshot.hook_ownership.issues.is_empty()
    {
        AgentmeshExitCode::Integrity
    } else if snapshot.lockfile.pending_conflicts > 0
        || snapshot.pending_syncs > 0
        || snapshot.core_health.as_ref().is_some_and(|health| {
            health.entities_out_of_sync > 0
                || health.failed_pending_syncs > 0
                || health.capability_skips > 0
                || health.pending_conflicts > 0
                || health.pending_syncs > 0
                || health.lockfile_privacy_warnings > 0
        })
    {
        AgentmeshExitCode::Drift
    } else {
        AgentmeshExitCode::Success
    }
}

pub(crate) fn integrity_exit_code(snapshot: &RepoSnapshot) -> AgentmeshExitCode {
    if snapshot.integrity.matches_running_binary == Some(false) {
        AgentmeshExitCode::Integrity
    } else {
        AgentmeshExitCode::Success
    }
}

fn inspect_hook_ownership(
    context: &CliContext,
    cache: &agentmesh_core::state::CacheLayout,
    runtimes: &[RuntimeSnapshot],
) -> Result<HookOwnershipSnapshot> {
    let path = cache.hook_ownership_json.clone();
    let ownership = match agentmesh_core::state::read_hook_ownership(&path) {
        Ok(ownership) => ownership,
        Err(agentmesh_core::state::StateError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            let issues = runtimes
                .iter()
                .filter(|runtime| runtime.hook_installed)
                .map(|runtime| {
                    format!(
                        "{} hook is installed but hook ownership is not recorded",
                        runtime.name
                    )
                })
                .collect::<Vec<_>>();
            let status = if issues.is_empty() {
                "not recorded".to_string()
            } else {
                "mismatch".to_string()
            };
            return Ok(HookOwnershipSnapshot {
                status,
                path,
                entries: Vec::new(),
                issues,
            });
        }
        Err(error) => return Err(CliError::new(error.to_string(), AgentmeshExitCode::Io)),
    };

    let mut entries = Vec::new();
    let mut issues = Vec::new();
    for (runtime, entry) in &ownership.0 {
        let overlay_path = context.repo_root.join(&entry.overlay_file);
        let hook_present = if runtime.as_str() == GIT_PRE_COMMIT_RUNTIME {
            fs::read_to_string(&overlay_path)
                .map(|content| {
                    content.contains(GIT_PRE_COMMIT_MARKER)
                        && content.contains("--trigger=git-pre-commit")
                })
                .unwrap_or(false)
        } else {
            let trigger = format!("{}-hook", runtime.as_str());
            fs::read_to_string(&overlay_path)
                .map(|content| content.contains(&trigger))
                .unwrap_or(false)
        };
        if !hook_present {
            issues.push(format!(
                "{} ownership is recorded but no matching hook was found in {}",
                runtime.as_str(),
                entry.overlay_file.display()
            ));
        }
        entries.push(HookOwnershipRuntimeSnapshot {
            runtime: runtime.as_str().to_string(),
            overlay_file: entry.overlay_file.clone(),
            entry_paths: entry.entry_paths.clone(),
            installed_at: entry.installed_at.clone(),
            installer_version: entry.installer_version.clone(),
            hook_present,
        });
    }

    for runtime in runtimes.iter().filter(|runtime| runtime.hook_installed) {
        let owned = entries.iter().any(|entry| entry.runtime == runtime.name);
        if !owned {
            issues.push(format!(
                "{} hook is installed but hook ownership has no entry",
                runtime.name
            ));
        }
    }

    let status = if issues.is_empty() { "ok" } else { "mismatch" }.to_string();
    Ok(HookOwnershipSnapshot {
        status,
        path,
        entries,
        issues,
    })
}

fn reviewed_diff_path(cache: &agentmesh_core::state::CacheLayout) -> PathBuf {
    cache.root.join("reviewed-diff.json")
}

pub(crate) fn write_reviewed_diff_state(
    context: &CliContext,
    summary: &agentmesh_core::SyncSummary,
) -> Result<Option<PathBuf>> {
    let cache = cache_layout(&context.repo_root)?;
    let path = reviewed_diff_path(&cache);
    if !summary.changed {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(CliError::from_io(error)),
        }
        return Ok(None);
    }

    cache
        .ensure_dirs()
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?;
    let state = ReviewedDiffState {
        repo_root: context.repo_root.clone(),
        created_at: timestamp_string(),
        summary: ReviewedDiffSummary::from(summary),
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?;
    fs::write(&path, bytes).map_err(CliError::from_io)?;
    Ok(Some(path))
}

pub(crate) fn read_reviewed_diff_state(
    context: &CliContext,
) -> Result<(PathBuf, ReviewedDiffState)> {
    let cache = cache_layout(&context.repo_root)?;
    let path = reviewed_diff_path(&cache);
    let bytes = fs::read(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CliError::new(
                "apply requires a reviewed diff; run `agentmesh diff` first",
                AgentmeshExitCode::Cancelled,
            )
        } else {
            CliError::from_io(error)
        }
    })?;
    let state = serde_json::from_slice(&bytes)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?;
    Ok((path, state))
}

pub(crate) fn clear_reviewed_diff_state(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CliError::from_io(error)),
    }
}

fn inspect_watcher(repo_root: &Path) -> WatcherSnapshot {
    match agentmesh_watcher::status(repo_root) {
        Ok(status) => WatcherSnapshot {
            status: status.state,
            running: status.running,
            drain_status: status.drain_status,
            log_file: Some(status.log_file),
        },
        Err(error) => WatcherSnapshot {
            status: format!("unavailable ({error})"),
            running: false,
            drain_status: "unknown".to_string(),
            log_file: None,
        },
    }
}

fn inspect_claude(context: &CliContext, import_entities: bool) -> Result<RuntimeSnapshot> {
    inspect_runtime(
        context,
        "claude",
        ".claude",
        ".claude/settings.local.json",
        "claude-hook",
        import_entities,
        agentmesh_adapter_claude::ClaudeAdapter,
    )
}

fn inspect_codex(context: &CliContext, import_entities: bool) -> Result<RuntimeSnapshot> {
    let mut runtime = inspect_runtime(
        context,
        "codex",
        ".codex",
        ".codex/hooks.json",
        "codex-hook",
        import_entities,
        agentmesh_adapter_codex::CodexAdapter,
    )?;
    if runtime.hook_installed {
        runtime.hook_note = Some(
            "Codex requires one-time trust approval before this command hook runs".to_string(),
        );
    }
    Ok(runtime)
}

fn inspect_runtime<A>(
    context: &CliContext,
    name: &'static str,
    runtime_dir_name: &str,
    overlay: &str,
    hook_trigger: &str,
    import_entities: bool,
    adapter: A,
) -> Result<RuntimeSnapshot>
where
    A: Adapter,
{
    let detected = adapter
        .detect(&context.repo_root)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?;
    let runtime_dir = context.repo_root.join(runtime_dir_name);
    let mut entities = Vec::new();
    let mut import_error = None;

    if detected.present && import_entities {
        match adapter.import(ImportRequest {
            canonical_dir: context.repo_root.join(".ai"),
            runtime_dir,
            filter: None,
        }) {
            Ok(imported) => {
                entities = imported
                    .entities
                    .into_iter()
                    .map(|entity| entity.id)
                    .collect();
            }
            Err(error) => {
                import_error = Some(error.to_string());
            }
        }
    }

    let overlay_path = PathBuf::from(overlay);
    let hook_installed = fs::read_to_string(context.repo_root.join(&overlay_path))
        .map(|content| content.contains(hook_trigger))
        .unwrap_or(false);

    Ok(RuntimeSnapshot {
        name,
        present: detected.present,
        evidence: detected.files,
        entities,
        import_error,
        hook_overlay: overlay_path,
        hook_installed,
        hook_note: None,
    })
}

pub(crate) fn status_json(snapshot: &RepoSnapshot) -> Result<String> {
    serde_json::to_string_pretty(&json!({
        "repo": snapshot.repo_name,
        "repo_root": snapshot.repo_root,
        "lockfile": {
            "status": snapshot.lockfile.status,
            "schema": snapshot.lockfile.schema,
            "entities": snapshot.lockfile.entities,
            "pending_conflicts": snapshot.lockfile.pending_conflicts,
            "pending_conflict_ids": snapshot.lockfile.pending_conflict_ids,
        },
        "integrity": {
            "status": snapshot.integrity.status,
            "pinned_path": snapshot.integrity.pinned_path,
            "pinned_sha256": snapshot.integrity.pinned_sha256,
            "running_path": snapshot.integrity.running_path,
            "running_sha256": snapshot.integrity.running_sha256,
            "matches_running_binary": snapshot.integrity.matches_running_binary,
        },
        "hook_ownership": hook_ownership_json(&snapshot.hook_ownership),
        "watcher": {
            "status": snapshot.watcher.status,
            "running": snapshot.watcher.running,
            "drain_status": snapshot.watcher.drain_status,
            "log_file": snapshot.watcher.log_file,
        },
        "pending_syncs": snapshot.pending_syncs,
        "unknown_runtimes": snapshot.unknown_runtimes,
        "core_findings": snapshot.core_findings,
        "core_health": core_health_json(snapshot.core_health.as_ref()),
        "runtimes": snapshot.runtimes.iter().map(runtime_json).collect::<Vec<_>>(),
    }))
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))
}

pub(crate) fn scan_json(snapshot: &RepoSnapshot) -> Result<String> {
    serde_json::to_string_pretty(&json!({
        "runtimes": snapshot.runtimes.iter().map(runtime_json).collect::<Vec<_>>(),
        "entity_count": snapshot.runtimes.iter().map(|runtime| runtime.entities.len()).sum::<usize>(),
    }))
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))
}

pub(crate) fn doctor_json(snapshot: &RepoSnapshot) -> Result<String> {
    serde_json::to_string_pretty(&json!({
        "version": agentmesh_core::VERSION,
        "repo_root": snapshot.repo_root,
        "lockfile": {
            "status": snapshot.lockfile.status,
            "schema": snapshot.lockfile.schema,
            "entities": snapshot.lockfile.entities,
            "pending_conflicts": snapshot.lockfile.pending_conflicts,
            "pending_conflict_ids": snapshot.lockfile.pending_conflict_ids,
        },
        "integrity": {
            "status": snapshot.integrity.status,
            "cache_root": snapshot.integrity.cache_root,
            "pinned_path": snapshot.integrity.pinned_path,
            "pinned_sha256": snapshot.integrity.pinned_sha256,
            "running_path": snapshot.integrity.running_path,
            "running_sha256": snapshot.integrity.running_sha256,
            "matches_running_binary": snapshot.integrity.matches_running_binary,
        },
        "hook_ownership": hook_ownership_json(&snapshot.hook_ownership),
        "runtimes": snapshot.runtimes.iter().map(runtime_json).collect::<Vec<_>>(),
        "watcher": {
            "status": snapshot.watcher.status,
            "running": snapshot.watcher.running,
            "drain_status": snapshot.watcher.drain_status,
            "log_file": snapshot.watcher.log_file,
        },
        "pending_syncs": snapshot.pending_syncs,
        "unknown_runtimes": snapshot.unknown_runtimes,
        "core_findings": snapshot.core_findings,
        "core_health": core_health_json(snapshot.core_health.as_ref()),
    }))
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))
}

fn core_health_json(health: Option<&agentmesh_core::DoctorHealth>) -> serde_json::Value {
    match health {
        Some(health) => json!({
            "entities_out_of_sync": health.entities_out_of_sync,
            "pending_conflicts": health.pending_conflicts,
            "pending_syncs": health.pending_syncs,
            "failed_pending_syncs": health.failed_pending_syncs,
            "capability_skips": health.capability_skips,
            "lockfile_privacy_warnings": health.lockfile_privacy_warnings,
        }),
        None => serde_json::Value::Null,
    }
}

fn runtime_json(runtime: &RuntimeSnapshot) -> serde_json::Value {
    json!({
        "name": runtime.name,
        "present": runtime.present,
        "evidence": runtime.evidence,
        "entities": runtime.entities,
        "import_error": runtime.import_error,
        "hook_overlay": runtime.hook_overlay,
        "hook_installed": runtime.hook_installed,
        "hook_note": runtime.hook_note,
    })
}

fn hook_ownership_json(ownership: &HookOwnershipSnapshot) -> serde_json::Value {
    json!({
        "status": ownership.status,
        "path": ownership.path,
        "entries": ownership.entries.iter().map(|entry| {
            json!({
                "runtime": &entry.runtime,
                "overlay_file": &entry.overlay_file,
                "entry_paths": &entry.entry_paths,
                "installed_at": &entry.installed_at,
                "installer_version": &entry.installer_version,
                "hook_present": entry.hook_present,
            })
        }).collect::<Vec<_>>(),
        "issues": &ownership.issues,
    })
}

pub(crate) fn print_status(_context: &CliContext, snapshot: &RepoSnapshot) {
    println!(
        "AgentMesh {}   repo: {}   lockfile: {}",
        agentmesh_core::VERSION,
        snapshot.repo_name,
        snapshot.lockfile.status
    );
    println!(
        "  hooks:    {}",
        snapshot
            .runtimes
            .iter()
            .map(|runtime| format!(
                "{} {}",
                runtime.name,
                check(_context, runtime.hook_installed)
            ))
            .collect::<Vec<_>>()
            .join("   ")
    );
    println!(
        "  watcher:  {} (drain: {})",
        snapshot.watcher.status, snapshot.watcher.drain_status
    );
    println!("  pending:  {} in queue", snapshot.pending_syncs);
    println!(
        "  conflicts: {} unresolved",
        snapshot.lockfile.pending_conflicts
    );
    println!("  integrity: {}", snapshot.integrity.status);
    if _context.verbose() {
        println!("  runtime details:");
        for runtime in &snapshot.runtimes {
            println!(
                "    {:<7} present={} hook={} entities={}",
                runtime.name,
                runtime.present,
                runtime.hook_installed,
                runtime.entities.len()
            );
            if _context.debug() && !runtime.evidence.is_empty() {
                println!(
                    "            evidence={}",
                    runtime
                        .evidence
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        if _context.debug() {
            println!("  cache: {}", snapshot.integrity.cache_root.display());
            for finding in &snapshot.core_findings {
                println!("  finding: {finding}");
            }
        }
    }
}

pub(crate) fn print_scan(context: &CliContext, snapshot: &RepoSnapshot) {
    println!("Detected runtimes:");
    for runtime in &snapshot.runtimes {
        let marker = check(context, runtime.present);
        let evidence = if runtime.evidence.is_empty() {
            "not detected".to_string()
        } else {
            runtime
                .evidence
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!("  {marker} {:<7} ({evidence})", runtime.name);
    }

    println!();
    println!("Detected entities:");
    let mut count = 0usize;
    for runtime in &snapshot.runtimes {
        if let Some(error) = &runtime.import_error {
            println!(
                "  {} {:<7} import failed: {error}",
                context.paint(OutputStyle::Warning, "⚠"),
                runtime.name
            );
            continue;
        }
        for entity in &runtime.entities {
            count += 1;
            println!("  {entity:<28} ({})", runtime.name);
        }
    }
    println!();
    println!("{count} runtime entity view(s) detected.");
}

pub(crate) fn print_doctor(context: &CliContext, snapshot: &RepoSnapshot) {
    println!("AgentMesh {}", agentmesh_core::VERSION);
    println!("Repository: {}", snapshot.repo_root.display());
    println!();
    println!("Adapters:");
    for runtime in &snapshot.runtimes {
        let state = if runtime.present {
            format!("{} detected", check(context, true))
        } else {
            format!("{} not detected", check(context, false))
        };
        println!(
            "  {:<7} {}   bundled, protocol 1, entities [instructions, skill, subagent]",
            runtime.name, state
        );
    }
    for runtime in &snapshot.unknown_runtimes {
        println!(
            "  unknown {} unsupported runtime candidate ({})",
            check(context, false),
            runtime.display()
        );
    }
    println!();
    print_integrity(snapshot);
    println!();
    println!("Hook entries:");
    for runtime in &snapshot.runtimes {
        println!(
            "  {:<7} {} pinned-absolute   ({})",
            runtime.name,
            check(context, runtime.hook_installed),
            runtime.hook_overlay.display()
        );
        if let Some(note) = &runtime.hook_note {
            println!(
                "           {} {note}",
                context.paint(OutputStyle::Warning, "⚠")
            );
        }
    }
    println!("  Ownership: {}", snapshot.hook_ownership.status);
    println!(
        "  Ownership file: {}",
        snapshot.hook_ownership.path.display()
    );
    for entry in &snapshot.hook_ownership.entries {
        println!(
            "    {:<7} {} owned entries ({})",
            entry.runtime,
            entry.entry_paths.len(),
            check(context, entry.hook_present)
        );
    }
    for issue in &snapshot.hook_ownership.issues {
        println!("    {} {issue}", context.paint(OutputStyle::Warning, "⚠"));
    }
    println!();
    println!("Watcher daemon:");
    println!("  Status:           {}", snapshot.watcher.status);
    println!("  Drain:            {}", snapshot.watcher.drain_status);
    if let Some(log_file) = &snapshot.watcher.log_file {
        println!("  Log:              {}", log_file.display());
    }
    println!();
    println!("Lockfile:");
    println!("  Status:           {}", snapshot.lockfile.status);
    if let Some(schema) = snapshot.lockfile.schema {
        println!("  Schema:           {schema} (current)");
    }
    println!("  Entities:         {}", snapshot.lockfile.entities);
    println!(
        "  Pending conflicts: {}",
        snapshot.lockfile.pending_conflicts
    );
    for entity_id in &snapshot.lockfile.pending_conflict_ids {
        println!("    {entity_id}");
        println!(
            "      restore: agentmesh restore {entity_id} --from <runtime> --at <timestamp> -y"
        );
        println!("      acknowledge: agentmesh ack {entity_id} -y");
    }
    if !snapshot.core_findings.is_empty() {
        println!();
        println!("Core findings:");
        for finding in &snapshot.core_findings {
            println!("  {finding}");
        }
    }
}

pub(crate) fn print_versions(snapshot: &RepoSnapshot) {
    println!("AgentMesh:          {}", agentmesh_core::VERSION);
    println!("Protocol versions:  supported [1]");
    println!(
        "Lockfile schema:    {}",
        snapshot
            .lockfile
            .schema
            .map(|schema| format!("{schema} (current)"))
            .unwrap_or_else(|| "not present".to_string())
    );
    println!();
    println!("Built-in adapters:");
    println!("  claude    bundled   protocol [1]   entities [instructions, skill, subagent]");
    println!("  codex     bundled   protocol [1]   entities [instructions, skill, subagent]");
}

pub(crate) fn print_integrity(snapshot: &RepoSnapshot) {
    println!("Hook integrity:");
    println!("  Status:           {}", snapshot.integrity.status);
    println!(
        "  Cache:            {}",
        snapshot.integrity.cache_root.display()
    );
    if let Some(path) = &snapshot.integrity.pinned_path {
        println!("  Binary path:      {} (pinned)", path.display());
    } else {
        println!("  Binary path:      not pinned yet");
    }
    if let Some(hash) = &snapshot.integrity.pinned_sha256 {
        println!("  Pinned sha256:    {hash}");
    }
    if let Some(path) = &snapshot.integrity.running_path {
        println!("  Running binary:   {}", path.display());
    }
    if let Some(hash) = &snapshot.integrity.running_sha256 {
        println!("  Running sha256:   {hash}");
    }
    println!("  Hook entry style: pinned-absolute for Claude and Codex when installed");
}
