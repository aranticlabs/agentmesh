//! Bundled Codex adapter entry points.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

mod hooks;

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
    EntityType::Hook,
    EntityType::McpBinding,
    EntityType::PermissionPolicy,
    EntityType::Skill,
    EntityType::Subagent,
];

const ALLOWED_READ_PATHS: &[&str] = &[
    ".codex/**",
    ".agents/skills/**",
    "AGENTS.md",
    "**/AGENTS.md",
];
const ALLOWED_WRITE_PATHS: &[&str] = &[
    ".codex/**",
    ".agents/skills/**",
    "AGENTS.md",
    "**/AGENTS.md",
];
const SUBAGENT_FORMATS: &[&str] = &["markdown", "toml"];
const JSON_FORMATS: &[&str] = &["json"];
const TOML_FORMATS: &[&str] = &["toml"];
const CODEX_PERMISSION_POLICY_KEYS: &[&str] = &[
    "profiles",
    "approval_policy",
    "sandbox_mode",
    "sandbox_workspace_write",
];
const FORMAT_TRANSLATIONS: &[FormatTranslation] = &[
    FormatTranslation {
        entity_type: EntityType::Hook,
        formats: JSON_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::McpBinding,
        formats: TOML_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::PermissionPolicy,
        formats: TOML_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::Subagent,
        formats: SUBAGENT_FORMATS,
    },
];

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
            workspace_root.join(".codex/hooks.json"),
            workspace_root.join(".codex/config.toml"),
            workspace_root.join(".agents/skills"),
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

        import_nested_instructions(&workspace_root, filter, &mut entities, &mut skipped)?;
        import_skills(
            &workspace_root,
            &request.runtime_dir.join("skills"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_skills(
            &workspace_root,
            &workspace_root.join(".agents/skills"),
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
        import_hooks_json(
            &workspace_root,
            &request.runtime_dir.join("hooks.json"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_config_toml(
            &workspace_root,
            &request.runtime_dir.join("config.toml"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_read_only_and_deferred_diagnostics(&workspace_root, filter, &mut skipped)?;

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
                    let path = if is_root_instruction(&entity.id, entity.scope.as_deref()) {
                        workspace_root.join("AGENTS.md")
                    } else if let Some(source_path) = native_agents_path(&entity) {
                        workspace_root.join(source_path)
                    } else {
                        let Some(path) = scoped_agents_path(&entity.id, entity.scope.as_deref())
                        else {
                            skipped.push(skipped_entity(
                                entity.id,
                                "scoped instructions cannot be represented as a nested AGENTS.md",
                            ));
                            continue;
                        };
                        workspace_root.join(path)
                    };
                    write_atomic(&path, rendered.as_bytes())?;
                    files_written.push(workspace_relative(&workspace_root, &path)?);
                }
                EntityType::Hook => {
                    let target = request.runtime_dir.join("hooks.json");
                    merge_json_section(&target, "hooks", &entity)?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                EntityType::McpBinding => {
                    let target = request.runtime_dir.join("config.toml");
                    merge_toml_sections(&target, &entity, &["mcp_servers"])?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                EntityType::PermissionPolicy => {
                    let target = request.runtime_dir.join("config.toml");
                    merge_toml_sections(&target, &entity, CODEX_PERMISSION_POLICY_KEYS)?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                EntityType::Skill => {
                    let slug = slug_for_entity(&entity.id, &entity.frontmatter);
                    let target_root =
                        skill_target_root(&workspace_root, &request.runtime_dir, &slug, &entity);
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
                unsupported => {
                    skipped.push(skipped_entity(
                        entity.id,
                        format!("{} entity is not supported", unsupported.as_str()),
                    ));
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
        hooks::install_hooks(request)
    }

    fn remove_hooks(
        &self,
        request: RemoveHooksRequest,
    ) -> agentmesh_adapter_sdk_rust::Result<RemoveHooksResponse> {
        hooks::remove_hooks(request)
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

fn import_nested_instructions(
    workspace_root: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    import_nested_instructions_in_dir(workspace_root, workspace_root, filter, entities, skipped)
}

fn import_nested_instructions_in_dir(
    workspace_root: &Path,
    dir: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    for entry in read_dir_sorted(dir)? {
        let path = entry.path();
        let file_type = dir_entry_file_type(&entry)?;
        if file_type.is_symlink() {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, &path),
                reason: "symlinked instruction path is not supported".to_string(),
            });
            continue;
        }
        if file_type.is_dir() {
            if should_skip_nested_instruction_dir(workspace_root, &path) {
                continue;
            }
            import_nested_instructions_in_dir(workspace_root, &path, filter, entities, skipped)?;
            continue;
        }
        if !file_type.is_file()
            || path.file_name().and_then(|name| name.to_str()) != Some("AGENTS.md")
        {
            continue;
        }
        let source_relative = workspace_relative(workspace_root, &path)?;
        if source_relative == Path::new("AGENTS.md")
            || !selected(filter, std::slice::from_ref(&source_relative))
        {
            continue;
        }
        let Some(scope_dir) = source_relative.parent() else {
            continue;
        };
        let slug = slugify(&scope_dir.to_string_lossy());
        let scope = format!("{}/**", scope_dir.to_string_lossy().replace('\\', "/"));
        entities.push(import_markdown_entity(
            &path,
            EntityType::Instructions,
            format!("instructions:scoped:{slug}"),
            Some(scope),
            PathBuf::from("instructions").join(format!("{slug}.md")),
            source_relative,
        )?);
    }

    Ok(())
}

fn should_skip_nested_instruction_dir(workspace_root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(workspace_root).unwrap_or(path);
    let Some(first) = relative.iter().next().and_then(|part| part.to_str()) else {
        return true;
    };
    matches!(
        first,
        ".git"
            | ".ai"
            | ".agents"
            | ".claude"
            | ".codex"
            | ".cursor"
            | ".gemini"
            | ".github"
            | "target"
    )
}

fn import_hooks_json(
    workspace_root: &Path,
    hooks_path: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let source_relative = PathBuf::from(".codex/hooks.json");
    if !selected(filter, std::slice::from_ref(&source_relative))
        || !is_regular_file(workspace_root, hooks_path)?
    {
        return Ok(());
    }
    let value = match read_json_object(hooks_path) {
        Ok(value) => value,
        Err(error) => {
            skipped.push(SkippedPath {
                path: source_relative,
                reason: error.to_string(),
            });
            return Ok(());
        }
    };
    entities.push(import_json_entity(
        hooks_path,
        source_relative,
        EntityType::Hook,
        "hook:codex-project",
        PathBuf::from("hooks/codex-project.json"),
        value,
    )?);
    Ok(())
}

fn import_config_toml(
    workspace_root: &Path,
    config_path: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let source_relative = PathBuf::from(".codex/config.toml");
    if !selected(filter, std::slice::from_ref(&source_relative))
        || !is_regular_file(workspace_root, config_path)?
    {
        return Ok(());
    }
    let table = match read_toml_table(config_path) {
        Ok(table) => table,
        Err(error) => {
            skipped.push(SkippedPath {
                path: source_relative,
                reason: error.to_string(),
            });
            return Ok(());
        }
    };
    if table.contains_key("hooks") {
        skipped.push(SkippedPath {
            path: source_relative.clone(),
            reason: "Codex inline config hooks are read-only diagnostics".to_string(),
        });
    }
    if table.contains_key("mcp_servers") {
        entities.push(import_toml_section_entity(
            config_path,
            source_relative.clone(),
            EntityType::McpBinding,
            "mcp-binding:codex-project",
            PathBuf::from("mcp-bindings/codex-project.toml"),
            &table,
            &["mcp_servers"],
        )?);
    }
    if CODEX_PERMISSION_POLICY_KEYS
        .iter()
        .any(|key| table.contains_key(*key))
    {
        entities.push(import_toml_section_entity(
            config_path,
            source_relative,
            EntityType::PermissionPolicy,
            "permission-policy:codex-project",
            PathBuf::from("permission-policies/codex-project.toml"),
            &table,
            CODEX_PERMISSION_POLICY_KEYS,
        )?);
    }
    Ok(())
}

fn import_read_only_and_deferred_diagnostics(
    workspace_root: &Path,
    filter: Option<&ImportFilter>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    import_diagnostic_file_tree(
        workspace_root,
        &workspace_root.join(".codex/rules"),
        filter,
        "Codex experimental rules are read-only diagnostics",
        skipped,
    )?;
    import_diagnostic_file_tree(
        workspace_root,
        &workspace_root.join(".codex/prompts"),
        filter,
        "Codex custom prompts are deferred for project sync",
        skipped,
    )?;
    import_diagnostic_file_tree(
        workspace_root,
        &workspace_root.join(".codex/commands"),
        filter,
        "Codex project commands are deferred for project sync",
        skipped,
    )
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
        if !file_type.is_file() {
            continue;
        }
        let relative = workspace_relative(workspace_root, &path)?;
        if selected(filter, std::slice::from_ref(&relative)) {
            skipped.push(SkippedPath {
                path: relative,
                reason: reason.to_string(),
            });
        }
    }
    Ok(())
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
        let frontmatter = match frontmatter_json(&content) {
            Ok(frontmatter) => frontmatter,
            Err(error) => {
                skipped.push(SkippedPath {
                    path: source_relative,
                    reason: error.to_string(),
                });
                continue;
            }
        };

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
        let entity = match import_toml_subagent(
            &path,
            format!("subagent:{slug}"),
            PathBuf::from("agents").join(format!("{slug}.md")),
            source_relative.clone(),
        ) {
            Ok(entity) => entity,
            Err(error) => {
                skipped.push(SkippedPath {
                    path: source_relative,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        entities.push(entity);
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
    normalize_imported_codex_skills(&mut frontmatter);
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

fn import_json_entity(
    path: &Path,
    source_path: PathBuf,
    entity_type: EntityType,
    id: &str,
    canonical_path: PathBuf,
    value: JsonValue,
) -> agentmesh_adapter_sdk_rust::Result<ImportedEntity> {
    let content = render_json(&value)?;
    let file_key = canonical_path
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| canonical_path.clone());
    let files = BTreeMap::from([(file_key, EntityFile::utf8(content.clone()))]);

    Ok(ImportedEntity {
        id: id.to_string(),
        entity_type,
        scope: None,
        canonical_path,
        files,
        frontmatter: BTreeMap::new(),
        canonical_sha256: sha256_bytes(content.as_bytes()),
        source_path,
        source_mtime: mtime_string(path)?,
    })
}

fn import_toml_section_entity(
    path: &Path,
    source_path: PathBuf,
    entity_type: EntityType,
    id: &str,
    canonical_path: PathBuf,
    source_table: &toml::map::Map<String, toml::Value>,
    section_keys: &[&str],
) -> agentmesh_adapter_sdk_rust::Result<ImportedEntity> {
    let mut table = toml::map::Map::new();
    for key in section_keys {
        if let Some(value) = source_table.get(*key) {
            table.insert((*key).to_string(), value.clone());
        }
    }
    let content = serialize_toml_table(&table);
    let file_key = canonical_path
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| canonical_path.clone());
    let files = BTreeMap::from([(file_key, EntityFile::utf8(content.clone()))]);

    Ok(ImportedEntity {
        id: id.to_string(),
        entity_type,
        scope: None,
        canonical_path,
        files,
        frontmatter: BTreeMap::new(),
        canonical_sha256: sha256_bytes(content.as_bytes()),
        source_path,
        source_mtime: mtime_string(path)?,
    })
}

fn render_json(value: &JsonValue) -> agentmesh_adapter_sdk_rust::Result<String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("failed to serialize JSON entity: {source}"),
        )
    })?;
    bytes.push(b'\n');
    String::from_utf8(bytes).map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("failed to encode JSON entity: {source}"),
        )
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

fn is_root_instruction(id: &str, scope: Option<&str>) -> bool {
    id == "instructions:root" || scope == Some("root")
}

fn scoped_agents_path(_id: &str, scope: Option<&str>) -> Option<PathBuf> {
    if let Some(scope) = scope.and_then(scope_directory) {
        return Some(scope.join("AGENTS.md"));
    }
    None
}

fn scope_directory(scope: &str) -> Option<PathBuf> {
    let trimmed = scope.trim().trim_matches('/');
    let trimmed = trimmed
        .strip_suffix("/**")
        .or_else(|| trimmed.strip_suffix("/*"))
        .unwrap_or(trimmed);
    if trimmed.contains(['*', '?', '[', ']']) {
        return None;
    }
    if trimmed.is_empty() || trimmed == "root" {
        return None;
    }
    let path = PathBuf::from(trimmed);
    if is_safe_relative(&path) {
        Some(path)
    } else {
        None
    }
}

fn skill_target_root(
    workspace_root: &Path,
    runtime_dir: &Path,
    slug: &str,
    entity: &agentmesh_protocol::EmitEntity,
) -> PathBuf {
    if let Some(source_root) = shared_skill_source_root(entity) {
        return workspace_root.join(source_root);
    }
    runtime_dir.join("skills").join(slug)
}

fn shared_skill_source_root(entity: &agentmesh_protocol::EmitEntity) -> Option<&Path> {
    let path = entity.source_path.as_deref()?;
    if !is_safe_relative(path)
        || !path.starts_with(".agents/skills")
        || path.file_name().and_then(|value| value.to_str()) != Some("SKILL.md")
    {
        return None;
    }
    let parent = path.parent()?;
    if parent.parent() == Some(Path::new(".agents/skills")) {
        Some(parent)
    } else {
        None
    }
}

fn native_agents_path(entity: &agentmesh_protocol::EmitEntity) -> Option<PathBuf> {
    let path = entity.source_path.as_ref()?;
    if !is_safe_relative(path) {
        return None;
    }
    if path.components().next().is_some_and(|component| {
        matches!(component, std::path::Component::Normal(part) if part.to_string_lossy().starts_with('.'))
    }) {
        return None;
    }
    if path.file_name().and_then(|value| value.to_str()) != Some("AGENTS.md") {
        return None;
    }
    Some(path.clone())
}

fn json_object_from_entity(
    entity: &agentmesh_protocol::EmitEntity,
    label: &str,
) -> agentmesh_adapter_sdk_rust::Result<serde_json::Map<String, JsonValue>> {
    let Some(content) = first_file_content(&entity.files) else {
        return Err(AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("{label} entity has no files"),
        ));
    };
    let value = serde_json::from_str::<JsonValue>(&content).map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("failed to parse {label} JSON: {source}"),
        )
    })?;
    match value {
        JsonValue::Object(object) => Ok(object),
        _ => Err(AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("{label} JSON root must be an object"),
        )),
    }
}

fn merge_json_section(
    target: &Path,
    section_key: &str,
    entity: &agentmesh_protocol::EmitEntity,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let mut existing = read_json_object(target)?;
    let payload = json_object_from_entity(entity, section_key)?;
    let replacement = payload
        .get(section_key)
        .cloned()
        .unwrap_or(JsonValue::Object(payload));
    let Some(existing_object) = existing.as_object_mut() else {
        return Err(AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            "JSON root must be an object",
        ));
    };
    let merged = merge_json_section_value(existing_object.remove(section_key), replacement);
    existing_object.insert(section_key.to_string(), merged);
    write_json_pretty(target, &existing)
}

fn merge_json_section_value(existing: Option<JsonValue>, replacement: JsonValue) -> JsonValue {
    match (existing, replacement) {
        (Some(JsonValue::Object(existing)), JsonValue::Object(replacement)) => {
            JsonValue::Object(merge_json_objects(existing, replacement))
        }
        (_, replacement) => replacement,
    }
}

fn merge_json_objects(
    mut existing: serde_json::Map<String, JsonValue>,
    replacement: serde_json::Map<String, JsonValue>,
) -> serde_json::Map<String, JsonValue> {
    for (key, value) in replacement {
        let merged = match (existing.remove(&key), value) {
            (Some(JsonValue::Object(existing)), JsonValue::Object(replacement)) => {
                JsonValue::Object(merge_json_objects(existing, replacement))
            }
            (Some(JsonValue::Array(existing)), JsonValue::Array(replacement)) => {
                JsonValue::Array(merge_json_arrays(existing, replacement))
            }
            (_, replacement) => replacement,
        };
        existing.insert(key, merged);
    }
    existing
}

fn merge_json_arrays(mut existing: Vec<JsonValue>, replacement: Vec<JsonValue>) -> Vec<JsonValue> {
    for value in replacement {
        if !existing.contains(&value) {
            existing.push(value);
        }
    }
    existing
}

fn read_toml_table(
    path: &Path,
) -> agentmesh_adapter_sdk_rust::Result<toml::map::Map<String, toml::Value>> {
    let content = read_to_string(path)?;
    toml_table_from_str(&content, path)
}

fn toml_table_from_str(
    content: &str,
    path: &Path,
) -> agentmesh_adapter_sdk_rust::Result<toml::map::Map<String, toml::Value>> {
    let value = content.parse::<toml::Value>().map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("failed to parse TOML at {}: {source}", path.display()),
        )
    })?;
    value.as_table().cloned().ok_or_else(|| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("TOML root at {} must be a table", path.display()),
        )
    })
}

fn merge_toml_sections(
    target: &Path,
    entity: &agentmesh_protocol::EmitEntity,
    section_keys: &[&str],
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let mut existing = if target.exists() {
        read_toml_table(target)?
    } else {
        toml::map::Map::new()
    };
    let Some(content) = first_file_content(&entity.files) else {
        return Err(AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            "TOML entity has no files",
        ));
    };
    let payload = toml_table_from_str(&content, target)?;
    for key in section_keys {
        if let Some(value) = payload.get(*key) {
            existing.insert((*key).to_string(), value.clone());
        }
    }
    write_atomic(target, serialize_toml_table(&existing).as_bytes())
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
    normalize_codex_skills_for_emit(&mut merged);

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

fn normalize_imported_codex_skills(frontmatter: &mut BTreeMap<String, JsonValue>) {
    let Some(skills) = frontmatter.get("skills").cloned() else {
        return;
    };
    let Some(bundled) = extract_current_codex_bundled_skills(&skills) else {
        return;
    };
    frontmatter.insert("skills".to_string(), JsonValue::Array(bundled));
}

fn normalize_codex_skills_for_emit(frontmatter: &mut BTreeMap<String, JsonValue>) {
    let Some(skills) = frontmatter.get("skills").cloned() else {
        return;
    };
    let Some(bundled) = extract_canonical_skills(&skills) else {
        return;
    };
    frontmatter.insert(
        "skills".to_string(),
        JsonValue::Object(
            [("bundled".to_string(), JsonValue::Array(bundled))]
                .into_iter()
                .collect(),
        ),
    );
}

fn extract_current_codex_bundled_skills(value: &JsonValue) -> Option<Vec<JsonValue>> {
    let JsonValue::Object(object) = value else {
        return None;
    };
    object.get("bundled").and_then(extract_canonical_skills)
}

fn extract_canonical_skills(value: &JsonValue) -> Option<Vec<JsonValue>> {
    match value {
        JsonValue::Array(values) => {
            let skills = values
                .iter()
                .filter_map(JsonValue::as_str)
                .map(|skill| JsonValue::String(skill.to_string()))
                .collect::<Vec<_>>();
            if skills.is_empty() {
                None
            } else {
                Some(skills)
            }
        }
        _ => None,
    }
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
            "name = \"code-reviewer\"\nmodel = \"gpt-5\"\ninstructions = \"Review code.\"\n\n[skills]\nbundled = [\"security-review\"]\n",
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
        assert_eq!(
            subagent.frontmatter.get("skills"),
            Some(&json!(["security-review"]))
        );
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
    fn import_skips_malformed_codex_entities_without_aborting() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(root.join("AGENTS.md"), "# Instructions\n");
        write(
            root.join(".codex/skills/good/SKILL.md"),
            "---\nname: good\n---\nBody\n",
        );
        write(
            root.join(".codex/skills/bad/SKILL.md"),
            "---\ndescription: \"unterminated\n---\nBody\n",
        );
        write(root.join(".codex/agents/broken.toml"), "name = \"broken\n");

        let adapter = CodexAdapter;
        let imported = adapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".codex"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should skip malformed entities: {error}"));
        let ids = imported
            .entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"instructions:root"));
        assert!(ids.contains(&"skill:good"));
        assert!(!ids.contains(&"skill:bad"));
        assert!(!ids.contains(&"subagent:broken"));
        assert_eq!(imported.skipped.len(), 2);
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".codex/skills/bad/SKILL.md")
                && skipped.reason.contains("failed to parse frontmatter")
        }));
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".codex/agents/broken.toml")
                && skipped
                    .reason
                    .contains("failed to parse Codex subagent TOML")
        }));
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
                source_path: None,
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
    fn emits_codex_subagent_skills_as_structured_table() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = CodexAdapter;
        let files = BTreeMap::from([(
            PathBuf::from("code-reviewer.md"),
            file(
                "---\nname: code-reviewer\nmodel: gpt-5\nskills:\n  - add-endpoint\n  - explore-architecture\n---\nReview code.\n",
            ),
        )]);

        let response = match adapter.emit(EmitRequest {
            runtime_dir: root.join(".codex"),
            mode: RuntimeMode::Managed,
            entities: vec![EmitEntity {
                id: "subagent:code-reviewer".to_string(),
                entity_type: agentmesh_protocol::EntityType::Subagent,
                scope: None,
                source_path: None,
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
        assert!(content.contains("[skills]"));
        assert!(content.contains("bundled = [\"add-endpoint\", \"explore-architecture\"]"));
        assert!(!content.contains("skills = \""));
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
                source_path: None,
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
    fn emits_shared_codex_skill_to_agents_surface() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = CodexAdapter;
        let files = BTreeMap::from([(
            PathBuf::from("SKILL.md"),
            file("---\nname: Shared Workflow v2\n---\nBody\n"),
        )]);

        let response = adapter
            .emit(EmitRequest {
                runtime_dir: root.join(".codex"),
                mode: RuntimeMode::Managed,
                entities: vec![EmitEntity {
                    id: "skill:shared-workflow".to_string(),
                    entity_type: agentmesh_protocol::EntityType::Skill,
                    scope: None,
                    source_path: Some(PathBuf::from(".agents/skills/shared-workflow/SKILL.md")),
                    files,
                    frontmatter: BTreeMap::from([(
                        "name".to_string(),
                        json!("Shared Workflow v2"),
                    )]),
                    overrides: BTreeMap::new(),
                }],
            })
            .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

        assert_eq!(
            response.files_written,
            vec![PathBuf::from(".agents/skills/shared-workflow/SKILL.md")]
        );
        assert!(
            root.join(".agents/skills/shared-workflow/SKILL.md")
                .is_file()
        );
        assert!(!root.join(".codex/skills/shared-workflow/SKILL.md").exists());
        assert!(
            !root
                .join(".agents/skills/shared-workflow-v2/SKILL.md")
                .exists()
        );
    }

    #[test]
    fn skips_codex_scoped_instruction_with_unrepresentable_glob_scope() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = CodexAdapter;

        let response = adapter
            .emit(EmitRequest {
                runtime_dir: root.join(".codex"),
                mode: RuntimeMode::Managed,
                entities: vec![EmitEntity {
                    id: "instructions:scoped:api-rs".to_string(),
                    entity_type: agentmesh_protocol::EntityType::Instructions,
                    scope: Some("packages/api/**/*.rs".to_string()),
                    source_path: None,
                    files: BTreeMap::from([(
                        PathBuf::from("AGENTS.md"),
                        file("Use API conventions.\n"),
                    )]),
                    frontmatter: BTreeMap::new(),
                    overrides: BTreeMap::new(),
                }],
            })
            .unwrap_or_else(|error| panic!("emit should succeed with skip: {error}"));

        assert!(response.files_written.is_empty());
        assert_eq!(response.skipped.len(), 1);
        assert!(!root.join("packages/api/**/*.rs/AGENTS.md").exists());
    }

    #[test]
    fn skips_codex_scoped_instruction_without_scope_or_source_path() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        let adapter = CodexAdapter;

        let response = adapter
            .emit(EmitRequest {
                runtime_dir: root.join(".codex"),
                mode: RuntimeMode::Managed,
                entities: vec![EmitEntity {
                    id: "instructions:scoped:packages-api".to_string(),
                    entity_type: agentmesh_protocol::EntityType::Instructions,
                    scope: None,
                    source_path: None,
                    files: BTreeMap::from([(
                        PathBuf::from("AGENTS.md"),
                        file("Use API conventions.\n"),
                    )]),
                    frontmatter: BTreeMap::new(),
                    overrides: BTreeMap::new(),
                }],
            })
            .unwrap_or_else(|error| panic!("emit should succeed with skip: {error}"));

        assert!(response.files_written.is_empty());
        assert_eq!(response.skipped.len(), 1);
        assert!(!root.join("packages-api/AGENTS.md").exists());
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

        assert_eq!(
            installed.hooks_installed[0].entry_path,
            "$.hooks.PostToolUse[0]"
        );
        let overlay = read(root.join(".codex/hooks.json"));
        assert!(overlay.contains("codex-hook"));
        assert!(overlay.contains("AgentMesh sync"));
        assert!(overlay.contains("\"hooks\""));

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
            r#"{"hooks":{"PostToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","command":"echo user"}]}]}}"#,
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
        assert_eq!(
            installed.hooks_installed[0].entry_path,
            "$.hooks.PostToolUse[1]"
        );

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
                source_path: Some(entity.source_path),
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

    #[test]
    fn install_migrates_legacy_codex_hook_without_duplication() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(
            root.join(".codex/hooks.json"),
            r#"{"PostToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"/old/agentmesh sync --trigger=codex-hook --silent"}]},{"matcher":"Bash","hooks":[{"type":"command","command":"echo user"}]}]}"#,
        );
        let adapter = CodexAdapter;

        let installed = adapter
            .install_hooks(InstallHooksRequest {
                runtime_dir: root.join(".codex"),
                agentmesh_binary_path: absolute_agentmesh_binary_path(),
                matcher_extra: None,
            })
            .unwrap_or_else(|error| panic!("install should succeed: {error}"));

        assert_eq!(
            installed.hooks_installed[0].entry_path,
            "$.hooks.PostToolUse[0]"
        );
        let overlay = read(root.join(".codex/hooks.json"));
        assert_eq!(overlay.matches("codex-hook").count(), 1);
        assert!(!overlay.contains("/old/agentmesh"));
        assert!(overlay.contains("echo user"));
    }

    #[test]
    fn remove_codex_hook_handles_legacy_entries_without_deleting_user_hooks() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(
            root.join(".codex/hooks.json"),
            r#"{"PostToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"/old/agentmesh sync --trigger=codex-hook --silent"}]},{"matcher":"Bash","hooks":[{"type":"command","command":"echo user"}]}]}"#,
        );
        let adapter = CodexAdapter;

        let removed = adapter
            .remove_hooks(RemoveHooksRequest {
                runtime_dir: root.join(".codex"),
                entry_paths: vec!["$.PostToolUse[0]".to_string()],
            })
            .unwrap_or_else(|error| panic!("remove should succeed: {error}"));

        assert!(removed.ok);
        assert_eq!(removed.removed_count, 1);
        let overlay = read(root.join(".codex/hooks.json"));
        assert!(overlay.contains("echo user"));
        assert!(!overlay.contains("codex-hook"));
    }

    #[test]
    fn imports_codex_v02_project_surfaces_and_diagnostics() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(root.join("AGENTS.md"), "# Root\n");
        write(root.join("packages/api/AGENTS.md"), "# API\n");
        write(
            root.join(".agents/skills/shared-workflow/SKILL.md"),
            "---\nname: shared-workflow\n---\nShared workflow.\n",
        );
        write(
            root.join(".codex/hooks.json"),
            r#"{"hooks":{"PostToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"npm test"}]}]}}"#,
        );
        write(
            root.join(".codex/config.toml"),
            r#"model = "gpt-5"
approval_policy = "on-request"
sandbox_mode = "workspace-write"

[hooks.PostToolUse]
command = "npm test"
matcher = "Edit"

[mcp_servers.filesystem]
command = "node"
args = ["server.js"]

[profiles.locked-down]
approval_policy = "never"
sandbox_mode = "read-only"
"#,
        );
        write(
            root.join(".codex/rules/strict.rules"),
            "deny = [\"network\"]\n",
        );
        write(root.join(".codex/prompts/release.md"), "# Release\n");
        write(root.join(".codex/commands/review.md"), "# Review\n");

        let adapter = CodexAdapter;
        let imported = adapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".codex"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));
        let ids = imported
            .entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"instructions:root"));
        assert!(ids.contains(&"instructions:scoped:packages-api"));
        assert!(ids.contains(&"skill:shared-workflow"));
        assert!(ids.contains(&"hook:codex-project"));
        assert!(ids.contains(&"mcp-binding:codex-project"));
        assert!(ids.contains(&"permission-policy:codex-project"));
        let nested = imported
            .entities
            .iter()
            .find(|entity| entity.id == "instructions:scoped:packages-api")
            .unwrap_or_else(|| panic!("nested instructions should be imported"));
        assert_eq!(nested.scope.as_deref(), Some("packages/api/**"));
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".codex/config.toml")
                && skipped.reason.contains("inline config hooks")
        }));
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".codex/rules/strict.rules")
                && skipped.reason.contains("experimental rules")
        }));
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".codex/prompts/release.md")
                && skipped.reason.contains("custom prompts")
        }));
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".codex/commands/review.md")
                && skipped.reason.contains("project commands")
        }));
    }

    #[test]
    fn emits_codex_v02_surfaces_without_clobbering_shared_config() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(
            root.join(".codex/config.toml"),
            "model = \"gpt-5\"\n\n[hooks.PostToolUse]\ncommand = \"npm test\"\n",
        );
        let adapter = CodexAdapter;

        let response = adapter
            .emit(EmitRequest {
                runtime_dir: root.join(".codex"),
                mode: RuntimeMode::Managed,
                entities: vec![
                    EmitEntity {
                        id: "instructions:scoped:packages-api".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Instructions,
                        scope: Some("packages/api/**".to_string()),
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("AGENTS.md"),
                            file("Use API conventions.\n"),
                        )]),
                        frontmatter: BTreeMap::new(),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "hook:codex-project".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Hook,
                        scope: None,
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("codex-project.json"),
                            file(r#"{"hooks":{"PostToolUse":[]}}"#),
                        )]),
                        frontmatter: BTreeMap::new(),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "mcp-binding:codex-project".to_string(),
                        entity_type: agentmesh_protocol::EntityType::McpBinding,
                        scope: None,
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("codex-project.toml"),
                            file(
                                "[mcp_servers.filesystem]\ncommand = \"node\"\nargs = [\"server.js\"]\n",
                            ),
                        )]),
                        frontmatter: BTreeMap::new(),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "permission-policy:codex-project".to_string(),
                        entity_type: agentmesh_protocol::EntityType::PermissionPolicy,
                        scope: None,
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("codex-project.toml"),
                            file(
                                "approval_policy = \"never\"\nsandbox_mode = \"read-only\"\n\n[profiles.locked]\napproval_policy = \"never\"\n",
                            ),
                        )]),
                        frontmatter: BTreeMap::new(),
                        overrides: BTreeMap::new(),
                    },
                ],
            })
            .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

        assert!(response.skipped.is_empty());
        assert!(
            response
                .files_written
                .contains(&PathBuf::from("packages/api/AGENTS.md"))
        );
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".codex/hooks.json"))
        );
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".codex/config.toml"))
        );
        let config = read(root.join(".codex/config.toml"));
        assert!(config.contains("model = \"gpt-5\""));
        assert!(config.contains("[hooks.PostToolUse]"));
        assert!(config.contains("[mcp_servers.filesystem]"));
        assert!(config.contains("[profiles.locked]"));
        assert!(config.contains("sandbox_mode = \"read-only\""));
    }

    #[test]
    fn emits_codex_hook_and_permission_policy_additively() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(
            root.join(".codex/hooks.json"),
            r#"{"metadata":{"owner":"user"},"hooks":{"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo user"}]}]}}"#,
        );
        write(
            root.join(".codex/config.toml"),
            "approval_policy = \"on-request\"\nsandbox_mode = \"workspace-write\"\nmodel = \"gpt-5\"\n",
        );
        let adapter = CodexAdapter;

        adapter
            .emit(EmitRequest {
                runtime_dir: root.join(".codex"),
                mode: RuntimeMode::Managed,
                entities: vec![
                    EmitEntity {
                        id: "hook:codex-project".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Hook,
                        scope: None,
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("codex-project.json"),
                            file(r#"{"hooks":{"PostToolUse":[{"matcher":"Edit","hooks":[{"type":"command","command":"agentmesh sync --trigger=codex-hook --silent"}]}]}}"#),
                        )]),
                        frontmatter: BTreeMap::new(),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "permission-policy:codex-project".to_string(),
                        entity_type: agentmesh_protocol::EntityType::PermissionPolicy,
                        scope: None,
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("codex-project.toml"),
                            file("approval_policy = \"never\"\n"),
                        )]),
                        frontmatter: BTreeMap::new(),
                        overrides: BTreeMap::new(),
                    },
                ],
            })
            .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

        let hooks = read(root.join(".codex/hooks.json"));
        assert!(hooks.contains("\"owner\": \"user\""));
        assert!(hooks.contains("echo user"));
        assert!(hooks.contains("codex-hook"));

        let config = read(root.join(".codex/config.toml"));
        assert!(config.contains("approval_policy = \"never\""));
        assert!(config.contains("sandbox_mode = \"workspace-write\""));
        assert!(config.contains("model = \"gpt-5\""));
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
                source_path: Some(entity.source_path),
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
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            panic!("parent directory should be created: {error}");
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
