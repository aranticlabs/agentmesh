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
    let capability_report =
        capability_skip_report_for_lockfile(&lockfile, &config, CapabilityReportMode::Diagnostic)?;
    let capability_skipped = capability_report.skipped;
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
    findings.extend(
        capability_report
            .findings
            .iter()
            .map(capability_skip_finding_message),
    );
    findings.push(format!("cache_root: {}", cache.root.display()));
    findings.extend(doctor_integrity_findings(repo_root, &cache)?);
    findings.extend(doctor_adapter_findings(repo_root, &lockfile, adapters)?);
    findings.extend(doctor_codex_surface_findings(repo_root)?);
    findings.extend(doctor_copilot_surface_findings(repo_root)?);
    findings.extend(doctor_cursor_rule_findings(repo_root)?);
    findings.extend(doctor_cursor_surface_findings(repo_root)?);
    findings.extend(doctor_gemini_surface_findings(repo_root)?);
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
        Value::String(value) => {
            if looks_like_sensitive_value(value) {
                let field = key
                    .map(|key| format!(" `{key}`"))
                    .unwrap_or_else(|| " string".to_string());
                push_privacy_warning(
                    warnings,
                    warning_count,
                    format!(
                        "override value{field} for `{}` at `{}` looks sensitive; keep secrets in machine-local config or environment variables",
                        entity_id.as_str(),
                        runtime.as_str()
                    ),
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
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

fn looks_like_sensitive_value(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.contains("-----BEGIN ") && trimmed.contains("PRIVATE KEY-----") {
        return true;
    }
    if trimmed.starts_with("github_pat_")
        || trimmed.starts_with("ghp_")
        || trimmed.starts_with("gho_")
        || trimmed.starts_with("ghu_")
        || trimmed.starts_with("ghs_")
        || trimmed.starts_with("ghr_")
        || trimmed.starts_with("sk-")
        || trimmed.starts_with("xoxb-")
        || trimmed.starts_with("xoxp-")
    {
        return true;
    }
    trimmed.len() >= 40
        && trimmed
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        && trimmed
            .chars()
            .any(|character| character.is_ascii_lowercase())
        && trimmed
            .chars()
            .any(|character| character.is_ascii_uppercase())
        && trimmed.chars().any(|character| character.is_ascii_digit())
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
    let mut findings = Vec::new();
    let claude_detected =
        doctor_detect_runtime(repo_root, adapters, &runtime_name("claude")?, &mut findings);
    let codex_detected =
        doctor_detect_runtime(repo_root, adapters, &runtime_name("codex")?, &mut findings);
    let copilot_detected = doctor_detect_runtime(
        repo_root,
        adapters,
        &runtime_name("copilot")?,
        &mut findings,
    );
    let cursor_detected =
        doctor_detect_runtime(repo_root, adapters, &runtime_name("cursor")?, &mut findings);
    let gemini_detected =
        doctor_detect_runtime(repo_root, adapters, &runtime_name("gemini")?, &mut findings);
    let known = [
        (runtime_name("claude")?, claude_detected),
        (runtime_name("codex")?, codex_detected),
        (runtime_name("copilot")?, copilot_detected),
        (runtime_name("cursor")?, cursor_detected),
        (runtime_name("gemini")?, gemini_detected),
    ];
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
        EntityType::Rule,
        EntityType::Prompt,
        EntityType::Command,
        EntityType::Hook,
        EntityType::McpBinding,
        EntityType::PermissionPolicy,
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

fn doctor_detect_runtime(
    repo_root: &Path,
    adapters: &dyn AdapterRegistry,
    runtime: &RuntimeName,
    findings: &mut Vec<String>,
) -> bool {
    match adapters.detect(runtime, repo_root) {
        Ok(response) => response.present,
        Err(error) => {
            findings.push(format!(
                "adapter_{}_detect_error: {error}",
                runtime.as_str()
            ));
            false
        }
    }
}

fn adapter_mode_name(mode: AdapterMode) -> &'static str {
    match mode {
        AdapterMode::Bundled => "bundled",
    }
}

fn doctor_codex_surface_findings(repo_root: &Path) -> Result<Vec<String>> {
    let mut findings = Vec::new();
    let config = repo_root.join(".codex/config.toml");
    let config_metadata = match fs::symlink_metadata(&config) {
        Ok(metadata) => Some(metadata),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read metadata",
                path: config.clone(),
                source,
            });
        }
    };
    if config_metadata.is_some_and(|metadata| metadata.is_file()) {
        let content = fs::read_to_string(&config).map_err(|source| PipelineError::Io {
            action: "read file",
            path: config.clone(),
            source,
        })?;
        match content.parse::<toml::Value>() {
            Ok(value)
                if value
                    .as_table()
                    .is_some_and(|table| table.contains_key("hooks")) =>
            {
                findings.push(
                    "codex_inline_config_hooks: read-only diagnostic; .codex/config.toml hooks are not emitted".to_string(),
                );
            }
            Ok(_) => {}
            Err(source) => findings.push(format!(
                "codex_config_toml: invalid TOML at {}: {source}",
                config.display()
            )),
        }
    }

    let rules = count_files_with_extension(&repo_root.join(".codex/rules"), "rules")?;
    if rules > 0 {
        findings.push(format!(
            "codex_experimental_rules: read-only diagnostic; {rules} file(s) are not emitted"
        ));
    }
    let prompts = count_files(&repo_root.join(".codex/prompts"))?;
    if prompts > 0 {
        findings.push(format!(
            "codex_custom_prompts: deferred; {prompts} file(s) are not emitted"
        ));
    }
    let commands = count_files(&repo_root.join(".codex/commands"))?;
    if commands > 0 {
        findings.push(format!(
            "codex_project_commands: deferred; {commands} file(s) are not emitted"
        ));
    }
    Ok(findings)
}

fn doctor_cursor_surface_findings(repo_root: &Path) -> Result<Vec<String>> {
    let mut findings = Vec::new();
    let skills = count_files(&repo_root.join(".cursor/skills"))?;
    if skills > 0 {
        findings.push(format!(
            "cursor_skills: deferred; {skills} file(s) are not emitted"
        ));
    }
    if regular_file_exists(&repo_root.join(".cursor/hooks.json"))? {
        findings.push("cursor_hooks: deferred; .cursor/hooks.json is not emitted".to_string());
    }
    let commands = count_files(&repo_root.join(".cursor/commands"))?;
    if commands > 0 {
        findings.push(format!(
            "cursor_commands: deferred; {commands} file(s) are not emitted"
        ));
    }
    let subagents = count_files(&repo_root.join(".cursor/agents"))?;
    if subagents > 0 {
        findings.push(format!(
            "cursor_subagents: deferred; {subagents} file(s) are not emitted"
        ));
    }
    if regular_file_exists(&repo_root.join(".cursor/mcp.json"))? {
        findings.push("cursor_mcp: deferred; .cursor/mcp.json is not emitted".to_string());
    }
    Ok(findings)
}

fn doctor_cursor_rule_findings(repo_root: &Path) -> Result<Vec<String>> {
    let mut findings = Vec::new();
    collect_cursor_rule_findings(repo_root, &repo_root.join(".cursor/rules"), &mut findings)?;
    Ok(findings)
}

fn doctor_copilot_surface_findings(repo_root: &Path) -> Result<Vec<String>> {
    let mut findings = Vec::new();
    collect_copilot_instruction_findings(
        repo_root,
        &repo_root.join(".github/instructions"),
        &mut findings,
    )?;
    collect_copilot_prompt_findings(repo_root, &repo_root.join(".github/prompts"), &mut findings)?;
    collect_copilot_skill_findings(
        repo_root,
        &repo_root.join(".github/skills"),
        false,
        &mut findings,
    )?;
    collect_copilot_skill_findings(
        repo_root,
        &repo_root.join(".agents/skills"),
        true,
        &mut findings,
    )?;
    collect_copilot_agent_findings(repo_root, &repo_root.join(".github/agents"), &mut findings)?;

    let hooks = count_files(&repo_root.join(".github/hooks"))?;
    if hooks > 0 {
        findings.push(format!(
            "copilot_hooks: deferred; {hooks} file(s) are not emitted"
        ));
    }
    if regular_file_exists(&repo_root.join("mcp/repository-mcp-settings.json"))? {
        findings.push(
            "copilot_repository_mcp: deferred; repository MCP settings are not emitted".to_string(),
        );
    }
    if regular_file_exists(&repo_root.join(".github/workflows/copilot-setup-steps.yml"))? {
        findings.push(
            "copilot_setup_steps: deferred; Copilot setup steps configure execution environment and must never be emitted"
                .to_string(),
        );
    }
    if regular_file_exists(&repo_root.join("environment/agent-environment.json"))? {
        findings.push(
            "copilot_agent_environment: deferred; Copilot agent environment variables and secrets are deferred and must never be emitted"
                .to_string(),
        );
    }
    Ok(findings)
}

fn doctor_gemini_surface_findings(repo_root: &Path) -> Result<Vec<String>> {
    let mut findings = Vec::new();
    collect_gemini_command_findings(
        repo_root,
        &repo_root.join(".gemini/commands"),
        &mut findings,
    )?;
    collect_gemini_skill_findings(
        repo_root,
        &repo_root.join(".gemini/skills"),
        false,
        &mut findings,
    )?;
    collect_gemini_skill_findings(
        repo_root,
        &repo_root.join(".agents/skills"),
        true,
        &mut findings,
    )?;

    let settings = repo_root.join(".gemini/settings.json");
    if regular_file_exists(&settings)? {
        match read_json::<serde_json::Value>(&settings) {
            Ok(value) => {
                if value.get("mcpServers").is_some() || value.get("mcp").is_some() {
                    findings.push(
                        "gemini_project_mcp: read-only diagnostic; project MCP settings are diagnostics-only and not emitted"
                            .to_string(),
                    );
                }
                if value.get("policyPaths").is_some() || value.get("adminPolicyPaths").is_some() {
                    findings.push(
                        "gemini_policy_settings: read-only diagnostic; project policy settings are diagnostics-only and not emitted"
                            .to_string(),
                    );
                }
                if value
                    .get("context")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|context| context.contains_key("fileName"))
                {
                    findings.push(
                        "gemini_custom_context_filenames: deferred; custom context filenames are detected but only GEMINI.md is emitted"
                            .to_string(),
                    );
                }
                if value.get("hooks").is_some() {
                    findings.push(
                        "gemini_hooks: deferred; Gemini hooks are deferred and must never be emitted"
                            .to_string(),
                    );
                }
            }
            Err(source) => findings.push(format!(
                "gemini_settings_invalid: {}: {source}",
                relative_to(repo_root, &settings).display()
            )),
        }
    }

    let subagents = count_files(&repo_root.join(".gemini/agents"))?;
    if subagents > 0 {
        findings.push(format!(
            "gemini_subagents: deferred; {subagents} file(s) are not emitted"
        ));
    }
    let hooks = count_files(&repo_root.join(".gemini/hooks"))?;
    if hooks > 0 {
        findings.push(format!(
            "gemini_hooks: deferred; {hooks} file(s) are not emitted"
        ));
    }
    let extensions = count_files(&repo_root.join(".gemini/extensions"))?;
    if extensions > 0 {
        findings.push(format!(
            "gemini_extensions: deferred; {extensions} file(s) are not emitted"
        ));
    }
    if regular_file_exists(&repo_root.join("gemini-extension.json"))? {
        findings
            .push("gemini_extensions: deferred; gemini-extension.json is not emitted".to_string());
    }
    Ok(findings)
}

fn collect_gemini_command_findings(
    repo_root: &Path,
    dir: &Path,
    findings: &mut Vec<String>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read metadata",
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        findings.push(format!(
            "gemini_command_invalid: {}: symlinked path is not supported",
            relative_to(repo_root, dir).display()
        ));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)
        .map_err(|source| PipelineError::Io {
            action: "read directory",
            path: dir.to_path_buf(),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| PipelineError::Io {
            action: "read directory entry",
            path: dir.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| PipelineError::Io {
            action: "read file type",
            path: path.clone(),
            source,
        })?;
        if file_type.is_symlink() {
            findings.push(format!(
                "gemini_command_invalid: {}: symlinked path is not supported",
                relative_to(repo_root, &path).display()
            ));
            continue;
        }
        if file_type.is_dir() {
            collect_gemini_command_findings(repo_root, &path, findings)?;
            continue;
        }
        if !file_type.is_file() || path.extension().and_then(|value| value.to_str()) != Some("toml")
        {
            continue;
        }
        let relative = relative_to(repo_root, &path);
        let content = fs::read_to_string(&path).map_err(|source| PipelineError::Io {
            action: "read file",
            path: path.clone(),
            source,
        })?;
        match content.parse::<toml::Value>() {
            Ok(toml::Value::Table(table)) => {
                if table
                    .get("prompt")
                    .and_then(toml::Value::as_str)
                    .is_none_or(|prompt| prompt.trim().is_empty())
                {
                    findings.push(format!(
                        "gemini_command_invalid: {}: missing required prompt field",
                        relative.display()
                    ));
                }
            }
            Ok(_) => findings.push(format!(
                "gemini_command_invalid: {}: TOML root must be a table",
                relative.display()
            )),
            Err(source) => findings.push(format!(
                "gemini_command_invalid: {}: failed to parse TOML: {source}",
                relative.display()
            )),
        }
    }
    Ok(())
}

fn collect_gemini_skill_findings(
    repo_root: &Path,
    root: &Path,
    shared: bool,
    findings: &mut Vec<String>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read metadata",
                path: root.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        findings.push(format!(
            "gemini_skill_invalid: {}: symlinked path is not supported",
            relative_to(repo_root, root).display()
        ));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut entries = fs::read_dir(root)
        .map_err(|source| PipelineError::Io {
            action: "read directory",
            path: root.to_path_buf(),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| PipelineError::Io {
            action: "read directory entry",
            path: root.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| PipelineError::Io {
            action: "read file type",
            path: path.clone(),
            source,
        })?;
        if file_type.is_symlink() {
            findings.push(format!(
                "gemini_skill_invalid: {}: symlinked path is not supported",
                relative_to(repo_root, &path).display()
            ));
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        let skill = path.join("SKILL.md");
        if !regular_file_exists(&skill)? {
            continue;
        }
        let content = fs::read_to_string(&skill).map_err(|source| PipelineError::Io {
            action: "read file",
            path: skill.clone(),
            source,
        })?;
        if let Err(source) = split_canonical_markdown(&content) {
            let message = if shared {
                "invalid shared Gemini skill frontmatter must not be overwritten"
            } else {
                "invalid Gemini skill frontmatter must not be overwritten"
            };
            findings.push(format!(
                "gemini_skill_invalid: {}: {message}: {source}",
                relative_to(repo_root, &skill).display()
            ));
        }
    }
    Ok(())
}

fn collect_copilot_instruction_findings(
    repo_root: &Path,
    dir: &Path,
    findings: &mut Vec<String>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read metadata",
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        findings.push(format!(
            "copilot_instruction_invalid: {}: symlinked path is not supported",
            relative_to(repo_root, dir).display()
        ));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)
        .map_err(|source| PipelineError::Io {
            action: "read directory",
            path: dir.to_path_buf(),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| PipelineError::Io {
            action: "read directory entry",
            path: dir.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| PipelineError::Io {
            action: "read file type",
            path: path.clone(),
            source,
        })?;
        if file_type.is_symlink() {
            findings.push(format!(
                "copilot_instruction_invalid: {}: symlinked path is not supported",
                relative_to(repo_root, &path).display()
            ));
            continue;
        }
        if file_type.is_dir() {
            collect_copilot_instruction_findings(repo_root, &path, findings)?;
            continue;
        }
        if !file_type.is_file() || !file_name_ends_with(&path, ".instructions.md") {
            continue;
        }
        let relative = relative_to(repo_root, &path);
        let content = fs::read_to_string(&path).map_err(|source| PipelineError::Io {
            action: "read file",
            path: path.clone(),
            source,
        })?;
        match split_canonical_markdown(&content) {
            Ok((frontmatter, _)) => {
                if let Some(reason) = copilot_apply_to_diagnostic(&frontmatter) {
                    findings.push(format!(
                        "copilot_instruction_invalid: {}: {reason}",
                        relative.display()
                    ));
                }
            }
            Err(source) => findings.push(format!(
                "copilot_instruction_invalid: {}: invalid path-specific instruction frontmatter must not be overwritten: {source}",
                relative.display()
            )),
        }
    }
    Ok(())
}

fn collect_copilot_prompt_findings(
    repo_root: &Path,
    dir: &Path,
    findings: &mut Vec<String>,
) -> Result<()> {
    collect_copilot_markdown_file_findings(
        repo_root,
        dir,
        ".prompt.md",
        "copilot_prompt_invalid",
        "invalid prompt frontmatter must not be overwritten",
        findings,
    )
}

fn collect_copilot_agent_findings(
    repo_root: &Path,
    dir: &Path,
    findings: &mut Vec<String>,
) -> Result<()> {
    collect_copilot_markdown_file_findings(
        repo_root,
        dir,
        ".md",
        "copilot_custom_agent_invalid",
        "invalid custom-agent frontmatter must not be overwritten",
        findings,
    )
}

fn collect_copilot_markdown_file_findings(
    repo_root: &Path,
    dir: &Path,
    extension_suffix: &str,
    label: &str,
    parse_message: &str,
    findings: &mut Vec<String>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read metadata",
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        findings.push(format!(
            "{label}: {}: symlinked path is not supported",
            relative_to(repo_root, dir).display()
        ));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut entries = fs::read_dir(dir)
        .map_err(|source| PipelineError::Io {
            action: "read directory",
            path: dir.to_path_buf(),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| PipelineError::Io {
            action: "read directory entry",
            path: dir.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| PipelineError::Io {
            action: "read file type",
            path: path.clone(),
            source,
        })?;
        if file_type.is_symlink() {
            findings.push(format!(
                "{label}: {}: symlinked path is not supported",
                relative_to(repo_root, &path).display()
            ));
            continue;
        }
        if !file_type.is_file() || !file_name_ends_with(&path, extension_suffix) {
            continue;
        }
        let relative = relative_to(repo_root, &path);
        let content = fs::read_to_string(&path).map_err(|source| PipelineError::Io {
            action: "read file",
            path: path.clone(),
            source,
        })?;
        if let Err(source) = split_canonical_markdown(&content) {
            findings.push(format!(
                "{label}: {}: {parse_message}: {source}",
                relative.display()
            ));
        }
    }
    Ok(())
}

fn collect_copilot_skill_findings(
    repo_root: &Path,
    root: &Path,
    shared: bool,
    findings: &mut Vec<String>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read metadata",
                path: root.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        findings.push(format!(
            "copilot_skill_invalid: {}: symlinked path is not supported",
            relative_to(repo_root, root).display()
        ));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    let mut entries = fs::read_dir(root)
        .map_err(|source| PipelineError::Io {
            action: "read directory",
            path: root.to_path_buf(),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| PipelineError::Io {
            action: "read directory entry",
            path: root.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| PipelineError::Io {
            action: "read file type",
            path: path.clone(),
            source,
        })?;
        if file_type.is_symlink() {
            findings.push(format!(
                "copilot_skill_invalid: {}: symlinked path is not supported",
                relative_to(repo_root, &path).display()
            ));
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        let skill = path.join("SKILL.md");
        if !regular_file_exists(&skill)? {
            continue;
        }
        let content = fs::read_to_string(&skill).map_err(|source| PipelineError::Io {
            action: "read file",
            path: skill.clone(),
            source,
        })?;
        if let Err(source) = split_canonical_markdown(&content) {
            let message = if shared {
                "invalid shared skill frontmatter must not be overwritten"
            } else {
                "invalid skill frontmatter must not be overwritten"
            };
            findings.push(format!(
                "copilot_skill_invalid: {}: {message}: {source}",
                relative_to(repo_root, &skill).display()
            ));
        }
    }
    Ok(())
}

fn file_name_ends_with(path: &Path, suffix: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(suffix))
}

fn copilot_apply_to_diagnostic(frontmatter: &serde_norway::Mapping) -> Option<&'static str> {
    match frontmatter.get(serde_norway::Value::String("applyTo".to_string())) {
        Some(serde_norway::Value::String(value)) if !value.trim().is_empty() => None,
        Some(serde_norway::Value::String(_)) | None => Some("missing applyTo frontmatter"),
        Some(serde_norway::Value::Sequence(values)) => {
            let mut count = 0;
            for value in values {
                let Some(scope) = value.as_str() else {
                    return Some("applyTo frontmatter must be a string or list of strings");
                };
                if scope.trim().is_empty() {
                    return Some("applyTo frontmatter entries must be non-empty strings");
                }
                count += 1;
            }
            match count {
                0 => Some("missing applyTo frontmatter"),
                1 => None,
                _ => Some("Copilot applyTo with multiple scopes cannot be represented losslessly"),
            }
        }
        Some(_) => Some("applyTo frontmatter must be a string or list of strings"),
    }
}

fn collect_cursor_rule_findings(
    repo_root: &Path,
    dir: &Path,
    findings: &mut Vec<String>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read metadata",
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        findings.push(format!(
            "cursor_rule_invalid: {}: symlinked path is not supported",
            relative_to(repo_root, dir).display()
        ));
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }

    let mut entries = fs::read_dir(dir)
        .map_err(|source| PipelineError::Io {
            action: "read directory",
            path: dir.to_path_buf(),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| PipelineError::Io {
            action: "read directory entry",
            path: dir.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| PipelineError::Io {
            action: "read file type",
            path: path.clone(),
            source,
        })?;
        if file_type.is_symlink() {
            findings.push(format!(
                "cursor_rule_invalid: {}: symlinked path is not supported",
                relative_to(repo_root, &path).display()
            ));
            continue;
        }
        if file_type.is_dir() {
            collect_cursor_rule_findings(repo_root, &path, findings)?;
            continue;
        }
        if !file_type.is_file() || path.extension().and_then(|value| value.to_str()) != Some("mdc")
        {
            continue;
        }
        let relative = relative_to(repo_root, &path);
        let content = fs::read_to_string(&path).map_err(|source| PipelineError::Io {
            action: "read file",
            path: path.clone(),
            source,
        })?;
        if let Err(source) = crate::merge::canonicalize_markdown(&content) {
            findings.push(format!(
                "cursor_rule_invalid: {}: {source}",
                relative.display()
            ));
            continue;
        }
        let (frontmatter, _) = split_canonical_markdown(&content)?;
        if let Some(reason) = cursor_globs_diagnostic(&frontmatter) {
            findings.push(format!(
                "cursor_rule_invalid: {}: {reason}",
                relative.display(),
            ));
        }
    }
    Ok(())
}

fn cursor_globs_diagnostic(frontmatter: &serde_norway::Mapping) -> Option<&'static str> {
    match frontmatter.get("globs") {
        Some(serde_norway::Value::String(value)) if value.trim().is_empty() => {
            Some("Cursor rule globs must contain one non-empty string scope")
        }
        Some(serde_norway::Value::String(_)) => None,
        Some(serde_norway::Value::Sequence(values)) => {
            let count = values
                .iter()
                .filter_map(serde_norway::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .count();
            match count {
                0 => Some("Cursor rule globs must contain one non-empty string scope"),
                1 => None,
                _ => {
                    Some("Cursor rule globs with multiple scopes cannot be represented losslessly")
                }
            }
        }
        Some(_) => Some("Cursor rule globs must be a string or list of strings"),
        None => None,
    }
}

fn regular_file_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !metadata.file_type().is_symlink()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(PipelineError::Io {
            action: "read metadata",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn count_files_with_extension(dir: &Path, extension: &str) -> Result<usize> {
    count_files_matching(dir, &|path| {
        path.extension().and_then(|value| value.to_str()) == Some(extension)
    })
}

fn count_files(dir: &Path) -> Result<usize> {
    count_files_matching(dir, &|_| true)
}

fn count_files_matching(dir: &Path, predicate: &dyn Fn(&Path) -> bool) -> Result<usize> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(PipelineError::Io {
                action: "read metadata",
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_file() {
        return Ok(usize::from(predicate(dir)));
    }
    if !metadata.is_dir() {
        return Ok(0);
    }

    let mut count = 0;
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
        count += count_files_matching(&entry.path(), predicate)?;
    }
    Ok(count)
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
