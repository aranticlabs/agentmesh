//! Bundled Claude adapter entry points.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agentmesh_adapter_sdk_rust::{
    Adapter, AdapterError, AdapterMetadata, FormatTranslation, compose_frontmatter,
    parse_frontmatter, sha256_bytes, skipped_entity, write_atomic,
};
use agentmesh_protocol::{
    AdapterErrorCode, DetectResponse, EmitRequest, EmitResponse, EntityFile, EntityFileEncoding,
    EntityType, ImportFilter, ImportRequest, ImportResponse, ImportedEntity, InstallHooksRequest,
    InstallHooksResponse, InstalledHook, RemoveHooksRequest, RemoveHooksResponse, RuntimeMode,
    SkippedPath,
};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use serde_yml::{Mapping as YamlMapping, Value as YamlValue};

const SUPPORTED_ENTITIES: &[EntityType] = &[
    EntityType::Instructions,
    EntityType::Skill,
    EntityType::Subagent,
];

const ALLOWED_READ_PATHS: &[&str] = &[".claude/**", "CLAUDE.md"];
const ALLOWED_WRITE_PATHS: &[&str] = &[".claude/**", "CLAUDE.md"];
const MARKDOWN_FORMATS: &[&str] = &["markdown"];
const FORMAT_TRANSLATIONS: &[FormatTranslation] = &[FormatTranslation {
    entity_type: EntityType::Subagent,
    formats: MARKDOWN_FORMATS,
}];

/// Claude adapter handle.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeAdapter;

impl Adapter for ClaudeAdapter {
    fn metadata(&self) -> AdapterMetadata {
        metadata()
    }

    fn detect(&self, workspace_root: &Path) -> agentmesh_adapter_sdk_rust::Result<DetectResponse> {
        let evidence = [
            workspace_root.join(".claude"),
            workspace_root.join(".claude/skills"),
            workspace_root.join(".claude/agents"),
            workspace_root.join("CLAUDE.md"),
        ];
        let files = evidence
            .iter()
            .filter(|path| path.exists())
            .filter_map(|path| workspace_relative(workspace_root, path).ok())
            .collect::<Vec<_>>();

        Ok(DetectResponse {
            present: !files.is_empty(),
            version: None,
            files,
        })
    }

    fn import(&self, request: ImportRequest) -> agentmesh_adapter_sdk_rust::Result<ImportResponse> {
        let workspace_root = workspace_root_for(&request.runtime_dir)?;
        let filter = request.filter.as_ref();
        let mut entities = Vec::new();
        let mut skipped = Vec::new();

        let instructions_path = workspace_root.join("CLAUDE.md");
        if selected(filter, &[PathBuf::from("CLAUDE.md")]) && instructions_path.is_file() {
            entities.push(import_markdown_entity(
                &workspace_root,
                &instructions_path,
                EntityType::Instructions,
                "instructions:root".to_string(),
                Some("root".to_string()),
                PathBuf::from("AGENTS.md"),
                PathBuf::from("CLAUDE.md"),
            )?);
        }

        import_skills(
            &workspace_root,
            &request.runtime_dir.join("skills"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_subagents(
            &workspace_root,
            &request.runtime_dir.join("agents"),
            filter,
            &mut entities,
            &mut skipped,
        )?;

        Ok(ImportResponse { entities, skipped })
    }

    fn emit(&self, request: EmitRequest) -> agentmesh_adapter_sdk_rust::Result<EmitResponse> {
        if request.mode == RuntimeMode::ReadOnly {
            return Ok(EmitResponse {
                files_written: Vec::new(),
                skipped: request
                    .entities
                    .into_iter()
                    .map(|entity| skipped_entity(entity.id, "runtime is read-only"))
                    .collect(),
                partial_fidelity: Vec::new(),
            });
        }

        let workspace_root = workspace_root_for(&request.runtime_dir)?;
        let mut files_written = Vec::new();
        let mut skipped = Vec::new();

        for entity in request.entities {
            match entity.entity_type {
                EntityType::Instructions => {
                    let Some(content) = first_file_content(&entity.files) else {
                        skipped.push(skipped_entity(
                            entity.id,
                            "instructions entity has no files",
                        ));
                        continue;
                    };
                    let rendered = render_markdown_with_overrides(
                        &content,
                        &entity.frontmatter,
                        &entity.overrides,
                    )?;
                    let path = workspace_root.join("CLAUDE.md");
                    write_atomic(&path, rendered.as_bytes())?;
                    files_written.push(PathBuf::from("CLAUDE.md"));
                }
                EntityType::Skill => {
                    let slug = slug_for_entity(&entity.id, &entity.frontmatter);
                    let target_root = request.runtime_dir.join("skills").join(&slug);
                    if entity.files.is_empty() {
                        skipped.push(skipped_entity(entity.id, "skill entity has no files"));
                        continue;
                    }

                    for (file_path, file) in &entity.files {
                        let Some(relative) = skill_runtime_file(file_path, &slug) else {
                            skipped.push(skipped_entity(
                                entity.id.clone(),
                                format!("unsafe skill file path {}", file_path.display()),
                            ));
                            continue;
                        };
                        let mut bytes = entity_file_bytes(file_path, file)?;
                        if relative == Path::new("SKILL.md") {
                            let content = entity_file_text(file_path, file)?;
                            let rendered = render_markdown_with_overrides(
                                &content,
                                &entity.frontmatter,
                                &entity.overrides,
                            )?;
                            bytes = rendered.into_bytes();
                        }
                        let target = target_root.join(&relative);
                        write_atomic(&target, &bytes)?;
                        files_written.push(workspace_relative(&workspace_root, &target)?);
                    }
                }
                EntityType::Subagent => {
                    let slug = slug_for_entity(&entity.id, &entity.frontmatter);
                    let Some(content) = first_file_content(&entity.files) else {
                        skipped.push(skipped_entity(entity.id, "subagent entity has no files"));
                        continue;
                    };
                    let rendered = render_markdown_with_overrides(
                        &content,
                        &entity.frontmatter,
                        &entity.overrides,
                    )?;
                    let target = request
                        .runtime_dir
                        .join("agents")
                        .join(format!("{slug}.md"));
                    write_atomic(&target, rendered.as_bytes())?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
            }
        }

        Ok(EmitResponse {
            files_written,
            skipped,
            partial_fidelity: Vec::new(),
        })
    }

    fn install_hooks(
        &self,
        request: InstallHooksRequest,
    ) -> agentmesh_adapter_sdk_rust::Result<InstallHooksResponse> {
        if !request.agentmesh_binary_path.is_absolute() {
            return Err(AdapterError::rpc(
                AdapterErrorCode::HookInstallFailed,
                "agentmesh_binary_path must be absolute",
            ));
        }

        let workspace_root = workspace_root_for(&request.runtime_dir)?;
        let overlay = request.runtime_dir.join("settings.local.json");
        let matcher = append_matcher("Edit|Write|MultiEdit", request.matcher_extra.as_deref());
        let command = format!(
            "{} sync --trigger=claude-hook --silent",
            request.agentmesh_binary_path.display()
        );
        let mut value = read_json_object(&overlay)?;
        let post_tool_use = ensure_hook_array(&mut value, &["hooks", "PostToolUse"])?;

        if let Some(index) = find_hook_group(post_tool_use, &command) {
            return Ok(InstallHooksResponse {
                hooks_installed: vec![InstalledHook {
                    overlay_file: workspace_relative(&workspace_root, &overlay)?,
                    entry_path: format!("$.hooks.PostToolUse[{index}]"),
                    command,
                    matcher,
                }],
                fallback_needed: false,
                fallback_reason: None,
            });
        }

        post_tool_use.push(json!({
            "matcher": matcher,
            "hooks": [{
                "type": "command",
                "command": command,
            }],
        }));
        let index = post_tool_use.len() - 1;
        write_json_pretty(&overlay, &value)?;

        Ok(InstallHooksResponse {
            hooks_installed: vec![InstalledHook {
                overlay_file: workspace_relative(&workspace_root, &overlay)?,
                entry_path: format!("$.hooks.PostToolUse[{index}]"),
                command,
                matcher,
            }],
            fallback_needed: false,
            fallback_reason: None,
        })
    }

    fn remove_hooks(
        &self,
        request: RemoveHooksRequest,
    ) -> agentmesh_adapter_sdk_rust::Result<RemoveHooksResponse> {
        let overlay = request.runtime_dir.join("settings.local.json");
        if !overlay.exists() {
            return Ok(RemoveHooksResponse {
                ok: false,
                removed_count: 0,
                error: Some("Claude hook overlay does not exist".to_string()),
            });
        }

        let mut value = read_json_object(&overlay)?;
        let Some(post_tool_use) = find_hook_array_mut(&mut value, &["hooks", "PostToolUse"]) else {
            return Ok(RemoveHooksResponse {
                ok: false,
                removed_count: 0,
                error: Some("Claude PostToolUse hook array not found".to_string()),
            });
        };

        let mut removed = remove_recorded_entries(
            post_tool_use,
            &request.entry_paths,
            "$.hooks.PostToolUse",
            "claude-hook",
        );
        if removed == 0 {
            removed = remove_matching_entries(post_tool_use, "claude-hook");
        }

        if removed == 0 {
            return Ok(RemoveHooksResponse {
                ok: false,
                removed_count: 0,
                error: Some("AgentMesh Claude hook entry not found".to_string()),
            });
        }

        write_json_pretty(&overlay, &value)?;
        Ok(RemoveHooksResponse {
            ok: true,
            removed_count: removed,
            error: None,
        })
    }
}

/// Returns static metadata for the Claude adapter.
#[must_use]
pub const fn metadata() -> AdapterMetadata {
    AdapterMetadata {
        name: "claude",
        runtime_dir: ".claude",
        supported_entities: SUPPORTED_ENTITIES,
        allowed_read_paths: ALLOWED_READ_PATHS,
        allowed_write_paths: ALLOWED_WRITE_PATHS,
        format_translations: FORMAT_TRANSLATIONS,
    }
}

fn import_skills(
    workspace_root: &Path,
    skills_root: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    if !skills_root.is_dir() {
        return Ok(());
    }

    for entry in read_dir_sorted(skills_root)? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            skipped.push(SkippedPath {
                path: workspace_relative(workspace_root, &path)?,
                reason: "skill directory name is not UTF-8".to_string(),
            });
            continue;
        };
        if name.starts_with('.') {
            skipped.push(SkippedPath {
                path: workspace_relative(workspace_root, &path)?,
                reason: "hidden skill directory is treated as a draft".to_string(),
            });
            continue;
        }

        let source_path = path.join("SKILL.md");
        let source_relative = workspace_relative(workspace_root, &source_path)?;
        let skill_relative = workspace_relative(workspace_root, &path)?;
        if !selected(filter, &[source_relative.clone(), skill_relative]) || !source_path.is_file() {
            continue;
        }

        let slug = slugify(name);
        let mut files = BTreeMap::new();
        collect_entity_files(&path, &path, &mut files)?;
        let content = read_to_string(&source_path)?;
        let frontmatter = frontmatter_json(&content)?;

        entities.push(ImportedEntity {
            id: format!("skill:{slug}"),
            entity_type: EntityType::Skill,
            scope: None,
            canonical_path: PathBuf::from("skills").join(&slug).join("SKILL.md"),
            canonical_sha256: hash_files(&files),
            files,
            frontmatter,
            source_path: source_relative,
            source_mtime: max_mtime_string(&path)?,
        });
    }

    Ok(())
}

fn import_subagents(
    workspace_root: &Path,
    agents_root: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    if !agents_root.is_dir() {
        return Ok(());
    }

    for entry in read_dir_sorted(agents_root)? {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
            continue;
        }
        let source_relative = workspace_relative(workspace_root, &path)?;
        if !selected(filter, std::slice::from_ref(&source_relative)) {
            continue;
        }

        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            skipped.push(SkippedPath {
                path: source_relative,
                reason: "subagent file name is not UTF-8".to_string(),
            });
            continue;
        };
        let slug = slugify(stem);

        entities.push(import_markdown_entity(
            workspace_root,
            &path,
            EntityType::Subagent,
            format!("subagent:{slug}"),
            None,
            PathBuf::from("agents").join(format!("{slug}.md")),
            source_relative,
        )?);
    }

    Ok(())
}

fn import_markdown_entity(
    workspace_root: &Path,
    path: &Path,
    entity_type: EntityType,
    id: String,
    scope: Option<String>,
    canonical_path: PathBuf,
    source_path: PathBuf,
) -> agentmesh_adapter_sdk_rust::Result<ImportedEntity> {
    let content = read_to_string(path)?;
    let frontmatter = frontmatter_json(&content)?;
    let files = BTreeMap::from([(
        canonical_path
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| canonical_path.clone()),
        EntityFile::utf8(content.clone()),
    )]);

    Ok(ImportedEntity {
        id,
        entity_type,
        scope,
        canonical_path,
        files,
        frontmatter,
        canonical_sha256: sha256_bytes(content.as_bytes()),
        source_path,
        source_mtime: mtime_string(
            &workspace_root.join(path.strip_prefix(workspace_root).unwrap_or(path)),
        )?,
    })
}

fn collect_entity_files(
    root: &Path,
    dir: &Path,
    files: &mut BTreeMap<PathBuf, EntityFile>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    for entry in read_dir_sorted(dir)? {
        let path = entry.path();
        if path.is_dir() {
            collect_entity_files(root, &path, files)?;
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let relative = path.strip_prefix(root).map_err(|_| {
            AdapterError::rpc(
                AdapterErrorCode::WorkspaceOutsideBound,
                format!("{} is outside {}", path.display(), root.display()),
            )
        })?;
        files.insert(relative.to_path_buf(), read_entity_file(&path)?);
    }

    Ok(())
}

fn first_file_content(files: &BTreeMap<PathBuf, EntityFile>) -> Option<String> {
    for key in [
        Path::new("SKILL.md"),
        Path::new("AGENTS.md"),
        Path::new("CLAUDE.md"),
    ] {
        if let Some(content) = files.get(key).and_then(file_text) {
            return Some(content);
        }
    }
    files.values().find_map(file_text)
}

fn render_markdown_with_overrides(
    content: &str,
    frontmatter: &BTreeMap<String, JsonValue>,
    overrides: &BTreeMap<String, JsonValue>,
) -> agentmesh_adapter_sdk_rust::Result<String> {
    if frontmatter.is_empty() && overrides.is_empty() && !content.starts_with("---\n") {
        return Ok(content.to_string());
    }
    let mut document = parse_frontmatter(content)?;
    for (key, value) in frontmatter {
        document
            .frontmatter
            .insert(YamlValue::String(key.clone()), json_to_yaml(value)?);
    }
    for (key, value) in overrides {
        document
            .frontmatter
            .insert(YamlValue::String(key.clone()), json_to_yaml(value)?);
    }
    compose_frontmatter(&document)
}

fn frontmatter_json(
    content: &str,
) -> agentmesh_adapter_sdk_rust::Result<BTreeMap<String, JsonValue>> {
    let document = parse_frontmatter(content)?;
    yaml_mapping_to_json(&document.frontmatter)
}

fn yaml_mapping_to_json(
    mapping: &YamlMapping,
) -> agentmesh_adapter_sdk_rust::Result<BTreeMap<String, JsonValue>> {
    let json_value =
        serde_json::to_value(YamlValue::Mapping(mapping.clone())).map_err(|source| {
            AdapterError::rpc(
                AdapterErrorCode::FormatTranslationFailed,
                format!("failed to convert YAML frontmatter to JSON: {source}"),
            )
        })?;
    let Some(object) = json_value.as_object() else {
        return Ok(BTreeMap::new());
    };
    Ok(object
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect())
}

fn json_to_yaml(value: &JsonValue) -> agentmesh_adapter_sdk_rust::Result<YamlValue> {
    serde_yml::from_str(&value.to_string()).map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("failed to convert JSON value to YAML: {source}"),
        )
    })
}

fn skill_runtime_file(path: &Path, slug: &str) -> Option<PathBuf> {
    if !is_safe_relative(path) {
        return None;
    }

    let canonical_prefix = Path::new("skills").join(slug);
    if let Ok(stripped) = path.strip_prefix(&canonical_prefix) {
        return Some(stripped.to_path_buf());
    }
    Some(path.to_path_buf())
}

fn slug_for_entity(id: &str, frontmatter: &BTreeMap<String, JsonValue>) -> String {
    frontmatter
        .get("name")
        .and_then(JsonValue::as_str)
        .map(slugify)
        .unwrap_or_else(|| {
            id.split_once(':')
                .map(|(_, slug)| slugify(slug))
                .unwrap_or_else(|| "unnamed".to_string())
        })
}

fn slugify(value: &str) -> String {
    let mut output = String::new();
    let mut last_was_separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            output.push(character);
            last_was_separator = false;
        } else if !last_was_separator && !output.is_empty() {
            output.push('-');
            last_was_separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "unnamed".to_string()
    } else {
        output
    }
}

fn hash_files(files: &BTreeMap<PathBuf, EntityFile>) -> String {
    let mut bytes = Vec::new();
    for (path, file) in files {
        bytes.extend_from_slice(path.as_os_str().as_encoded_bytes());
        bytes.push(0);
        bytes.extend_from_slice(file.encoding.as_str().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(file.content.as_bytes());
        bytes.push(0);
    }
    sha256_bytes(&bytes)
}

fn selected(filter: Option<&ImportFilter>, candidates: &[PathBuf]) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    if filter.changed_paths.is_empty() {
        return true;
    }
    filter.changed_paths.iter().any(|changed| {
        candidates
            .iter()
            .any(|candidate| changed == candidate || changed.starts_with(candidate))
    })
}

fn read_dir_sorted(path: &Path) -> agentmesh_adapter_sdk_rust::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .map_err(|source| AdapterError::Io {
            action: "read directory",
            path: path.to_path_buf(),
            source,
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|source| AdapterError::Io {
            action: "read directory entry",
            path: path.to_path_buf(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.path());
    Ok(entries)
}

fn read_to_string(path: &Path) -> agentmesh_adapter_sdk_rust::Result<String> {
    fs::read_to_string(path).map_err(|source| AdapterError::Io {
        action: "read file",
        path: path.to_path_buf(),
        source,
    })
}

fn read_entity_file(path: &Path) -> agentmesh_adapter_sdk_rust::Result<EntityFile> {
    fs::read(path)
        .map(EntityFile::from_bytes)
        .map_err(|source| AdapterError::Io {
            action: "read file",
            path: path.to_path_buf(),
            source,
        })
}

fn file_text(file: &EntityFile) -> Option<String> {
    match file.encoding {
        EntityFileEncoding::Utf8 => Some(file.content.clone()),
        EntityFileEncoding::Base64 => None,
    }
}

fn entity_file_text(path: &Path, file: &EntityFile) -> agentmesh_adapter_sdk_rust::Result<String> {
    file_text(file).ok_or_else(|| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("{} must be UTF-8 text", path.display()),
        )
    })
}

fn entity_file_bytes(
    path: &Path,
    file: &EntityFile,
) -> agentmesh_adapter_sdk_rust::Result<Vec<u8>> {
    file.decode_bytes().map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("failed to decode {}: {source}", path.display()),
        )
    })
}

fn write_json_pretty(path: &Path, value: &JsonValue) -> agentmesh_adapter_sdk_rust::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::HookInstallFailed,
            format!("failed to serialize hook JSON: {source}"),
        )
    })?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

fn read_json_object(path: &Path) -> agentmesh_adapter_sdk_rust::Result<JsonValue> {
    if !path.exists() {
        return Ok(JsonValue::Object(JsonMap::new()));
    }
    let content = read_to_string(path)?;
    let value = serde_json::from_str::<JsonValue>(&content).map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::HookInstallFailed,
            format!("failed to parse hook overlay JSON: {source}"),
        )
    })?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(AdapterError::rpc(
            AdapterErrorCode::HookInstallFailed,
            "hook overlay root must be a JSON object",
        ))
    }
}

fn ensure_hook_array<'a>(
    value: &'a mut JsonValue,
    path: &[&str],
) -> agentmesh_adapter_sdk_rust::Result<&'a mut Vec<JsonValue>> {
    let mut current = value;
    for key in &path[..path.len() - 1] {
        let Some(object) = current.as_object_mut() else {
            return Err(AdapterError::rpc(
                AdapterErrorCode::HookInstallFailed,
                "hook overlay path must contain JSON objects",
            ));
        };
        current = object
            .entry((*key).to_string())
            .or_insert_with(|| JsonValue::Object(JsonMap::new()));
    }

    let final_key = path[path.len() - 1];
    let Some(object) = current.as_object_mut() else {
        return Err(AdapterError::rpc(
            AdapterErrorCode::HookInstallFailed,
            "hook overlay path must contain JSON objects",
        ));
    };
    let entry = object
        .entry(final_key.to_string())
        .or_insert_with(|| JsonValue::Array(Vec::new()));
    entry.as_array_mut().ok_or_else(|| {
        AdapterError::rpc(
            AdapterErrorCode::HookInstallFailed,
            format!("hook overlay field `{final_key}` must be an array"),
        )
    })
}

fn find_hook_array_mut<'a>(
    value: &'a mut JsonValue,
    path: &[&str],
) -> Option<&'a mut Vec<JsonValue>> {
    let mut current = value;
    for key in &path[..path.len() - 1] {
        current = current.as_object_mut()?.get_mut(*key)?;
    }
    current
        .as_object_mut()?
        .get_mut(path[path.len() - 1])?
        .as_array_mut()
}

fn find_hook_group(entries: &[JsonValue], command: &str) -> Option<usize> {
    entries
        .iter()
        .position(|entry| group_contains_command(entry, command))
}

fn group_contains_command(entry: &JsonValue, command: &str) -> bool {
    entry
        .get("hooks")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .any(|hook| hook.get("command").and_then(JsonValue::as_str) == Some(command))
}

fn remove_recorded_entries(
    entries: &mut Vec<JsonValue>,
    entry_paths: &[String],
    prefix: &str,
    trigger: &str,
) -> u32 {
    let mut indices = entry_paths
        .iter()
        .filter_map(|entry_path| parse_entry_index(entry_path, prefix))
        .filter(|index| {
            entries
                .get(*index)
                .is_some_and(|entry| group_has_trigger(entry, trigger))
        })
        .collect::<Vec<_>>();
    indices.sort_unstable();
    indices.dedup();

    let removed = indices.len() as u32;
    for index in indices.into_iter().rev() {
        entries.remove(index);
    }
    removed
}

fn remove_matching_entries(entries: &mut Vec<JsonValue>, trigger: &str) -> u32 {
    let original_len = entries.len();
    entries.retain(|entry| !group_has_trigger(entry, trigger));
    (original_len - entries.len()) as u32
}

fn group_has_trigger(entry: &JsonValue, trigger: &str) -> bool {
    entry
        .get("hooks")
        .and_then(JsonValue::as_array)
        .into_iter()
        .flatten()
        .any(|hook| {
            hook.get("command")
                .and_then(JsonValue::as_str)
                .is_some_and(|command| command.contains("agentmesh") && command.contains(trigger))
        })
}

fn parse_entry_index(entry_path: &str, prefix: &str) -> Option<usize> {
    entry_path
        .strip_prefix(prefix)?
        .strip_prefix('[')?
        .strip_suffix(']')?
        .parse()
        .ok()
}

fn append_matcher(default: &str, extra: Option<&str>) -> String {
    match extra.map(str::trim).filter(|extra| !extra.is_empty()) {
        Some(extra) => format!("{default}|{extra}"),
        None => default.to_string(),
    }
}

fn workspace_root_for(runtime_dir: &Path) -> agentmesh_adapter_sdk_rust::Result<PathBuf> {
    runtime_dir.parent().map(Path::to_path_buf).ok_or_else(|| {
        AdapterError::rpc(
            AdapterErrorCode::WorkspaceOutsideBound,
            "runtime_dir must have a workspace parent",
        )
    })
}

fn workspace_relative(
    workspace_root: &Path,
    path: &Path,
) -> agentmesh_adapter_sdk_rust::Result<PathBuf> {
    path.strip_prefix(workspace_root)
        .map(Path::to_path_buf)
        .map_err(|_| {
            AdapterError::rpc(
                AdapterErrorCode::WorkspaceOutsideBound,
                format!("{} is outside {}", path.display(), workspace_root.display()),
            )
        })
}

fn is_safe_relative(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn max_mtime_string(path: &Path) -> agentmesh_adapter_sdk_rust::Result<String> {
    if path.is_file() {
        return mtime_string(path);
    }
    let mut newest = UNIX_EPOCH;
    for entry in read_dir_sorted(path)? {
        let entry_path = entry.path();
        let modified = if entry_path.is_dir() {
            system_time_from_string(&max_mtime_string(&entry_path)?)
        } else {
            fs::metadata(&entry_path)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH)
        };
        if modified > newest {
            newest = modified;
        }
    }
    Ok(format_system_time(newest))
}

fn mtime_string(path: &Path) -> agentmesh_adapter_sdk_rust::Result<String> {
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_err(|source| AdapterError::Io {
            action: "read metadata",
            path: path.to_path_buf(),
            source,
        })?;
    Ok(format_system_time(modified))
}

fn format_system_time(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => format!(
            "unix:{}.{:09}Z",
            duration.as_secs(),
            duration.subsec_nanos()
        ),
        Err(_) => "unix:0.000000000Z".to_string(),
    }
}

fn system_time_from_string(value: &str) -> SystemTime {
    let Some(rest) = value.strip_prefix("unix:") else {
        return UNIX_EPOCH;
    };
    let Some((seconds, nanos)) = rest
        .strip_suffix('Z')
        .and_then(|value| value.split_once('.'))
    else {
        return UNIX_EPOCH;
    };
    let Ok(seconds) = seconds.parse::<u64>() else {
        return UNIX_EPOCH;
    };
    let Ok(nanos) = nanos.parse::<u32>() else {
        return UNIX_EPOCH;
    };
    UNIX_EPOCH + std::time::Duration::new(seconds, nanos)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use agentmesh_adapter_sdk_rust::Adapter;
    use agentmesh_protocol::{
        EmitEntity, EmitRequest, EntityFile, EntityFileEncoding, ImportRequest,
        InstallHooksRequest, RemoveHooksRequest, RuntimeMode,
    };
    use serde_json::json;

    use super::ClaudeAdapter;

    fn file(content: &str) -> EntityFile {
        EntityFile {
            content: content.to_string(),
            encoding: EntityFileEncoding::Utf8,
        }
    }

    #[test]
    fn detects_and_imports_claude_runtime_files() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(root.join("CLAUDE.md"), "# Instructions\n");
        write(
            root.join(".claude/skills/security-review/SKILL.md"),
            "---\nname: security-review\ndescription: Security review\n---\nBody\n",
        );
        write_bytes(
            root.join(".claude/skills/security-review/assets/icon.bin"),
            &[0, 159, 146, 150],
        );
        write(
            root.join(".claude/agents/code-reviewer.md"),
            "---\nname: code-reviewer\nmodel: opus\n---\nReview code.\n",
        );

        let adapter = ClaudeAdapter;
        let detected = match adapter.detect(root) {
            Ok(detected) => detected,
            Err(error) => panic!("detect should succeed: {error}"),
        };
        assert!(detected.present);

        let imported = match adapter.import(ImportRequest {
            canonical_dir: root.join(".ai"),
            runtime_dir: root.join(".claude"),
            filter: None,
        }) {
            Ok(imported) => imported,
            Err(error) => panic!("import should succeed: {error}"),
        };

        let ids = imported
            .entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"instructions:root"));
        assert!(ids.contains(&"skill:security-review"));
        assert!(ids.contains(&"subagent:code-reviewer"));
        let skill = imported
            .entities
            .iter()
            .find(|entity| entity.id == "skill:security-review")
            .unwrap_or_else(|| panic!("skill should be imported"));
        let asset = skill
            .files
            .get(Path::new("assets/icon.bin"))
            .unwrap_or_else(|| panic!("binary skill asset should be imported"));
        assert_eq!(asset.encoding, EntityFileEncoding::Base64);
        assert_eq!(
            asset
                .decode_bytes()
                .unwrap_or_else(|error| panic!("asset should decode: {error}")),
            vec![0, 159, 146, 150]
        );
    }

    #[test]
    fn emits_claude_runtime_files() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = ClaudeAdapter;

        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("SKILL.md"),
            file("---\nname: security-review\n---\nBody\n"),
        );
        files.insert(
            PathBuf::from("assets/icon.bin"),
            EntityFile::from_bytes(vec![0, 159, 146, 150]),
        );
        let response = match adapter.emit(EmitRequest {
            runtime_dir: root.join(".claude"),
            mode: RuntimeMode::Managed,
            entities: vec![EmitEntity {
                id: "skill:security-review".to_string(),
                entity_type: agentmesh_protocol::EntityType::Skill,
                scope: None,
                files,
                frontmatter: BTreeMap::new(),
                overrides: BTreeMap::from([("model".to_string(), json!("opus"))]),
            }],
        }) {
            Ok(response) => response,
            Err(error) => panic!("emit should succeed: {error}"),
        };

        assert_eq!(
            response.files_written,
            vec![
                PathBuf::from(".claude/skills/security-review/SKILL.md"),
                PathBuf::from(".claude/skills/security-review/assets/icon.bin"),
            ]
        );
        let content = read(root.join(".claude/skills/security-review/SKILL.md"));
        assert!(content.contains("model: opus"));
        assert_eq!(
            read_bytes(root.join(".claude/skills/security-review/assets/icon.bin")),
            vec![0, 159, 146, 150]
        );
    }

    #[test]
    fn installs_and_removes_claude_hook_additively() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(
            root.join(".claude/settings.local.json"),
            r#"{"hooks":{"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo user"}]}]}}"#,
        );

        let adapter = ClaudeAdapter;
        let installed = match adapter.install_hooks(InstallHooksRequest {
            runtime_dir: root.join(".claude"),
            agentmesh_binary_path: PathBuf::from("/usr/local/bin/agentmesh"),
            matcher_extra: Some("Bash".to_string()),
        }) {
            Ok(installed) => installed,
            Err(error) => panic!("install should succeed: {error}"),
        };

        assert_eq!(
            installed.hooks_installed[0].entry_path,
            "$.hooks.PostToolUse[1]"
        );
        let overlay = read(root.join(".claude/settings.local.json"));
        assert!(overlay.contains("echo user"));
        assert!(overlay.contains("claude-hook"));

        let removed = match adapter.remove_hooks(RemoveHooksRequest {
            runtime_dir: root.join(".claude"),
            entry_paths: vec![installed.hooks_installed[0].entry_path.clone()],
        }) {
            Ok(removed) => removed,
            Err(error) => panic!("remove should succeed: {error}"),
        };

        assert!(removed.ok);
        let overlay = read(root.join(".claude/settings.local.json"));
        assert!(overlay.contains("echo user"));
        assert!(!overlay.contains("claude-hook"));
    }

    #[test]
    fn read_only_emit_skips_without_writing_runtime_files() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = ClaudeAdapter;
        let files = BTreeMap::from([(
            PathBuf::from("SKILL.md"),
            file("---\nname: security-review\n---\nBody\n"),
        )]);

        let response = match adapter.emit(EmitRequest {
            runtime_dir: root.join(".claude"),
            mode: RuntimeMode::ReadOnly,
            entities: vec![EmitEntity {
                id: "skill:security-review".to_string(),
                entity_type: agentmesh_protocol::EntityType::Skill,
                scope: None,
                files,
                frontmatter: BTreeMap::new(),
                overrides: BTreeMap::new(),
            }],
        }) {
            Ok(response) => response,
            Err(error) => panic!("read-only emit should succeed: {error}"),
        };

        assert!(response.files_written.is_empty());
        assert_eq!(response.skipped.len(), 1);
        assert!(
            !root
                .join(".claude/skills/security-review/SKILL.md")
                .exists()
        );
    }

    #[test]
    fn repeated_hook_install_returns_existing_entry_without_duplication() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = ClaudeAdapter;
        let request = InstallHooksRequest {
            runtime_dir: root.join(".claude"),
            agentmesh_binary_path: PathBuf::from("/usr/local/bin/agentmesh"),
            matcher_extra: None,
        };

        let first = match adapter.install_hooks(request.clone()) {
            Ok(installed) => installed,
            Err(error) => panic!("first install should succeed: {error}"),
        };
        let second = match adapter.install_hooks(request) {
            Ok(installed) => installed,
            Err(error) => panic!("second install should succeed: {error}"),
        };
        let overlay = read(root.join(".claude/settings.local.json"));
        let hook_count = overlay.matches("claude-hook").count();

        assert_eq!(
            first.hooks_installed[0].entry_path,
            "$.hooks.PostToolUse[0]"
        );
        assert_eq!(
            second.hooks_installed[0].entry_path,
            "$.hooks.PostToolUse[0]"
        );
        assert_eq!(hook_count, 1);
    }

    fn write(path: impl AsRef<Path>, content: &str) {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                panic!("parent directory should be created: {error}");
            }
        }
        if let Err(error) = fs::write(path, content) {
            panic!("file should be written: {error}");
        }
    }

    fn write_bytes(path: impl AsRef<Path>, content: &[u8]) {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            panic!("parent directory should be created: {error}");
        }
        if let Err(error) = fs::write(path, content) {
            panic!("file should be written: {error}");
        }
    }

    fn read(path: impl AsRef<Path>) -> String {
        match fs::read_to_string(path.as_ref()) {
            Ok(content) => content,
            Err(error) => panic!("file should be readable: {error}"),
        }
    }

    fn read_bytes(path: impl AsRef<Path>) -> Vec<u8> {
        match fs::read(path.as_ref()) {
            Ok(content) => content,
            Err(error) => panic!("file should be readable: {error}"),
        }
    }
}
