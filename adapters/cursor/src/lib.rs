//! Bundled Cursor adapter entry points.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentmesh_adapter_sdk_rust::{
    Adapter, AdapterError, AdapterMetadata, FormatTranslation, compose_frontmatter,
    dir_entry_file_type, is_regular_dir, is_regular_file, is_safe_relative, mtime_string,
    parse_frontmatter, read_dir_sorted, read_to_string, selected, sha256_bytes, skipped_entity,
    slug_for_entity, slugify, workspace_relative, workspace_root_for, write_atomic,
};
use agentmesh_protocol::{
    AdapterErrorCode, DetectResponse, EmitRequest, EmitResponse, EntityFile, EntityFileEncoding,
    EntityType, ImportFilter, ImportRequest, ImportResponse, ImportedEntity, InstallHooksRequest,
    InstallHooksResponse, RemoveHooksRequest, RemoveHooksResponse, RuntimeMode, SkippedPath,
};
use serde_json::Value as JsonValue;
use serde_norway::{Mapping as YamlMapping, Value as YamlValue};

const SUPPORTED_ENTITIES: &[EntityType] = &[EntityType::Instructions, EntityType::Rule];
const ALLOWED_READ_PATHS: &[&str] = &[
    ".cursor/rules/**",
    ".cursor/skills/**",
    ".cursor/hooks.json",
    ".cursor/commands/**",
    ".cursor/agents/**",
    ".cursor/mcp.json",
];
const ALLOWED_WRITE_PATHS: &[&str] = &[".cursor/rules/**"];
const MARKDOWN_FORMATS: &[&str] = &["markdown", "mdc"];
const FORMAT_TRANSLATIONS: &[FormatTranslation] = &[
    FormatTranslation {
        entity_type: EntityType::Instructions,
        formats: MARKDOWN_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::Rule,
        formats: MARKDOWN_FORMATS,
    },
];

/// Cursor adapter handle.
#[derive(Debug, Clone, Copy, Default)]
pub struct CursorAdapter;

impl Adapter for CursorAdapter {
    fn metadata(&self) -> AdapterMetadata {
        metadata()
    }

    fn detect(&self, workspace_root: &Path) -> agentmesh_adapter_sdk_rust::Result<DetectResponse> {
        let rules_dir = workspace_root.join(".cursor/rules");
        let files = if is_regular_dir(workspace_root, &rules_dir)? {
            vec![workspace_relative(workspace_root, &rules_dir)?]
        } else {
            Vec::new()
        };

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

        import_rules(
            &workspace_root,
            &request.runtime_dir.join("rules"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_deferred_diagnostics(&workspace_root, filter, &mut skipped)?;

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
        let rules_root = request.runtime_dir.join("rules");
        let mut files_written = Vec::new();
        let mut skipped = Vec::new();

        for entity in request.entities {
            match entity.entity_type {
                EntityType::Instructions => {
                    if is_root_instruction(&entity.id, entity.scope.as_deref()) {
                        skipped.push(skipped_entity(
                            entity.id,
                            "Cursor uses root AGENTS.md and does not emit duplicate root instructions",
                        ));
                        continue;
                    }
                    let Some(content) = first_file_content(&entity.files) else {
                        skipped.push(skipped_entity(
                            entity.id,
                            "instructions entity has no files",
                        ));
                        continue;
                    };
                    let frontmatter = cursor_frontmatter_for_emit(&entity, true);
                    let rendered = render_markdown_with_frontmatter(
                        &content,
                        &frontmatter,
                        &entity.overrides,
                    )?;
                    let target = cursor_source_path(&entity)
                        .map(|path| workspace_root.join(path))
                        .unwrap_or_else(|| {
                            request
                                .runtime_dir
                                .join("rules")
                                .join(format!("{}.mdc", scoped_instruction_slug(&entity.id)))
                        });
                    validate_cursor_write_path(&workspace_root, &rules_root, &target)?;
                    write_atomic(&target, rendered.as_bytes())?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                EntityType::Rule => {
                    let Some(content) = first_file_content(&entity.files) else {
                        skipped.push(skipped_entity(entity.id, "rule entity has no files"));
                        continue;
                    };
                    let slug = slug_for_entity(&entity.id, &entity.frontmatter);
                    let frontmatter = cursor_frontmatter_for_emit(&entity, false);
                    let rendered = render_markdown_with_frontmatter(
                        &content,
                        &frontmatter,
                        &entity.overrides,
                    )?;
                    let target = cursor_source_path(&entity)
                        .map(|path| workspace_root.join(path))
                        .unwrap_or_else(|| {
                            request
                                .runtime_dir
                                .join("rules")
                                .join(format!("{slug}.mdc"))
                        });
                    validate_cursor_write_path(&workspace_root, &rules_root, &target)?;
                    write_atomic(&target, rendered.as_bytes())?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                unsupported => skipped.push(skipped_entity(
                    entity.id,
                    format!("{} entity is not supported", unsupported.as_str()),
                )),
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
        _request: InstallHooksRequest,
    ) -> agentmesh_adapter_sdk_rust::Result<InstallHooksResponse> {
        Ok(InstallHooksResponse {
            hooks_installed: Vec::new(),
            fallback_needed: false,
            fallback_reason: None,
        })
    }

    fn remove_hooks(
        &self,
        _request: RemoveHooksRequest,
    ) -> agentmesh_adapter_sdk_rust::Result<RemoveHooksResponse> {
        Ok(RemoveHooksResponse {
            ok: true,
            removed_count: 0,
            error: None,
        })
    }
}

/// Returns static metadata for the Cursor adapter.
#[must_use]
pub const fn metadata() -> AdapterMetadata {
    AdapterMetadata {
        name: "cursor",
        runtime_dir: ".cursor",
        supported_entities: SUPPORTED_ENTITIES,
        allowed_read_paths: ALLOWED_READ_PATHS,
        allowed_write_paths: ALLOWED_WRITE_PATHS,
        format_translations: FORMAT_TRANSLATIONS,
    }
}

fn import_rules(
    workspace_root: &Path,
    rules_root: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    match is_regular_dir(workspace_root, rules_root) {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(error) => {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, rules_root),
                reason: error.to_string(),
            });
            return Ok(());
        }
    }

    import_rules_inner(
        workspace_root,
        rules_root,
        rules_root,
        filter,
        entities,
        skipped,
    )
}

fn import_rules_inner(
    workspace_root: &Path,
    rules_root: &Path,
    dir: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    for entry in read_dir_sorted(dir)? {
        let file_type = match dir_entry_file_type(&entry) {
            Ok(file_type) => file_type,
            Err(error) => {
                skipped.push(SkippedPath {
                    path: relative_or_path(workspace_root, &entry.path()),
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let path = entry.path();
        if file_type.is_symlink() {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, &path),
                reason: "symlinked Cursor rule path is not supported".to_string(),
            });
            continue;
        }
        if file_type.is_dir() {
            import_rules_inner(workspace_root, rules_root, &path, filter, entities, skipped)?;
            continue;
        }
        if !file_type.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("mdc")
        {
            continue;
        }
        let source_relative = workspace_relative(workspace_root, &path)?;
        if !selected(filter, std::slice::from_ref(&source_relative)) {
            continue;
        }
        let slug = path_slug(rules_root, &path);
        let content = match read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                skipped.push(SkippedPath {
                    path: source_relative,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let frontmatter = match frontmatter_json_for_path(&source_relative, &content) {
            Ok(frontmatter) => frontmatter,
            Err(error) => {
                skipped.push(SkippedPath {
                    path: source_relative,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        let (entity_type, id, scope, canonical_path) = if frontmatter.contains_key("globs") {
            let scope = match scope_from_globs(&frontmatter) {
                Ok(scope) => scope,
                Err(reason) => {
                    skipped.push(SkippedPath {
                        path: source_relative,
                        reason,
                    });
                    continue;
                }
            };
            (
                EntityType::Instructions,
                format!("instructions:scoped:{slug}"),
                scope,
                PathBuf::from("instructions").join(format!("{slug}.md")),
            )
        } else {
            (
                EntityType::Rule,
                format!("rule:{slug}"),
                None,
                PathBuf::from("rules").join(format!("{slug}.mdc")),
            )
        };

        let canonical_content =
            render_canonical_cursor_content(&content, &portable_cursor_frontmatter(&frontmatter))?;

        entities.push(import_markdown_entity(MarkdownImport {
            path: &path,
            entity_type,
            id,
            scope,
            canonical_path,
            source_path: source_relative,
            content: canonical_content,
            frontmatter,
        })?);
    }
    Ok(())
}

fn import_deferred_diagnostics(
    workspace_root: &Path,
    filter: Option<&ImportFilter>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    import_diagnostic_file_tree(
        workspace_root,
        &workspace_root.join(".cursor/skills"),
        filter,
        "Cursor skills are deferred and must never be emitted",
        skipped,
    )?;
    import_diagnostic_file(
        workspace_root,
        &workspace_root.join(".cursor/hooks.json"),
        filter,
        "Cursor hooks are deferred and must never be emitted",
        skipped,
    )?;
    import_diagnostic_file_tree(
        workspace_root,
        &workspace_root.join(".cursor/commands"),
        filter,
        "Cursor commands are deferred and must never be emitted",
        skipped,
    )?;
    import_diagnostic_file_tree(
        workspace_root,
        &workspace_root.join(".cursor/agents"),
        filter,
        "Cursor subagents are deferred and must never be emitted",
        skipped,
    )?;
    import_diagnostic_file(
        workspace_root,
        &workspace_root.join(".cursor/mcp.json"),
        filter,
        "Cursor MCP configuration is deferred and must never be emitted",
        skipped,
    )
}

fn import_diagnostic_file(
    workspace_root: &Path,
    path: &Path,
    filter: Option<&ImportFilter>,
    reason: &str,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let relative = relative_or_path(workspace_root, path);
    if !selected(filter, std::slice::from_ref(&relative)) || !is_regular_file(workspace_root, path)?
    {
        return Ok(());
    }
    skipped.push(SkippedPath {
        path: relative,
        reason: reason.to_string(),
    });
    Ok(())
}

fn import_diagnostic_file_tree(
    workspace_root: &Path,
    root: &Path,
    filter: Option<&ImportFilter>,
    reason: &str,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    match is_regular_dir(workspace_root, root) {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(error) => {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, root),
                reason: error.to_string(),
            });
            return Ok(());
        }
    }
    import_diagnostic_file_tree_inner(workspace_root, root, filter, reason, skipped)
}

fn import_diagnostic_file_tree_inner(
    workspace_root: &Path,
    dir: &Path,
    filter: Option<&ImportFilter>,
    reason: &str,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    for entry in read_dir_sorted(dir)? {
        let path = entry.path();
        let file_type = dir_entry_file_type(&entry)?;
        if file_type.is_symlink() {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, &path),
                reason: "symlinked diagnostic path is not supported".to_string(),
            });
            continue;
        }
        if file_type.is_dir() {
            import_diagnostic_file_tree_inner(workspace_root, &path, filter, reason, skipped)?;
            continue;
        }
        if file_type.is_file() {
            let relative = workspace_relative(workspace_root, &path)?;
            if selected(filter, std::slice::from_ref(&relative)) {
                skipped.push(SkippedPath {
                    path: relative,
                    reason: reason.to_string(),
                });
            }
        }
    }
    Ok(())
}

struct MarkdownImport<'a> {
    path: &'a Path,
    entity_type: EntityType,
    id: String,
    scope: Option<String>,
    canonical_path: PathBuf,
    source_path: PathBuf,
    content: String,
    frontmatter: BTreeMap<String, JsonValue>,
}

fn import_markdown_entity(
    input: MarkdownImport<'_>,
) -> agentmesh_adapter_sdk_rust::Result<ImportedEntity> {
    let files = BTreeMap::from([(
        input
            .canonical_path
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| input.canonical_path.clone()),
        EntityFile::utf8(input.content.clone()),
    )]);
    Ok(ImportedEntity {
        id: input.id,
        entity_type: input.entity_type,
        scope: input.scope,
        canonical_path: input.canonical_path,
        files,
        frontmatter: input.frontmatter,
        canonical_sha256: sha256_bytes(input.content.as_bytes()),
        source_path: input.source_path,
        source_mtime: mtime_string(input.path)?,
    })
}

fn cursor_frontmatter_for_emit(
    entity: &agentmesh_protocol::EmitEntity,
    scoped_instruction: bool,
) -> BTreeMap<String, JsonValue> {
    let cursor_origin = entity
        .source_path
        .as_ref()
        .is_some_and(|path| path.starts_with(".cursor/rules"));
    let mut frontmatter = if cursor_origin {
        entity.frontmatter.clone()
    } else {
        entity
            .frontmatter
            .iter()
            .filter(|(key, _)| matches!(key.as_str(), "description" | "globs" | "alwaysApply"))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    };
    if scoped_instruction && !frontmatter.contains_key("globs") {
        if let Some(scope) = entity.scope.as_deref().filter(|scope| *scope != "root") {
            frontmatter.insert(
                "globs".to_string(),
                JsonValue::Array(vec![JsonValue::String(scope.to_string())]),
            );
        }
    }
    frontmatter
}

fn portable_cursor_frontmatter(
    frontmatter: &BTreeMap<String, JsonValue>,
) -> BTreeMap<String, JsonValue> {
    frontmatter
        .iter()
        .filter(|(key, _)| key.as_str() == "description")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn render_canonical_cursor_content(
    content: &str,
    frontmatter: &BTreeMap<String, JsonValue>,
) -> agentmesh_adapter_sdk_rust::Result<String> {
    let mut document = parse_frontmatter(content)?;
    document.frontmatter.clear();
    if frontmatter.is_empty() {
        return Ok(document.body);
    }
    for (key, value) in frontmatter {
        document
            .frontmatter
            .insert(YamlValue::String(key.clone()), json_to_yaml(value)?);
    }
    compose_frontmatter(&document)
}

fn scope_from_globs(frontmatter: &BTreeMap<String, JsonValue>) -> Result<Option<String>, String> {
    match frontmatter.get("globs") {
        Some(JsonValue::String(value)) => {
            if value.trim().is_empty() {
                return Err("Cursor rule globs must contain one non-empty string scope".to_string());
            }
            Ok(Some(value.clone()))
        }
        Some(JsonValue::Array(values)) => {
            let scopes = values
                .iter()
                .filter_map(JsonValue::as_str)
                .filter(|value| !value.trim().is_empty())
                .collect::<Vec<_>>();
            if scopes.is_empty() {
                return Err("Cursor rule globs must contain one non-empty string scope".to_string());
            }
            if scopes.len() > 1 {
                return Err(
                    "Cursor rule globs with multiple scopes cannot be represented losslessly"
                        .to_string(),
                );
            }
            Ok(scopes.first().map(|scope| (*scope).to_string()))
        }
        Some(_) => Err("Cursor rule globs must be a string or list of strings".to_string()),
        _ => Ok(None),
    }
}

fn cursor_source_path(entity: &agentmesh_protocol::EmitEntity) -> Option<PathBuf> {
    let path = entity.source_path.as_ref()?;
    if !is_safe_relative(path) || !path.starts_with(".cursor/rules") {
        return None;
    }
    if path.extension().and_then(|value| value.to_str()) != Some("mdc") {
        return None;
    }
    Some(path.clone())
}

fn validate_cursor_write_path(
    workspace_root: &Path,
    rules_root: &Path,
    target: &Path,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let relative = workspace_relative(workspace_root, target)?;
    if !is_safe_relative(&relative) {
        return Err(AdapterError::rpc(
            AdapterErrorCode::WorkspaceOutsideBound,
            format!("unsafe Cursor rule path {}", target.display()),
        ));
    }
    if !target.starts_with(rules_root) {
        return Err(AdapterError::rpc(
            AdapterErrorCode::WorkspaceOutsideBound,
            format!(
                "{} is outside declared Cursor rules root {}",
                target.display(),
                rules_root.display()
            ),
        ));
    }
    if target.extension().and_then(|value| value.to_str()) != Some("mdc") {
        return Err(AdapterError::rpc(
            AdapterErrorCode::WorkspaceOutsideBound,
            format!("Cursor rule target must use .mdc: {}", target.display()),
        ));
    }

    let parent = target.parent().ok_or_else(|| {
        AdapterError::rpc(
            AdapterErrorCode::WorkspaceOutsideBound,
            format!("Cursor rule target has no parent: {}", target.display()),
        )
    })?;
    validate_existing_components(workspace_root, parent, true)?;
    validate_existing_components(workspace_root, target, false)
}

fn validate_existing_components(
    workspace_root: &Path,
    path: &Path,
    require_directory: bool,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let relative = workspace_relative(workspace_root, path)?;
    let mut current = workspace_root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return Err(AdapterError::rpc(
                AdapterErrorCode::WorkspaceOutsideBound,
                format!("unsafe Cursor path component in {}", path.display()),
            ));
        };
        current.push(part);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(AdapterError::Io {
                    action: "read metadata",
                    path: current,
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(AdapterError::rpc(
                AdapterErrorCode::WorkspaceOutsideBound,
                format!(
                    "symlinked Cursor path {} is not supported",
                    current.display()
                ),
            ));
        }
        if require_directory && !metadata.is_dir() {
            return Err(AdapterError::rpc(
                AdapterErrorCode::WorkspaceOutsideBound,
                format!(
                    "Cursor path component is not a directory: {}",
                    current.display()
                ),
            ));
        }
    }
    Ok(())
}

fn is_root_instruction(id: &str, scope: Option<&str>) -> bool {
    id == "instructions:root" || scope == Some("root")
}

fn scoped_instruction_slug(id: &str) -> String {
    id.strip_prefix("instructions:scoped:")
        .unwrap_or("scoped")
        .to_string()
}

fn path_slug(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut parts = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str().map(ToString::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
    if let Some(last) = parts.last_mut()
        && let Some(stem) = Path::new(last)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToString::to_string)
    {
        *last = stem;
    }
    parts
        .into_iter()
        .map(|part| slugify(&part))
        .collect::<Vec<_>>()
        .join("-")
}

fn render_markdown_with_frontmatter(
    content: &str,
    frontmatter: &BTreeMap<String, JsonValue>,
    overrides: &BTreeMap<String, JsonValue>,
) -> agentmesh_adapter_sdk_rust::Result<String> {
    if frontmatter.is_empty() && overrides.is_empty() && !content.starts_with("---\n") {
        return Ok(content.to_string());
    }
    let mut document = parse_frontmatter(content)?;
    document.frontmatter.clear();
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

fn frontmatter_json_for_path(
    source_path: &Path,
    content: &str,
) -> agentmesh_adapter_sdk_rust::Result<BTreeMap<String, JsonValue>> {
    frontmatter_json(content).map_err(|error| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!(
                "failed to parse frontmatter in {}: {error}",
                source_path.display()
            ),
        )
    })
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
    serde_norway::to_value(value).map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("failed to convert JSON value to YAML: {source}"),
        )
    })
}

fn first_file_content(files: &BTreeMap<PathBuf, EntityFile>) -> Option<String> {
    files.values().find_map(file_text)
}

fn file_text(file: &EntityFile) -> Option<String> {
    match file.encoding {
        EntityFileEncoding::Utf8 => Some(file.content.clone()),
        EntityFileEncoding::Base64 => None,
    }
}

fn relative_or_path(workspace_root: &Path, path: &Path) -> PathBuf {
    workspace_relative(workspace_root, path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use agentmesh_adapter_sdk_rust::Adapter;
    use agentmesh_protocol::{EmitEntity, EmitRequest, EntityFile, ImportRequest, RuntimeMode};
    use serde_json::json;

    use super::CursorAdapter;

    fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("parent dirs should be created: {error}"));
        }
        std::fs::write(path, contents)
            .unwrap_or_else(|error| panic!("fixture should be written: {error}"));
    }

    fn read(path: impl AsRef<Path>) -> String {
        std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("fixture should be readable: {error}"))
    }

    fn file(contents: &str) -> EntityFile {
        EntityFile::utf8(contents.to_string())
    }

    #[test]
    fn imports_cursor_rules_and_deferred_diagnostics() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(
            root.join(".cursor/rules/security.mdc"),
            "---\ndescription: Security\nalwaysApply: true\n---\nSecurity body.\n",
        );
        write(
            root.join(".cursor/rules/api.mdc"),
            "---\ndescription: API\nglobs:\n  - crates/**/src/**/*.rs\n---\nAPI body.\n",
        );
        write(
            root.join(".cursor/rules/team/security.mdc"),
            "---\ndescription: Team security\nalwaysApply: true\n---\nSecurity body.\n",
        );
        write(root.join(".cursor/commands/review.md"), "# Review\n");

        let imported = CursorAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".cursor"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));
        let ids = imported
            .entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"rule:security"));
        assert!(ids.contains(&"instructions:scoped:api"));
        assert!(ids.contains(&"rule:team-security"));
        let scoped = imported
            .entities
            .iter()
            .find(|entity| entity.id == "instructions:scoped:api")
            .unwrap_or_else(|| panic!("scoped rule should import"));
        assert_eq!(scoped.scope.as_deref(), Some("crates/**/src/**/*.rs"));
        assert!(!scoped.files[Path::new("api.md")].content.contains("globs:"));
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".cursor/commands/review.md")
                && skipped.reason.contains("Cursor commands are deferred")
        }));
    }

    #[test]
    fn detects_only_cursor_rules_presence() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(root.join(".cursor/mcp.json"), "{}\n");

        let deferred_only = CursorAdapter
            .detect(root)
            .unwrap_or_else(|error| panic!("detect should succeed: {error}"));
        assert!(!deferred_only.present);

        write(root.join(".cursor/rules/security.mdc"), "# Security\n");
        let with_rules = CursorAdapter
            .detect(root)
            .unwrap_or_else(|error| panic!("detect should succeed: {error}"));
        assert!(with_rules.present);
    }

    #[test]
    fn emits_cursor_rules_and_preserves_cursor_origin_metadata() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();

        let response = CursorAdapter
            .emit(EmitRequest {
                runtime_dir: root.join(".cursor"),
                mode: RuntimeMode::Managed,
                entities: vec![
                    EmitEntity {
                        id: "rule:preserve-metadata".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Rule,
                        scope: None,
                        source_path: Some(PathBuf::from(".cursor/rules/preserve-metadata.mdc")),
                        files: BTreeMap::from([(
                            PathBuf::from("preserve-metadata.mdc"),
                            file("# Preserve\n"),
                        )]),
                        frontmatter: BTreeMap::from([
                            ("description".to_string(), json!("Preserve")),
                            ("cursorPriority".to_string(), json!("high")),
                        ]),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "instructions:scoped:api".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Instructions,
                        scope: Some("crates/**/src/**/*.rs".to_string()),
                        source_path: None,
                        files: BTreeMap::from([(PathBuf::from("api.md"), file("# API\n"))]),
                        frontmatter: BTreeMap::from([("description".to_string(), json!("API"))]),
                        overrides: BTreeMap::new(),
                    },
                ],
            })
            .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

        assert!(response.skipped.is_empty());
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".cursor/rules/preserve-metadata.mdc"))
        );
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".cursor/rules/api.mdc"))
        );
        let preserved = read(root.join(".cursor/rules/preserve-metadata.mdc"));
        assert!(preserved.contains("cursorPriority: high"));
        let scoped = read(root.join(".cursor/rules/api.mdc"));
        assert!(scoped.contains("globs:"));
    }

    #[test]
    fn skips_invalid_cursor_frontmatter() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(
            root.join(".cursor/rules/broken.mdc"),
            "---\ndescription: \"unterminated\n---\nBroken\n",
        );

        let imported = CursorAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".cursor"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));

        assert!(imported.entities.is_empty());
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".cursor/rules/broken.mdc")
                && skipped.reason.contains("failed to parse frontmatter")
        }));
    }

    #[test]
    fn skips_lossy_multi_glob_cursor_rule() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(
            root.join(".cursor/rules/multi.mdc"),
            "---\nglobs:\n  - crates/api/**\n  - crates/web/**\n---\nMulti\n",
        );
        write(
            root.join(".cursor/rules/empty.mdc"),
            "---\nglobs: []\n---\nEmpty\n",
        );
        write(
            root.join(".cursor/rules/object.mdc"),
            "---\nglobs:\n  include: crates/**\n---\nObject\n",
        );

        let imported = CursorAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".cursor"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));

        assert!(imported.entities.is_empty());
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".cursor/rules/multi.mdc")
                && skipped.reason.contains("multiple scopes")
        }));
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".cursor/rules/empty.mdc")
                && skipped.reason.contains("one non-empty string scope")
        }));
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".cursor/rules/object.mdc")
                && skipped.reason.contains("string or list of strings")
        }));
    }

    #[test]
    fn keeps_cursor_only_frontmatter_out_of_canonical_content() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(
            root.join(".cursor/rules/preserve.mdc"),
            "---\ndescription: Preserve\nalwaysApply: true\ncursorPriority: high\n---\nBody\n",
        );

        let imported = CursorAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".cursor"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));
        let entity = imported
            .entities
            .iter()
            .find(|entity| entity.id == "rule:preserve")
            .unwrap_or_else(|| panic!("rule should import"));
        let content = &entity.files[Path::new("preserve.mdc")].content;

        assert!(entity.frontmatter.contains_key("cursorPriority"));
        assert!(!content.contains("cursorPriority"));
        assert!(!content.contains("alwaysApply"));
        assert!(content.contains("description: Preserve"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_cursor_rule_write_root() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside)
            .unwrap_or_else(|error| panic!("outside dir should be created: {error}"));
        std::fs::create_dir_all(root.join(".cursor"))
            .unwrap_or_else(|error| panic!("cursor dir should be created: {error}"));
        symlink(&outside, root.join(".cursor/rules"))
            .unwrap_or_else(|error| panic!("symlink should be created: {error}"));

        let error = CursorAdapter
            .emit(EmitRequest {
                runtime_dir: root.join(".cursor"),
                mode: RuntimeMode::Managed,
                entities: vec![EmitEntity {
                    id: "rule:escape".to_string(),
                    entity_type: agentmesh_protocol::EntityType::Rule,
                    scope: None,
                    source_path: None,
                    files: BTreeMap::from([(PathBuf::from("escape.md"), file("# Escape\n"))]),
                    frontmatter: BTreeMap::new(),
                    overrides: BTreeMap::new(),
                }],
            })
            .expect_err("symlinked Cursor rules root should be rejected");

        assert!(error.to_string().contains("symlinked Cursor path"));
        assert!(!outside.join("escape.mdc").exists());
    }
}
