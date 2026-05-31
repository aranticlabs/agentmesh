//! Bundled Codex adapter entry points.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentmesh_adapter_sdk_rust::{
    Adapter, AdapterError, AdapterMetadata, FormatTranslation, FrontmatterDocument,
    collect_entity_files, compose_frontmatter, dir_entry_file_type, ensure_hook_array,
    find_hook_array_mut, find_hook_group, hash_files, is_regular_dir, is_regular_file,
    is_safe_relative, max_mtime_string, mtime_string, parse_frontmatter, read_dir_sorted,
    read_json_object, read_to_string, remove_matching_entries, remove_recorded_entries, selected,
    sha256_bytes, skipped_entity, slug_for_entity, slugify, workspace_relative, workspace_root_for,
    write_atomic, write_json_pretty,
};
use agentmesh_protocol::{
    AdapterErrorCode, DetectResponse, EmitRequest, EmitResponse, EntityFile, EntityFileEncoding,
    EntityType, ImportFilter, ImportRequest, ImportResponse, ImportedEntity, InstallHooksRequest,
    InstallHooksResponse, InstalledHook, RemoveHooksRequest, RemoveHooksResponse, RuntimeMode,
    SkippedPath,
};
use serde_json::{Number as JsonNumber, Value as JsonValue, json};
use serde_norway::{Mapping as YamlMapping, Value as YamlValue};

const SUPPORTED_ENTITIES: &[EntityType] = &[
    EntityType::Instructions,
    EntityType::Skill,
    EntityType::Subagent,
];

const ALLOWED_READ_PATHS: &[&str] = &[".codex/**", "AGENTS.md"];
const ALLOWED_WRITE_PATHS: &[&str] = &[".codex/**", "AGENTS.md"];
const SUBAGENT_FORMATS: &[&str] = &["markdown", "toml"];
const FORMAT_TRANSLATIONS: &[FormatTranslation] = &[FormatTranslation {
    entity_type: EntityType::Subagent,
    formats: SUBAGENT_FORMATS,
}];

/// Codex adapter handle.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexAdapter;

impl Adapter for CodexAdapter {
    fn metadata(&self) -> AdapterMetadata {
        metadata()
    }

    fn detect(&self, workspace_root: &Path) -> agentmesh_adapter_sdk_rust::Result<DetectResponse> {
        let evidence = [
            workspace_root.join(".codex"),
            workspace_root.join(".codex/skills"),
            workspace_root.join(".codex/agents"),
            workspace_root.join("AGENTS.md"),
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

        let instructions_path = workspace_root.join("AGENTS.md");
        if selected(filter, &[PathBuf::from("AGENTS.md")])
            && is_regular_file(&workspace_root, &instructions_path)?
        {
            entities.push(import_markdown_entity(
                &instructions_path,
                EntityType::Instructions,
                "instructions:root".to_string(),
                Some("root".to_string()),
                PathBuf::from("AGENTS.md"),
                PathBuf::from("AGENTS.md"),
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
                    let path = workspace_root.join("AGENTS.md");
                    write_atomic(&path, rendered.as_bytes())?;
                    files_written.push(PathBuf::from("AGENTS.md"));
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
                    let rendered =
                        render_toml_subagent(&content, &entity.frontmatter, &entity.overrides)?;
                    let target = request
                        .runtime_dir
                        .join("agents")
                        .join(format!("{slug}.toml"));
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
        let overlay = request.runtime_dir.join("hooks.json");
        let matcher = codex_matcher(request.matcher_extra.as_deref());
        let command = format!(
            "{} sync --trigger=codex-hook --silent",
            request.agentmesh_binary_path.display()
        );
        let mut value = read_json_object(&overlay)?;
        let post_tool_use = ensure_hook_array(&mut value, &["PostToolUse"])?;

        if let Some(index) = find_hook_group(post_tool_use, &command) {
            return Ok(InstallHooksResponse {
                hooks_installed: vec![InstalledHook {
                    overlay_file: workspace_relative(&workspace_root, &overlay)?,
                    entry_path: format!("$.PostToolUse[{index}]"),
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
                "timeout": 5,
                "statusMessage": "AgentMesh sync",
            }],
        }));
        let index = post_tool_use.len() - 1;
        write_json_pretty(&overlay, &value)?;

        Ok(InstallHooksResponse {
            hooks_installed: vec![InstalledHook {
                overlay_file: workspace_relative(&workspace_root, &overlay)?,
                entry_path: format!("$.PostToolUse[{index}]"),
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
        let overlay = request.runtime_dir.join("hooks.json");
        if !overlay.exists() {
            return Ok(RemoveHooksResponse {
                ok: false,
                removed_count: 0,
                error: Some("Codex hook overlay does not exist".to_string()),
            });
        }

        let mut value = read_json_object(&overlay)?;
        let removed = {
            let Some(post_tool_use) = find_hook_array_mut(&mut value, &["PostToolUse"]) else {
                return Ok(RemoveHooksResponse {
                    ok: false,
                    removed_count: 0,
                    error: Some("Codex PostToolUse hook array not found".to_string()),
                });
            };

            let mut removed = remove_recorded_entries(
                post_tool_use,
                &request.entry_paths,
                "$.PostToolUse",
                "codex-hook",
            );
            if removed == 0 {
                removed = remove_matching_entries(post_tool_use, "codex-hook");
            }
            removed
        };

        if removed == 0 {
            return Ok(RemoveHooksResponse {
                ok: false,
                removed_count: 0,
                error: Some("AgentMesh Codex hook entry not found".to_string()),
            });
        }

        if codex_hooks_are_empty(&value) {
            fs::remove_file(&overlay).map_err(|source| AdapterError::Io {
                action: "remove file",
                path: overlay.clone(),
                source,
            })?;
        } else {
            write_json_pretty(&overlay, &value)?;
        }

        Ok(RemoveHooksResponse {
            ok: true,
            removed_count: removed,
            error: None,
        })
    }
}

/// Returns static metadata for the Codex adapter.
#[must_use]
pub const fn metadata() -> AdapterMetadata {
    AdapterMetadata {
        name: "codex",
        runtime_dir: ".codex",
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
    match is_regular_dir(workspace_root, skills_root) {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(error) => {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, skills_root),
                reason: error.to_string(),
            });
            return Ok(());
        }
    }

    for entry in read_dir_sorted(skills_root)? {
        let path = entry.path();
        let file_type = dir_entry_file_type(&entry)?;
        if file_type.is_symlink() {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, &path),
                reason: "symlinked skill path is not supported".to_string(),
            });
            continue;
        }
        if !file_type.is_dir() {
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
        if !selected(filter, &[source_relative.clone(), skill_relative]) {
            continue;
        }
        let source_is_file = match is_regular_file(workspace_root, &source_path) {
            Ok(source_is_file) => source_is_file,
            Err(error) => {
                skipped.push(SkippedPath {
                    path: source_relative,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        if !source_is_file {
            continue;
        }

        let slug = slugify(name);
        let mut files = BTreeMap::new();
        if let Err(error) = collect_entity_files(&path, &path, &mut files) {
            skipped.push(SkippedPath {
                path: workspace_relative(workspace_root, &path)?,
                reason: error.to_string(),
            });
            continue;
        }
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

fn relative_or_path(workspace_root: &Path, path: &Path) -> PathBuf {
    workspace_relative(workspace_root, path).unwrap_or_else(|_| path.to_path_buf())
}

fn import_subagents(
    workspace_root: &Path,
    agents_root: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    match is_regular_dir(workspace_root, agents_root) {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(error) => {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, agents_root),
                reason: error.to_string(),
            });
            return Ok(());
        }
    }

    for entry in read_dir_sorted(agents_root)? {
        let path = entry.path();
        let file_type = dir_entry_file_type(&entry)?;
        if file_type.is_symlink() {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, &path),
                reason: "symlinked subagent path is not supported".to_string(),
            });
            continue;
        }
        if !file_type.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("toml")
        {
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
        entities.push(import_toml_subagent(
            &path,
            format!("subagent:{slug}"),
            PathBuf::from("agents").join(format!("{slug}.md")),
            source_relative,
        )?);
    }

    Ok(())
}

fn import_markdown_entity(
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
        source_mtime: mtime_string(path)?,
    })
}

fn import_toml_subagent(
    path: &Path,
    id: String,
    canonical_path: PathBuf,
    source_path: PathBuf,
) -> agentmesh_adapter_sdk_rust::Result<ImportedEntity> {
    let content = read_to_string(path)?;
    let value = content.parse::<toml::Value>().map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("failed to parse Codex subagent TOML: {source}"),
        )
    })?;
    let Some(table) = value.as_table() else {
        return Err(AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            "Codex subagent TOML root must be a table",
        ));
    };
    let mut frontmatter = table
        .iter()
        .map(|(key, value)| (key.clone(), toml_to_json(value)))
        .collect::<BTreeMap<_, _>>();
    let body = frontmatter
        .get("instructions")
        .or_else(|| frontmatter.get("prompt"))
        .and_then(JsonValue::as_str)
        .map(|body| format!("{body}\n"))
        .unwrap_or_default();
    frontmatter.remove("instructions");
    frontmatter.remove("prompt");
    let markdown = compose_frontmatter(&FrontmatterDocument {
        frontmatter: json_map_to_yaml(&frontmatter)?,
        body,
    })?;
    let files = BTreeMap::from([(
        canonical_path
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| canonical_path.clone()),
        EntityFile::utf8(markdown.clone()),
    )]);

    Ok(ImportedEntity {
        id,
        entity_type: EntityType::Subagent,
        scope: None,
        canonical_path,
        files,
        frontmatter,
        canonical_sha256: sha256_bytes(markdown.as_bytes()),
        source_path,
        source_mtime: mtime_string(path)?,
    })
}

fn first_file_content(files: &BTreeMap<PathBuf, EntityFile>) -> Option<String> {
    for key in [Path::new("SKILL.md"), Path::new("AGENTS.md")] {
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

fn render_toml_subagent(
    content: &str,
    frontmatter: &BTreeMap<String, JsonValue>,
    overrides: &BTreeMap<String, JsonValue>,
) -> agentmesh_adapter_sdk_rust::Result<String> {
    let document = parse_frontmatter(content)?;
    let mut merged = yaml_mapping_to_json(&document.frontmatter)?;
    merged.extend(
        frontmatter
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    merged.extend(
        overrides
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );

    let body = toml_instructions_body(&document.body);
    if !document.body.is_empty()
        && !merged.contains_key("instructions")
        && !merged.contains_key("prompt")
    {
        merged.insert(
            "instructions".to_string(),
            JsonValue::String(body.to_string()),
        );
    }

    let mut table = toml::map::Map::new();
    for (key, value) in merged {
        if let Some(value) = json_to_toml(&value) {
            table.insert(key, value);
        }
    }
    Ok(serialize_toml_table(&table))
}

fn toml_instructions_body(body: &str) -> String {
    body.strip_suffix('\n').unwrap_or(body).to_string()
}

fn serialize_toml_table(table: &toml::map::Map<String, toml::Value>) -> String {
    let mut output = String::new();
    write_toml_table(&mut output, None, table);
    output
}

fn write_toml_table(
    output: &mut String,
    prefix: Option<&str>,
    table: &toml::map::Map<String, toml::Value>,
) {
    let mut nested = Vec::new();
    for (key, value) in table {
        if let toml::Value::Table(child) = value {
            nested.push((key, child));
        } else {
            output.push_str(&quote_toml_key(key));
            output.push_str(" = ");
            output.push_str(&inline_toml_value(value));
            output.push('\n');
        }
    }

    for (key, child) in nested {
        if !output.ends_with("\n\n") {
            output.push('\n');
        }
        let section = match prefix {
            Some(prefix) => format!("{prefix}.{}", quote_toml_key(key)),
            None => quote_toml_key(key),
        };
        output.push('[');
        output.push_str(&section);
        output.push_str("]\n");
        write_toml_table(output, Some(&section), child);
    }
}

fn inline_toml_value(value: &toml::Value) -> String {
    match value {
        toml::Value::String(value) => quote_toml_string(value),
        toml::Value::Integer(value) => value.to_string(),
        toml::Value::Float(value) => value.to_string(),
        toml::Value::Boolean(value) => value.to_string(),
        toml::Value::Datetime(value) => quote_toml_string(&value.to_string()),
        toml::Value::Array(values) => {
            let values = values.iter().map(inline_toml_value).collect::<Vec<_>>();
            format!("[{}]", values.join(", "))
        }
        toml::Value::Table(table) => inline_toml_table(table),
    }
}

fn inline_toml_table(table: &toml::map::Map<String, toml::Value>) -> String {
    if table.is_empty() {
        return "{}".to_string();
    }

    let entries = table
        .iter()
        .map(|(key, value)| format!("{} = {}", quote_toml_key(key), inline_toml_value(value)))
        .collect::<Vec<_>>();
    format!("{{ {} }}", entries.join(", "))
}

fn quote_toml_key(key: &str) -> String {
    if key
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        key.to_string()
    } else {
        quote_toml_string(key)
    }
}

fn quote_toml_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character => output.push(character),
        }
    }
    output.push('"');
    output
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

fn json_map_to_yaml(
    map: &BTreeMap<String, JsonValue>,
) -> agentmesh_adapter_sdk_rust::Result<YamlMapping> {
    let mut mapping = YamlMapping::new();
    for (key, value) in map {
        mapping.insert(YamlValue::String(key.clone()), json_to_yaml(value)?);
    }
    Ok(mapping)
}

fn json_to_yaml(value: &JsonValue) -> agentmesh_adapter_sdk_rust::Result<YamlValue> {
    serde_norway::to_value(value).map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("failed to convert JSON value to YAML: {source}"),
        )
    })
}

fn toml_to_json(value: &toml::Value) -> JsonValue {
    match value {
        toml::Value::String(value) => JsonValue::String(value.clone()),
        toml::Value::Integer(value) => JsonValue::Number(JsonNumber::from(*value)),
        toml::Value::Float(value) => JsonNumber::from_f64(*value)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        toml::Value::Boolean(value) => JsonValue::Bool(*value),
        toml::Value::Datetime(value) => JsonValue::String(value.to_string()),
        toml::Value::Array(values) => JsonValue::Array(values.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => JsonValue::Object(
            table
                .iter()
                .map(|(key, value)| (key.clone(), toml_to_json(value)))
                .collect(),
        ),
    }
}

fn json_to_toml(value: &JsonValue) -> Option<toml::Value> {
    match value {
        JsonValue::Null => None,
        JsonValue::Bool(value) => Some(toml::Value::Boolean(*value)),
        JsonValue::Number(value) => value
            .as_i64()
            .map(toml::Value::Integer)
            .or_else(|| value.as_f64().map(toml::Value::Float)),
        JsonValue::String(value) => Some(toml::Value::String(value.clone())),
        JsonValue::Array(values) => Some(toml::Value::Array(
            values.iter().filter_map(json_to_toml).collect(),
        )),
        JsonValue::Object(values) => {
            let mut table = toml::map::Map::new();
            for (key, value) in values {
                if let Some(value) = json_to_toml(value) {
                    table.insert(key.clone(), value);
                }
            }
            Some(toml::Value::Table(table))
        }
    }
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

fn codex_matcher(extra: Option<&str>) -> String {
    let mut tools = vec!["Edit", "Write", "MultiEdit"];
    if let Some(extra) = extra.map(str::trim).filter(|extra| !extra.is_empty()) {
        tools.push(extra);
    }
    format!("^({})$", tools.join("|"))
}

fn codex_hooks_are_empty(value: &JsonValue) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object
        .iter()
        .all(|(key, value)| key == "PostToolUse" && value.as_array().is_some_and(Vec::is_empty))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use agentmesh_adapter_sdk_rust::{Adapter, canonicalize_frontmatter};
    use agentmesh_protocol::{
        EmitEntity, EmitRequest, EntityFile, EntityFileEncoding, ImportRequest, ImportedEntity,
        InstallHooksRequest, RemoveHooksRequest, RuntimeMode,
    };
    use proptest::prelude::*;
    use serde_json::json;

    use super::CodexAdapter;

    type SemanticEntity = (
        String,
        agentmesh_protocol::EntityType,
        Option<String>,
        BTreeMap<PathBuf, EntityFile>,
        BTreeMap<String, serde_json::Value>,
    );

    fn absolute_agentmesh_binary_path() -> PathBuf {
        match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => panic!("current test executable path should resolve: {error}"),
        }
    }

    fn file(content: &str) -> EntityFile {
        EntityFile {
            content: content.to_string(),
            encoding: EntityFileEncoding::Utf8,
        }
    }

    #[test]
    fn imports_codex_skills_and_toml_subagents() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(root.join("AGENTS.md"), "# Instructions\n");
        write(
            root.join(".codex/skills/security-review/SKILL.md"),
            "---\nname: security-review\n---\nBody\n",
        );
        write_bytes(
            root.join(".codex/skills/security-review/assets/icon.bin"),
            &[0, 159, 146, 150],
        );
        write(
            root.join(".codex/agents/code-reviewer.toml"),
            "name = \"code-reviewer\"\nmodel = \"gpt-5\"\ninstructions = \"Review code.\"\n",
        );

        let adapter = CodexAdapter;
        let imported = match adapter.import(ImportRequest {
            canonical_dir: root.join(".ai"),
            runtime_dir: root.join(".codex"),
            filter: None,
        }) {
            Ok(imported) => imported,
            Err(error) => panic!("import should succeed: {error}"),
        };

        let subagent = imported
            .entities
            .iter()
            .find(|entity| entity.id == "subagent:code-reviewer");
        let Some(subagent) = subagent else {
            panic!("subagent should be imported");
        };
        assert_eq!(subagent.frontmatter.get("model"), Some(&json!("gpt-5")));
        assert_eq!(subagent.frontmatter.get("instructions"), None);
        assert!(
            subagent
                .files
                .values()
                .any(|file| file.content.contains("Review code."))
        );
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
    fn emits_codex_subagent_toml_from_markdown() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = CodexAdapter;
        let files = BTreeMap::from([(
            PathBuf::from("code-reviewer.md"),
            file("---\nname: code-reviewer\nmodel: gpt-5\n---\nReview code.\n"),
        )]);

        let response = match adapter.emit(EmitRequest {
            runtime_dir: root.join(".codex"),
            mode: RuntimeMode::Managed,
            entities: vec![EmitEntity {
                id: "subagent:code-reviewer".to_string(),
                entity_type: agentmesh_protocol::EntityType::Subagent,
                scope: None,
                files,
                frontmatter: BTreeMap::new(),
                overrides: BTreeMap::new(),
            }],
        }) {
            Ok(response) => response,
            Err(error) => panic!("emit should succeed: {error}"),
        };

        assert_eq!(
            response.files_written,
            vec![PathBuf::from(".codex/agents/code-reviewer.toml")]
        );
        let content = read(root.join(".codex/agents/code-reviewer.toml"));
        assert!(content.contains("name = \"code-reviewer\""));
        assert!(content.contains("instructions = \"Review code.\""));
    }

    #[test]
    fn emits_codex_skill_assets() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = CodexAdapter;
        let files = BTreeMap::from([
            (
                PathBuf::from("SKILL.md"),
                file("---\nname: security-review\n---\nBody\n"),
            ),
            (
                PathBuf::from("assets/icon.bin"),
                EntityFile::from_bytes(vec![0, 159, 146, 150]),
            ),
        ]);

        let response = match adapter.emit(EmitRequest {
            runtime_dir: root.join(".codex"),
            mode: RuntimeMode::Managed,
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
            Err(error) => panic!("emit should succeed: {error}"),
        };

        assert_eq!(
            response.files_written,
            vec![
                PathBuf::from(".codex/skills/security-review/SKILL.md"),
                PathBuf::from(".codex/skills/security-review/assets/icon.bin"),
            ]
        );
        assert_eq!(
            read_bytes(root.join(".codex/skills/security-review/assets/icon.bin")),
            vec![0, 159, 146, 150]
        );
    }

    #[test]
    fn installs_and_removes_codex_hook_file() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = CodexAdapter;

        let installed = match adapter.install_hooks(InstallHooksRequest {
            runtime_dir: root.join(".codex"),
            agentmesh_binary_path: absolute_agentmesh_binary_path(),
            matcher_extra: None,
        }) {
            Ok(installed) => installed,
            Err(error) => panic!("install should succeed: {error}"),
        };

        assert_eq!(installed.hooks_installed[0].entry_path, "$.PostToolUse[0]");
        let overlay = read(root.join(".codex/hooks.json"));
        assert!(overlay.contains("codex-hook"));
        assert!(overlay.contains("AgentMesh sync"));

        let removed = match adapter.remove_hooks(RemoveHooksRequest {
            runtime_dir: root.join(".codex"),
            entry_paths: vec![installed.hooks_installed[0].entry_path.clone()],
        }) {
            Ok(removed) => removed,
            Err(error) => panic!("remove should succeed: {error}"),
        };
        assert!(removed.ok);
        assert!(!root.join(".codex/hooks.json").exists());
    }

    #[test]
    fn installs_codex_hook_additively() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(
            root.join(".codex/hooks.json"),
            r#"{"PostToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","command":"echo user"}]}]}"#,
        );
        let adapter = CodexAdapter;

        let installed = match adapter.install_hooks(InstallHooksRequest {
            runtime_dir: root.join(".codex"),
            agentmesh_binary_path: absolute_agentmesh_binary_path(),
            matcher_extra: Some("Bash".to_string()),
        }) {
            Ok(installed) => installed,
            Err(error) => panic!("install should succeed: {error}"),
        };
        assert_eq!(installed.hooks_installed[0].entry_path, "$.PostToolUse[1]");

        let removed = match adapter.remove_hooks(RemoveHooksRequest {
            runtime_dir: root.join(".codex"),
            entry_paths: vec![installed.hooks_installed[0].entry_path.clone()],
        }) {
            Ok(removed) => removed,
            Err(error) => panic!("remove should succeed: {error}"),
        };
        assert!(removed.ok);
        let overlay = read(root.join(".codex/hooks.json"));
        assert!(overlay.contains("echo user"));
        assert!(!overlay.contains("codex-hook"));
    }

    #[test]
    fn subagent_toml_roundtrip_preserves_nested_values() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(
            root.join(".codex/agents/security-reviewer.toml"),
            r#"name = "security-reviewer"
model = "gpt-5"
instructions = "Review code.\nUse structured findings."
tags = ["security", "review"]
constraints = [{ kind = "deny", values = ["unsafe"] }, { kind = "require", metadata = { level = "high" } }]

[metadata]
severity = ["high", "medium"]
"#,
        );

        let adapter = CodexAdapter;
        let imported = match adapter.import(ImportRequest {
            canonical_dir: root.join(".ai"),
            runtime_dir: root.join(".codex"),
            filter: None,
        }) {
            Ok(imported) => imported,
            Err(error) => panic!("import should succeed: {error}"),
        };
        let Some(entity) = imported
            .entities
            .into_iter()
            .find(|entity| entity.id == "subagent:security-reviewer")
        else {
            panic!("subagent should be imported");
        };

        let response = match adapter.emit(EmitRequest {
            runtime_dir: root.join(".codex-roundtrip"),
            mode: RuntimeMode::Managed,
            entities: vec![EmitEntity {
                id: entity.id,
                entity_type: entity.entity_type,
                scope: entity.scope,
                files: entity.files,
                frontmatter: entity.frontmatter,
                overrides: BTreeMap::new(),
            }],
        }) {
            Ok(response) => response,
            Err(error) => panic!("emit should succeed: {error}"),
        };

        assert_eq!(
            response.files_written,
            vec![PathBuf::from(
                ".codex-roundtrip/agents/security-reviewer.toml"
            )]
        );
        let content = read(root.join(".codex-roundtrip/agents/security-reviewer.toml"));
        let parsed = match content.parse::<toml::Value>() {
            Ok(parsed) => parsed,
            Err(error) => panic!("roundtripped TOML should parse: {error}"),
        };
        let Some(table) = parsed.as_table() else {
            panic!("roundtripped TOML should be a table");
        };
        let tags = table
            .get("tags")
            .and_then(toml::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .collect::<Vec<_>>()
            });
        let severity = table
            .get("metadata")
            .and_then(toml::Value::as_table)
            .and_then(|metadata| metadata.get("severity"))
            .and_then(toml::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .collect::<Vec<_>>()
            });
        let constraints = table
            .get("constraints")
            .and_then(toml::Value::as_array)
            .unwrap_or_else(|| panic!("constraints should be preserved"));

        assert_eq!(
            table.get("instructions").and_then(toml::Value::as_str),
            Some("Review code.\nUse structured findings.")
        );
        assert_eq!(tags, Some(vec!["security", "review"]));
        assert_eq!(severity, Some(vec!["high", "medium"]));
        assert_eq!(
            constraints
                .first()
                .and_then(toml::Value::as_table)
                .and_then(|constraint| constraint.get("kind"))
                .and_then(toml::Value::as_str),
            Some("deny")
        );
        assert_eq!(
            constraints
                .get(1)
                .and_then(toml::Value::as_table)
                .and_then(|constraint| constraint.get("metadata"))
                .and_then(toml::Value::as_table)
                .and_then(|metadata| metadata.get("level"))
                .and_then(toml::Value::as_str),
            Some("high")
        );
    }

    #[test]
    fn repeated_hook_install_returns_existing_entry_without_duplication() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = CodexAdapter;
        let request = InstallHooksRequest {
            runtime_dir: root.join(".codex"),
            agentmesh_binary_path: absolute_agentmesh_binary_path(),
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
        let overlay = read(root.join(".codex/hooks.json"));
        let hook_count = overlay.matches("codex-hook").count();

        assert_eq!(first.hooks_installed[0].entry_path, "$.PostToolUse[0]");
        assert_eq!(second.hooks_installed[0].entry_path, "$.PostToolUse[0]");
        assert_eq!(hook_count, 1);
    }

    proptest! {
        #[test]
        fn import_emit_import_roundtrip_preserves_entity_shape(
            slug in "[a-z][a-z0-9]{0,8}(-[a-z0-9]{1,8}){0,2}",
            body in prop::collection::vec("[A-Za-z0-9 .,]{0,40}", 1..4).prop_map(|lines| lines.join("\n")),
            model in prop::sample::select(vec!["gpt-5", "gpt-5.4", "gpt-5.4-mini"]),
        ) {
            let temp = tempfile::tempdir()
                .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
            let root = temp.path();
            write(
                root.join("AGENTS.md"),
                &format!("Root instructions\n{body}\n"),
            );
            write(
                root.join(format!(".codex/skills/{slug}/SKILL.md")),
                &format!("---\nname: {slug}\ntags:\n  - generated\n---\n{body}\n"),
            );
            write(
                root.join(format!(".codex/agents/{slug}-agent.toml")),
                &format!(
                    "name = \"{slug}-agent\"\nmodel = \"{model}\"\ninstructions = \"{}\"\n",
                    body.replace('\n', "\\n")
                ),
            );

            let adapter = CodexAdapter;
            let imported = adapter
                .import(ImportRequest {
                    canonical_dir: root.join(".ai"),
                    runtime_dir: root.join(".codex"),
                    filter: None,
                })
                .unwrap_or_else(|error| panic!("import should succeed: {error}"));
            let emit_entities = emit_entities(imported.entities.clone());

            adapter
                .emit(EmitRequest {
                    runtime_dir: root.join(".codex-roundtrip"),
                    mode: RuntimeMode::Managed,
                    entities: emit_entities,
                })
                .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

            let roundtripped = adapter
                .import(ImportRequest {
                    canonical_dir: root.join(".ai"),
                    runtime_dir: root.join(".codex-roundtrip"),
                    filter: None,
                })
                .unwrap_or_else(|error| panic!("roundtrip import should succeed: {error}"));

            prop_assert_eq!(
                semantic_entities(imported.entities),
                semantic_entities(roundtripped.entities)
            );
        }
    }

    fn emit_entities(entities: Vec<ImportedEntity>) -> Vec<EmitEntity> {
        entities
            .into_iter()
            .map(|entity| EmitEntity {
                id: entity.id,
                entity_type: entity.entity_type,
                scope: entity.scope,
                files: entity.files,
                frontmatter: entity.frontmatter,
                overrides: BTreeMap::new(),
            })
            .collect()
    }

    fn semantic_entities(entities: Vec<ImportedEntity>) -> Vec<SemanticEntity> {
        let mut normalized = entities
            .into_iter()
            .map(|entity| {
                (
                    entity.id,
                    entity.entity_type,
                    entity.scope,
                    normalize_files(entity.files),
                    entity.frontmatter,
                )
            })
            .collect::<Vec<_>>();
        normalized.sort_by(|left, right| left.0.cmp(&right.0));
        normalized
    }

    fn normalize_files(files: BTreeMap<PathBuf, EntityFile>) -> BTreeMap<PathBuf, EntityFile> {
        files
            .into_iter()
            .map(|(path, file)| {
                if file.encoding == EntityFileEncoding::Utf8 {
                    let content = canonicalize_frontmatter(&file.content)
                        .unwrap_or_else(|_| file.content.clone());
                    (path, EntityFile::utf8(content))
                } else {
                    (path, file)
                }
            })
            .collect()
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
        if let Some(parent) = path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                panic!("parent directory should be created: {error}");
            }
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
