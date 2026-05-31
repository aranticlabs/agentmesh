use super::*;

const DOCTOR_PRIVACY_WARNING_DETAIL_LIMIT: usize = 20;

pub fn doctor(repo_root: &Path) -> Result<DoctorReport> {
    doctor_with_adapter_registry(repo_root, &SubprocessAdapterRegistry)
}

/// Builds a health report with an explicit adapter registry.
pub fn doctor_with_adapter_registry(
    repo_root: &Path,
    adapters: &dyn AdapterRegistry,
) -> Result<DoctorReport> {
    let cache = CacheLayout::new(&default_cache_root()?, repo_root)?;
    let lockfile = read_lockfile_or_empty(repo_root)?;
    let pending_queue = PendingQueue::new(&cache.pending_syncs_dir);
    let pending_count = pending_queue.read_ready()?.len();
    let failed_pending_count = failed_pending_records(&cache.pending_syncs_dir)?;
    let pending_conflicts = lockfile
        .entities
        .values()
        .filter(|entry| entry.pending_conflict_resolution == Some(true))
        .count();
    let config = load_config(repo_root)?.config;
    let capability_skipped = capability_skip_count_for_lockfile(&lockfile, &config)?;
    let sync_state = entity_sync_state(repo_root, &lockfile)?;
    let privacy_findings = doctor_lockfile_privacy_findings(&lockfile);

    let mut findings = Vec::new();
    findings.push(format!("entities: {}", lockfile.entities.len()));
    findings.push(format!("entities_in_sync: {}", sync_state.in_sync));
    findings.push(format!("entities_out_of_sync: {}", sync_state.out_of_sync));
    findings.push(format!("pending_conflicts: {pending_conflicts}"));
    findings.push(format!("pending_syncs: {pending_count}"));
    findings.push(format!("failed_pending_syncs: {failed_pending_count}"));
    findings.extend(doctor_pending_failure_findings(&cache.pending_syncs_dir)?);
    findings.push(format!("capability_skips: {capability_skipped}"));
    findings.push(format!("cache_root: {}", cache.root.display()));
    findings.extend(doctor_integrity_findings(repo_root, &cache)?);
    findings.extend(doctor_adapter_findings(repo_root, &lockfile, adapters)?);
    findings.extend(doctor_hook_findings(repo_root, &cache)?);
    findings.extend(doctor_conflict_findings(&cache, &lockfile)?);
    findings.extend(privacy_findings.findings);
    findings.push(format!("watcher_pid: {}", cache.watcher_pid.display()));
    findings.push(format!("watcher_log: {}", cache.watcher_log.display()));
    findings.push("network: disabled".to_string());

    Ok(DoctorReport {
        findings,
        health: DoctorHealth {
            entities_out_of_sync: sync_state.out_of_sync,
            pending_conflicts,
            pending_syncs: pending_count,
            failed_pending_syncs: failed_pending_count,
            capability_skips: capability_skipped,
            lockfile_privacy_warnings: privacy_findings.warning_count,
        },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LockfilePrivacyFindings {
    warning_count: usize,
    findings: Vec<String>,
}

fn doctor_lockfile_privacy_findings(lockfile: &Lockfile) -> LockfilePrivacyFindings {
    let mut warnings = Vec::new();
    let mut warning_count = 0;

    for (entity_id, entity) in &lockfile.entities {
        if contains_sensitive_term(entity_id.as_str()) {
            push_privacy_warning(
                &mut warnings,
                &mut warning_count,
                format!(
                    "entity id `{}` contains sensitive-looking text",
                    entity_id.as_str()
                ),
            );
        }
        for (location, path) in &entity.locations {
            if path_contains_sensitive_term(path) {
                push_privacy_warning(
                    &mut warnings,
                    &mut warning_count,
                    format!(
                        "location path for `{}` at `{}` contains sensitive-looking text: {}",
                        entity_id.as_str(),
                        location.as_str(),
                        path.display()
                    ),
                );
            }
        }
        for entry in &entity.lineage {
            if path_contains_sensitive_term(&entry.imported_from) {
                push_privacy_warning(
                    &mut warnings,
                    &mut warning_count,
                    format!(
                        "lineage path for `{}` contains sensitive-looking text: {}",
                        entity_id.as_str(),
                        entry.imported_from.display()
                    ),
                );
            }
        }
        for record in &entity.rename_history {
            if path_contains_sensitive_term(&record.from) {
                push_privacy_warning(
                    &mut warnings,
                    &mut warning_count,
                    format!(
                        "rename source for `{}` contains sensitive-looking text: {}",
                        entity_id.as_str(),
                        record.from.display()
                    ),
                );
            }
            if path_contains_sensitive_term(&record.to) {
                push_privacy_warning(
                    &mut warnings,
                    &mut warning_count,
                    format!(
                        "rename target for `{}` contains sensitive-looking text: {}",
                        entity_id.as_str(),
                        record.to.display()
                    ),
                );
            }
        }
    }

    for (entity_id, overrides) in &lockfile.overrides {
        for (runtime, override_entry) in overrides {
            collect_sensitive_override_keys(
                entity_id,
                runtime,
                &override_entry.0,
                &mut warnings,
                &mut warning_count,
            );
        }
    }

    let mut findings = Vec::new();
    if warning_count > 0 {
        findings.push(format!("lockfile_privacy_warnings: {warning_count}"));
    }
    findings.extend(warnings);
    if warning_count > DOCTOR_PRIVACY_WARNING_DETAIL_LIMIT {
        findings.push(format!(
            "lockfile_privacy_warnings_truncated: {} additional warning(s)",
            warning_count - DOCTOR_PRIVACY_WARNING_DETAIL_LIMIT
        ));
    }

    LockfilePrivacyFindings {
        warning_count,
        findings,
    }
}

fn push_privacy_warning(warnings: &mut Vec<String>, warning_count: &mut usize, detail: String) {
    *warning_count += 1;
    if warnings.len() < DOCTOR_PRIVACY_WARNING_DETAIL_LIMIT {
        warnings.push(format!(
            "lockfile_privacy_warning_{warning_count}: {detail}"
        ));
    }
}

fn collect_sensitive_override_keys(
    entity_id: &EntityId,
    runtime: &RuntimeName,
    values: &BTreeMap<String, Value>,
    warnings: &mut Vec<String>,
    warning_count: &mut usize,
) {
    for (key, value) in values {
        collect_sensitive_json_keys(
            entity_id,
            runtime,
            Some(key),
            value,
            warnings,
            warning_count,
        );
    }
}

fn collect_sensitive_json_keys(
    entity_id: &EntityId,
    runtime: &RuntimeName,
    key: Option<&str>,
    value: &Value,
    warnings: &mut Vec<String>,
    warning_count: &mut usize,
) {
    if let Some(key) = key
        && contains_sensitive_term(key)
    {
        push_privacy_warning(
            warnings,
            warning_count,
            format!(
                "override key `{key}` for `{}` at `{}` looks sensitive; keep secrets in machine-local config or environment variables",
                entity_id.as_str(),
                runtime.as_str()
            ),
        );
    }

    match value {
        Value::Object(map) => {
            for (child_key, child_value) in map {
                collect_sensitive_json_keys(
                    entity_id,
                    runtime,
                    Some(child_key),
                    child_value,
                    warnings,
                    warning_count,
                );
            }
        }
        Value::Array(values) => {
            for child_value in values {
                collect_sensitive_json_keys(
                    entity_id,
                    runtime,
                    None,
                    child_value,
                    warnings,
                    warning_count,
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn path_contains_sensitive_term(path: &Path) -> bool {
    path.components()
        .any(|component| contains_sensitive_term(&component.as_os_str().to_string_lossy()))
}

fn contains_sensitive_term(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    [
        "access-key",
        "access_key",
        "apikey",
        "api-key",
        "api_key",
        "auth-token",
        "auth_token",
        "bearer",
        "client-secret",
        "client_secret",
        "cookie",
        "credential",
        "jwt",
        "oauth",
        "passwd",
        "password",
        "private-key",
        "private_key",
        "secret",
        "session",
        "token",
    ]
    .iter()
    .any(|term| normalized.contains(term))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct EntitySyncState {
    in_sync: usize,
    out_of_sync: usize,
}

fn failed_pending_records(dir: &Path) -> Result<usize> {
    let mut count = 0;
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|source| PipelineError::Io {
                    action: "read directory entry",
                    path: dir.to_path_buf(),
                    source,
                })?;
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("failed-"))
                {
                    count += 1;
                }
            }
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read directory",
                path: dir.to_path_buf(),
                source,
            });
        }
    }
    Ok(count)
}

fn doctor_pending_failure_findings(dir: &Path) -> Result<Vec<String>> {
    let mut findings = Vec::new();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|source| PipelineError::Io {
                    action: "read directory entry",
                    path: dir.to_path_buf(),
                    source,
                })?;
                let path = entry.path();
                if !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("failed-"))
                {
                    continue;
                }
                let record = read_json::<PendingSyncRecord>(&path)?;
                findings.push(format!(
                    "pending_failure_{}: path={} attempts={} error={}",
                    record.pending_id,
                    path.display(),
                    record.attempts,
                    record.last_error.as_deref().unwrap_or("unknown")
                ));
            }
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read directory",
                path: dir.to_path_buf(),
                source,
            });
        }
    }
    findings.sort();
    Ok(findings)
}

fn entity_sync_state(repo_root: &Path, lockfile: &Lockfile) -> Result<EntitySyncState> {
    let mut state = EntitySyncState::default();
    for entity in lockfile.entities.values() {
        let mut out_of_sync = entity.locations.is_empty();
        for (location, path) in &entity.locations {
            let Some(expected_hash) = entity.emitted_native_sha256.get(location).or_else(|| {
                if location.as_str() == ".ai" {
                    Some(&entity.canonical_sha256)
                } else {
                    None
                }
            }) else {
                out_of_sync = true;
                continue;
            };
            let Some(actual_hash) =
                entity_location_hash(repo_root, entity.entity_type, location, path)?
            else {
                out_of_sync = true;
                continue;
            };
            if &actual_hash != expected_hash {
                out_of_sync = true;
            }
        }
        if out_of_sync {
            state.out_of_sync += 1;
        } else {
            state.in_sync += 1;
        }
    }
    Ok(state)
}

fn doctor_integrity_findings(repo_root: &Path, cache: &CacheLayout) -> Result<Vec<String>> {
    let current = current_integrity_pin(repo_root)?;
    match read_integrity_pin(&cache.integrity_json) {
        Ok(pin) => {
            let mode = if pin.binary_path.is_absolute() {
                "pinned-absolute"
            } else {
                "path-resolved"
            };
            let status = if pin.binary_path == current.binary_path
                && pin.binary_sha256 == current.binary_sha256
            {
                "match"
            } else {
                "mismatch"
            };
            Ok(vec![
                format!("integrity: {status}"),
                format!("integrity_mode: {mode}"),
                format!("integrity_pinned_binary: {}", pin.binary_path.display()),
                format!("integrity_pinned_sha256: {}", pin.binary_sha256.as_str()),
                format!(
                    "integrity_current_binary: {}",
                    current.binary_path.display()
                ),
                format!(
                    "integrity_current_sha256: {}",
                    current.binary_sha256.as_str()
                ),
                format!("integrity_version: {}", pin.binary_version),
            ])
        }
        Err(StateError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(vec![
                "integrity: unpinned".to_string(),
                format!(
                    "integrity_current_binary: {}",
                    current.binary_path.display()
                ),
                format!(
                    "integrity_current_sha256: {}",
                    current.binary_sha256.as_str()
                ),
            ])
        }
        Err(error) => Err(error.into()),
    }
}

fn doctor_adapter_findings(
    repo_root: &Path,
    lockfile: &Lockfile,
    adapters: &dyn AdapterRegistry,
) -> Result<Vec<String>> {
    let markers = detect_runtime_markers(repo_root, adapters)?;
    let known = [
        (runtime_name("claude")?, markers.claude),
        (runtime_name("codex")?, markers.codex),
    ];
    let mut findings = Vec::new();
    let mut known_runtimes = BTreeSet::new();
    for (runtime, detected) in &known {
        known_runtimes.insert(runtime.clone());
        if let Some(adapter) = lockfile.adapters.get(runtime) {
            findings.push(format!(
                "adapter_{}: detected={} declared=true mode={} protocol={} entities={} hooks={}",
                runtime.as_str(),
                detected,
                adapter_mode_name(adapter.mode),
                adapter.protocol_version,
                adapter.entities.len(),
                adapter.hooks.len()
            ));
        } else {
            findings.push(format!(
                "adapter_{}: detected={} declared=false",
                runtime.as_str(),
                detected
            ));
        }
    }
    findings.extend(
        lockfile
        .adapters
        .iter()
            .filter(|(runtime, _)| !known_runtimes.contains(*runtime))
            .map(|(runtime, adapter)| {
            format!(
                    "adapter_{}: detected=false declared=true mode={} protocol={} entities={} hooks={}",
                runtime.as_str(),
                adapter_mode_name(adapter.mode),
                adapter.protocol_version,
                    adapter.entities.len(),
                    adapter.hooks.len()
            )
            }),
    );
    for entity_type in [
        EntityType::Instructions,
        EntityType::Skill,
        EntityType::Subagent,
    ] {
        let runtimes = lockfile
            .adapters
            .iter()
            .filter(|(_, adapter)| adapter.entities.contains(&entity_type))
            .map(|(runtime, _)| runtime.as_str())
            .collect::<Vec<_>>();
        let coverage = if runtimes.is_empty() {
            "none".to_string()
        } else {
            runtimes.join(",")
        };
        findings.push(format!(
            "adapter_coverage_{}: {coverage}",
            entity_type.as_str()
        ));
    }
    if lockfile.adapters.is_empty() {
        findings.push("adapters: none".to_string());
    }
    Ok(findings)
}

fn adapter_mode_name(mode: AdapterMode) -> &'static str {
    match mode {
        AdapterMode::Bundled => "bundled",
    }
}

fn doctor_hook_findings(repo_root: &Path, cache: &CacheLayout) -> Result<Vec<String>> {
    match read_hook_ownership(&cache.hook_ownership_json) {
        Ok(ownership) if ownership.0.is_empty() => Ok(vec!["hooks: none".to_string()]),
        Ok(ownership) => Ok(ownership
            .0
            .iter()
            .map(|(runtime, entry)| {
                let overlay = repo_root.join(&entry.overlay_file);
                let overlay_exists = overlay.is_file();
                let command_present = if overlay_exists {
                    fs::read_to_string(&overlay)
                        .map(|contents| {
                            contents.contains("agentmesh")
                                && contents.contains(&format!("{}-hook", runtime.as_str()))
                        })
                        .unwrap_or(false)
                } else {
                    false
                };
                let drift = !overlay_exists || entry.entry_paths.is_empty() || !command_present;
                format!(
                    "hook_{}: overlay={} entries={} exists={} command_present={} drift={}",
                    runtime.as_str(),
                    entry.overlay_file.display(),
                    entry.entry_paths.len(),
                    overlay_exists,
                    command_present,
                    drift
                )
            })
            .collect()),
        Err(StateError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Ok(vec!["hooks: none".to_string()])
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn doctor_conflict_findings(
    cache: &CacheLayout,
    lockfile: &Lockfile,
) -> Result<Vec<String>> {
    let mut preserved = 0;
    match fs::read_dir(&cache.conflicts_dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|source| PipelineError::Io {
                    action: "read directory entry",
                    path: cache.conflicts_dir.clone(),
                    source,
                })?;
                if entry.path().is_dir() {
                    preserved += 1;
                }
            }
        }
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read directory",
                path: cache.conflicts_dir.clone(),
                source,
            });
        }
    }
    let pending = lockfile
        .entities
        .values()
        .filter(|entry| entry.pending_conflict_resolution == Some(true))
        .count();
    let mut findings = vec![
        format!("preserved_conflict_entities: {preserved}"),
        format!("pending_conflict_entities: {pending}"),
    ];
    for (entity_id, entity) in &lockfile.entities {
        if entity.pending_conflict_resolution != Some(true) {
            continue;
        }
        let preserved_paths = preserved_conflict_paths(cache, entity_id)?;
        let preserved = if preserved_paths.is_empty() {
            "none".to_string()
        } else {
            preserved_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        findings.push(format!(
            "conflict_{}: pending=true preserved={preserved}",
            entity_id.as_str()
        ));
    }
    Ok(findings)
}

fn preserved_conflict_paths(cache: &CacheLayout, entity_id: &EntityId) -> Result<Vec<PathBuf>> {
    let dir = conflict_entity_dir(&cache.conflicts_dir, entity_id);
    let mut paths = Vec::new();
    match fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|source| PipelineError::Io {
                    action: "read directory entry",
                    path: dir.clone(),
                    source,
                })?;
                let path = entry.path();
                if path.is_file() {
                    paths.push(path);
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
    paths.sort();
    Ok(paths)
}
