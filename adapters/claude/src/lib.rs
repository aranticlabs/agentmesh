//! Bundled Claude adapter entry points.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod hooks;

use agentmesh_adapter_sdk_rust::{
    Adapter, AdapterError, AdapterMetadata, FormatTranslation, collect_entity_files,
    compose_frontmatter, dir_entry_file_type, ensure_hook_array, find_hook_array_mut,
    find_hook_group, hash_files, is_regular_dir, is_regular_file, is_safe_relative,
    max_mtime_string, mtime_string, parse_frontmatter, read_dir_sorted, read_json_object,
    read_to_string, remove_matching_entries, remove_recorded_entries, selected, sha256_bytes,
    skipped_entity, slug_for_entity, slugify, workspace_relative, workspace_root_for, write_atomic,
    write_json_pretty,
};
use agentmesh_protocol::{
    AdapterErrorCode, DetectResponse, EmitRequest, EmitResponse, EntityFile, EntityFileEncoding,
    EntityType, ImportFilter, ImportRequest, ImportResponse, ImportedEntity, InstallHooksRequest,
    InstallHooksResponse, InstalledHook, RemoveHooksRequest, RemoveHooksResponse, RuntimeMode,
    SkippedPath,
};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use serde_norway::{Mapping as YamlMapping, Value as YamlValue};

const SUPPORTED_ENTITIES: &[EntityType] = &[
    EntityType::Instructions,
    EntityType::Rule,
    EntityType::Command,
    EntityType::Hook,
    EntityType::McpBinding,
    EntityType::PermissionPolicy,
    EntityType::Skill,
    EntityType::Subagent,
];

const ALLOWED_READ_PATHS: &[&str] = &[".claude/**", ".mcp.json", "CLAUDE.md"];
const ALLOWED_WRITE_PATHS: &[&str] = &[".claude/**", ".mcp.json", "CLAUDE.md"];
const MARKDOWN_FORMATS: &[&str] = &["markdown"];
const JSON_FORMATS: &[&str] = &["json"];
const FORMAT_TRANSLATIONS: &[FormatTranslation] = &[
    FormatTranslation {
        entity_type: EntityType::Rule,
        formats: MARKDOWN_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::Command,
        formats: MARKDOWN_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::Hook,
        formats: JSON_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::McpBinding,
        formats: JSON_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::PermissionPolicy,
        formats: JSON_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::Subagent,
        formats: MARKDOWN_FORMATS,
    },
];

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
            workspace_root.join(".claude/rules"),
            workspace_root.join(".claude/commands"),
            workspace_root.join(".claude/settings.json"),
            workspace_root.join(".mcp.json"),
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
        let mut imported_root_instructions = false;
        if selected(filter, &[PathBuf::from("CLAUDE.md")])
            && is_regular_file(&workspace_root, &instructions_path)?
        {
            entities.push(import_markdown_entity(
                &workspace_root,
                &instructions_path,
                EntityType::Instructions,
                "instructions:root".to_string(),
                Some("root".to_string()),
                PathBuf::from("AGENTS.md"),
                PathBuf::from("CLAUDE.md"),
            )?);
            imported_root_instructions = true;
        }

        let dot_claude_instructions_path = request.runtime_dir.join("CLAUDE.md");
        let dot_claude_instructions_relative = PathBuf::from(".claude/CLAUDE.md");
        if !imported_root_instructions
            && selected(
                filter,
                std::slice::from_ref(&dot_claude_instructions_relative),
            )
            && is_regular_file(&workspace_root, &dot_claude_instructions_path)?
        {
            entities.push(import_markdown_entity(
                &workspace_root,
                &dot_claude_instructions_path,
                EntityType::Instructions,
                "instructions:root".to_string(),
                Some("root".to_string()),
                PathBuf::from("AGENTS.md"),
                dot_claude_instructions_relative,
            )?);
        }

        import_rules(
            &workspace_root,
            &request.runtime_dir.join("rules"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_commands(
            &workspace_root,
            &request.runtime_dir.join("commands"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_settings_json(
            &workspace_root,
            &request.runtime_dir.join("settings.json"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_mcp_json(&workspace_root, filter, &mut entities, &mut skipped)?;
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
                    let is_root = is_root_instruction(&entity.id, entity.scope.as_deref());
                    let frontmatter = claude_frontmatter_for_emit(&entity, !is_root);
                    let rendered = render_markdown_with_overrides(
                        &content,
                        &frontmatter,
                        &entity.overrides,
                        if is_root { &[] } else { &["globs"] },
                    )?;
                    let path = if is_root {
                        workspace_root.join("CLAUDE.md")
                    } else if let Some(source_path) =
                        native_source_path(&entity, ".claude/rules", "md")
                    {
                        workspace_root.join(source_path)
                    } else {
                        request
                            .runtime_dir
                            .join("rules")
                            .join(format!("{}.md", scoped_instruction_slug(&entity.id)))
                    };
                    write_atomic(&path, rendered.as_bytes())?;
                    files_written.push(workspace_relative(&workspace_root, &path)?);
                }
                EntityType::Rule => {
                    let slug = slug_for_entity(&entity.id, &entity.frontmatter);
                    let Some(content) = first_file_content(&entity.files) else {
                        skipped.push(skipped_entity(entity.id, "rule entity has no files"));
                        continue;
                    };
                    let rendered = render_markdown_with_overrides(
                        &content,
                        &entity.frontmatter,
                        &entity.overrides,
                        &[],
                    )?;
                    let target = native_source_path(&entity, ".claude/rules", "md")
                        .map(|path| workspace_root.join(path))
                        .unwrap_or_else(|| {
                            request.runtime_dir.join("rules").join(format!("{slug}.md"))
                        });
                    write_atomic(&target, rendered.as_bytes())?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                EntityType::Command => {
                    let Some(content) = first_file_content(&entity.files) else {
                        skipped.push(skipped_entity(entity.id, "command entity has no files"));
                        continue;
                    };
                    let rendered = render_markdown_with_overrides(
                        &content,
                        &entity.frontmatter,
                        &entity.overrides,
                        &[],
                    )?;
                    let target = native_source_path(&entity, ".claude/commands", "md")
                        .map(|path| workspace_root.join(path))
                        .unwrap_or_else(|| {
                            request
                                .runtime_dir
                                .join("commands")
                                .join(command_runtime_file(&entity.id, "md"))
                        });
                    write_atomic(&target, rendered.as_bytes())?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                EntityType::Hook => {
                    let target = request.runtime_dir.join("settings.json");
                    merge_json_section(&target, "hooks", &entity)?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                EntityType::McpBinding => {
                    let target = workspace_root.join(".mcp.json");
                    merge_json_section(&target, "mcpServers", &entity)?;
                    files_written.push(PathBuf::from(".mcp.json"));
                }
                EntityType::PermissionPolicy => {
                    let target = request.runtime_dir.join("settings.json");
                    merge_json_section(&target, "permissions", &entity)?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
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
                                &[],
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
                        &[],
                    )?;
                    let target = request
                        .runtime_dir
                        .join("agents")
                        .join(format!("{slug}.md"));
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

    import_rules_in_dir(
        workspace_root,
        rules_root,
        rules_root,
        filter,
        entities,
        skipped,
    )
}

fn import_rules_in_dir(
    workspace_root: &Path,
    rules_root: &Path,
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
                reason: "symlinked rule path is not supported".to_string(),
            });
            continue;
        }
        if file_type.is_dir() {
            import_rules_in_dir(workspace_root, rules_root, &path, filter, entities, skipped)?;
            continue;
        }
        if !file_type.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("md")
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
        let (entity_type, id, scope, canonical_path) = if frontmatter.contains_key("paths") {
            let scope = match scope_from_paths(&frontmatter, &slug) {
                Ok(scope) => scope,
                Err(reason) => {
                    skipped.push(SkippedPath {
                        path: source_relative,
                        reason,
                    });
                    continue;
                }
            };
            let slug = scope
                .as_deref()
                .and_then(scope_directory)
                .map(|path| slugify(&path.to_string_lossy()))
                .unwrap_or(slug);
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
                PathBuf::from("rules").join(format!("{slug}.md")),
            )
        };

        entities.push(import_markdown_entity(
            workspace_root,
            &path,
            entity_type,
            id,
            scope,
            canonical_path,
            source_relative,
        )?);
    }

    Ok(())
}

fn import_commands(
    workspace_root: &Path,
    commands_root: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    match is_regular_dir(workspace_root, commands_root) {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(error) => {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, commands_root),
                reason: error.to_string(),
            });
            return Ok(());
        }
    }

    import_commands_in_dir(
        workspace_root,
        commands_root,
        commands_root,
        filter,
        entities,
        skipped,
    )
}

fn import_commands_in_dir(
    workspace_root: &Path,
    commands_root: &Path,
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
                reason: "symlinked command path is not supported".to_string(),
            });
            continue;
        }
        if file_type.is_dir() {
            import_commands_in_dir(
                workspace_root,
                commands_root,
                &path,
                filter,
                entities,
                skipped,
            )?;
            continue;
        }
        if !file_type.is_file()
            || path.extension().and_then(|extension| extension.to_str()) != Some("md")
        {
            continue;
        }
        let source_relative = workspace_relative(workspace_root, &path)?;
        if !selected(filter, std::slice::from_ref(&source_relative)) {
            continue;
        }
        let slug = command_slug(commands_root, &path);
        let canonical_path =
            PathBuf::from("commands").join(command_runtime_file(&format!("command:{slug}"), "md"));
        let entity = match import_markdown_entity(
            workspace_root,
            &path,
            EntityType::Command,
            format!("command:{slug}"),
            None,
            canonical_path,
            source_relative,
        ) {
            Ok(entity) => entity,
            Err(error) => {
                skipped.push(SkippedPath {
                    path: workspace_relative(workspace_root, &path)?,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        entities.push(entity);
    }

    Ok(())
}

fn import_settings_json(
    workspace_root: &Path,
    settings_path: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let source_relative = PathBuf::from(".claude/settings.json");
    if !selected(filter, std::slice::from_ref(&source_relative))
        || !is_regular_file(workspace_root, settings_path)?
    {
        return Ok(());
    }
    let value = match read_json_object(settings_path) {
        Ok(value) => value,
        Err(error) => {
            skipped.push(SkippedPath {
                path: source_relative,
                reason: error.to_string(),
            });
            return Ok(());
        }
    };
    if let Some(hooks) = value.get("hooks") {
        entities.push(import_json_section_entity(
            settings_path,
            source_relative.clone(),
            EntityType::Hook,
            "hook:claude-project",
            PathBuf::from("hooks/claude-project.json"),
            "hooks",
            hooks.clone(),
        )?);
    }
    if let Some(permissions) = value.get("permissions") {
        entities.push(import_json_section_entity(
            settings_path,
            source_relative,
            EntityType::PermissionPolicy,
            "permission-policy:claude-project",
            PathBuf::from("permission-policies/claude-project.json"),
            "permissions",
            permissions.clone(),
        )?);
    }
    Ok(())
}

fn import_mcp_json(
    workspace_root: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let source_path = workspace_root.join(".mcp.json");
    let source_relative = PathBuf::from(".mcp.json");
    if !selected(filter, std::slice::from_ref(&source_relative))
        || !is_regular_file(workspace_root, &source_path)?
    {
        return Ok(());
    }
    let value = match read_json_object(&source_path) {
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
        &source_path,
        source_relative,
        EntityType::McpBinding,
        "mcp-binding:project",
        PathBuf::from("mcp-bindings/project.json"),
        value,
    )?);
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
            || path.extension().and_then(|extension| extension.to_str()) != Some("md")
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

        let entity = match import_markdown_entity(
            workspace_root,
            &path,
            EntityType::Subagent,
            format!("subagent:{slug}"),
            None,
            PathBuf::from("agents").join(format!("{slug}.md")),
            source_relative,
        ) {
            Ok(entity) => entity,
            Err(error) => {
                skipped.push(SkippedPath {
                    path: workspace_relative(workspace_root, &path)?,
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
    workspace_root: &Path,
    path: &Path,
    entity_type: EntityType,
    id: String,
    scope: Option<String>,
    canonical_path: PathBuf,
    source_path: PathBuf,
) -> agentmesh_adapter_sdk_rust::Result<ImportedEntity> {
    let content = read_to_string(path)?;
    let frontmatter = frontmatter_json_for_path(&source_path, &content)?;
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

fn import_json_section_entity(
    path: &Path,
    source_path: PathBuf,
    entity_type: EntityType,
    id: &str,
    canonical_path: PathBuf,
    section_key: &str,
    section_value: JsonValue,
) -> agentmesh_adapter_sdk_rust::Result<ImportedEntity> {
    let mut object = JsonMap::new();
    object.insert(section_key.to_string(), section_value);
    import_json_entity(
        path,
        source_path,
        entity_type,
        id,
        canonical_path,
        JsonValue::Object(object),
    )
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

fn is_root_instruction(id: &str, scope: Option<&str>) -> bool {
    id == "instructions:root" || scope == Some("root")
}

fn scoped_instruction_slug(id: &str) -> String {
    id.strip_prefix("instructions:scoped:")
        .map(ToString::to_string)
        .unwrap_or_else(|| slugify(id))
}

fn command_runtime_file(id: &str, extension: &str) -> PathBuf {
    let slug = id.strip_prefix("command:").unwrap_or(id);
    path_from_colon_slug(slug, extension)
}

fn path_from_colon_slug(slug: &str, extension: &str) -> PathBuf {
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

fn path_slug(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut parts = relative
        .iter()
        .filter_map(|part| part.to_str())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if let Some(last) = parts.last_mut()
        && let Some(stem) = Path::new(last)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToString::to_string)
    {
        *last = stem;
    }
    slugify(&parts.join("-"))
}

fn command_slug(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut parts = relative
        .iter()
        .filter_map(|part| part.to_str())
        .map(ToString::to_string)
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
        .join(":")
}

fn scope_from_paths(
    frontmatter: &BTreeMap<String, JsonValue>,
    fallback_slug: &str,
) -> Result<Option<String>, String> {
    match frontmatter.get("paths") {
        Some(JsonValue::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(JsonValue::Array(values)) => {
            let scopes = values
                .iter()
                .filter_map(JsonValue::as_str)
                .filter(|value| !value.trim().is_empty())
                .collect::<Vec<_>>();
            if scopes.len() > 1 {
                return Err(
                    "Claude rule paths with multiple scopes cannot be represented losslessly"
                        .to_string(),
                );
            }
            Ok(scopes
                .first()
                .map(|scope| (*scope).to_string())
                .or_else(|| Some(fallback_slug.to_string())))
        }
        _ => Ok(Some(fallback_slug.to_string())),
    }
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

fn native_source_path(
    entity: &agentmesh_protocol::EmitEntity,
    required_prefix: &str,
    extension: &str,
) -> Option<PathBuf> {
    let path = entity.source_path.as_ref()?;
    if !is_safe_relative(path) || !path.starts_with(required_prefix) {
        return None;
    }
    if path.extension().and_then(|value| value.to_str()) != Some(extension) {
        return None;
    }
    Some(path.clone())
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
            "settings JSON root must be an object",
        ));
    };
    let merged = merge_json_section_value(existing_object.remove(section_key), replacement);
    existing_object.insert(section_key.to_string(), merged);
    write_json_pretty(target, &existing)
}

fn merge_json_section_value(existing: Option<JsonValue>, replacement: JsonValue) -> JsonValue {
    match (existing, replacement) {
        (Some(JsonValue::Object(mut existing)), JsonValue::Object(replacement)) => {
            existing.extend(replacement);
            JsonValue::Object(existing)
        }
        (_, replacement) => replacement,
    }
}

fn json_object_from_entity(
    entity: &agentmesh_protocol::EmitEntity,
    label: &str,
) -> agentmesh_adapter_sdk_rust::Result<JsonMap<String, JsonValue>> {
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

fn claude_frontmatter_for_emit(
    entity: &agentmesh_protocol::EmitEntity,
    scoped_instruction: bool,
) -> BTreeMap<String, JsonValue> {
    let mut frontmatter = entity.frontmatter.clone();
    if scoped_instruction {
        frontmatter.remove("globs");
        if !frontmatter.contains_key("paths")
            && let Some(scope) = entity.scope.as_deref().filter(|scope| *scope != "root")
        {
            frontmatter.insert(
                "paths".to_string(),
                JsonValue::Array(vec![JsonValue::String(scope.to_string())]),
            );
        }
    }
    frontmatter
}

fn render_markdown_with_overrides(
    content: &str,
    frontmatter: &BTreeMap<String, JsonValue>,
    overrides: &BTreeMap<String, JsonValue>,
    excluded_frontmatter: &[&str],
) -> agentmesh_adapter_sdk_rust::Result<String> {
    if frontmatter.is_empty()
        && overrides.is_empty()
        && excluded_frontmatter.is_empty()
        && !content.starts_with("---\n")
    {
        return Ok(content.to_string());
    }
    let mut document = parse_frontmatter(content)?;
    for key in excluded_frontmatter {
        document
            .frontmatter
            .remove(YamlValue::String((*key).to_string()));
    }
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
    for key in excluded_frontmatter {
        document
            .frontmatter
            .remove(YamlValue::String((*key).to_string()));
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

    use super::ClaudeAdapter;

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
    fn import_skips_claude_entities_with_malformed_frontmatter() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(root.join("CLAUDE.md"), "# Instructions\n");
        write(
            root.join(".claude/skills/good/SKILL.md"),
            "---\nname: good\n---\nBody\n",
        );
        write(
            root.join(".claude/skills/bad/SKILL.md"),
            "---\ndescription: \"unterminated\n---\nBody\n",
        );
        write(
            root.join(".claude/agents/bad-agent.md"),
            "---\ndescription: \"unterminated\n---\nReview code.\n",
        );

        let adapter = ClaudeAdapter;
        let imported = match adapter.import(ImportRequest {
            canonical_dir: root.join(".ai"),
            runtime_dir: root.join(".claude"),
            filter: None,
        }) {
            Ok(imported) => imported,
            Err(error) => panic!("import should skip malformed entities: {error}"),
        };

        let ids = imported
            .entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&"instructions:root"));
        assert!(ids.contains(&"skill:good"));
        assert!(!ids.contains(&"skill:bad"));
        assert_eq!(imported.skipped.len(), 2);
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".claude/skills/bad/SKILL.md")
                && skipped.reason.contains("failed to parse frontmatter")
        }));
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".claude/agents/bad-agent.md")
                && skipped.reason.contains("failed to parse frontmatter")
        }));
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
                source_path: None,
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
                source_path: None,
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

    #[test]
    fn imports_claude_v02_project_surfaces() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(root.join("CLAUDE.md"), "# Instructions\n");
        write(
            root.join(".claude/rules/security.md"),
            "---\ndescription: Security\n---\nCheck input boundaries.\n",
        );
        write(
            root.join(".claude/rules/api.md"),
            "---\npaths:\n  - packages/api/**\n---\nUse API conventions.\n",
        );
        write(
            root.join(".claude/commands/git/commit.md"),
            "---\ndescription: Commit message\n---\nDraft a commit message.\n",
        );
        write(
            root.join(".claude/settings.json"),
            r#"{"hooks":{"PostToolUse":[]},"permissions":{"allow":["Bash(git status:*)"]},"theme":"dark"}"#,
        );
        write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"filesystem":{"command":"node","args":["server.js"]}}}"#,
        );

        let adapter = ClaudeAdapter;
        let imported = adapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".claude"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));
        let ids = imported
            .entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"rule:security"));
        assert!(ids.contains(&"instructions:scoped:packages-api"));
        assert!(ids.contains(&"command:git:commit"));
        assert!(ids.contains(&"hook:claude-project"));
        assert!(ids.contains(&"permission-policy:claude-project"));
        assert!(ids.contains(&"mcp-binding:project"));
        let scoped = imported
            .entities
            .iter()
            .find(|entity| entity.id == "instructions:scoped:packages-api")
            .unwrap_or_else(|| panic!("scoped rule should be imported"));
        assert_eq!(scoped.scope.as_deref(), Some("packages/api/**"));
    }

    #[test]
    fn skips_claude_rule_with_multiple_path_scopes() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(
            root.join(".claude/rules/multi.md"),
            "---\npaths:\n  - packages/api/**\n  - packages/web/**\n---\nUse package conventions.\n",
        );

        let imported = ClaudeAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".claude"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));

        assert!(imported.entities.is_empty());
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".claude/rules/multi.md")
                && skipped.reason.contains("multiple scopes")
        }));
    }

    #[test]
    fn emits_claude_v02_surfaces_without_clobbering_shared_settings() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(root.join(".claude/settings.json"), r#"{"theme":"dark"}"#);
        let adapter = ClaudeAdapter;

        let response = adapter
            .emit(EmitRequest {
                runtime_dir: root.join(".claude"),
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
                        id: "command:git:commit".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Command,
                        scope: None,
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("commit.md"),
                            file("Draft a commit message.\n"),
                        )]),
                        frontmatter: BTreeMap::new(),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "hook:claude-project".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Hook,
                        scope: None,
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("claude-project.json"),
                            file(r#"{"hooks":{"PostToolUse":[]}}"#),
                        )]),
                        frontmatter: BTreeMap::new(),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "permission-policy:claude-project".to_string(),
                        entity_type: agentmesh_protocol::EntityType::PermissionPolicy,
                        scope: None,
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("claude-project.json"),
                            file(r#"{"permissions":{"allow":["Bash(git status:*)"]}}"#),
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
                .contains(&PathBuf::from(".claude/rules/packages-api.md"))
        );
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".claude/commands/git/commit.md"))
        );
        let scoped = read(root.join(".claude/rules/packages-api.md"));
        assert!(scoped.contains("paths:"));
        assert!(!scoped.contains("globs:"));
        let settings = read(root.join(".claude/settings.json"));
        assert!(settings.contains("\"theme\": \"dark\""));
        assert!(settings.contains("\"hooks\""));
        assert!(settings.contains("\"permissions\""));
    }

    #[test]
    fn emits_claude_mcp_binding_without_clobbering_other_servers() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let root = temp.path();
        write(
            root.join(".mcp.json"),
            r#"{"mcpServers":{"filesystem":{"command":"old-node"},"browser":{"command":"browser-server"}},"metadata":{"owner":"user"}}"#,
        );
        let adapter = ClaudeAdapter;

        adapter
            .emit(EmitRequest {
                runtime_dir: root.join(".claude"),
                mode: RuntimeMode::Managed,
                entities: vec![EmitEntity {
                    id: "mcp-binding:project".to_string(),
                    entity_type: agentmesh_protocol::EntityType::McpBinding,
                    scope: None,
                    source_path: None,
                    files: BTreeMap::from([(
                        PathBuf::from("project.json"),
                        file(r#"{"mcpServers":{"filesystem":{"command":"node","args":["server.js"]}}}"#),
                    )]),
                    frontmatter: BTreeMap::new(),
                    overrides: BTreeMap::new(),
                }],
            })
            .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

        let mcp = read(root.join(".mcp.json"));
        assert!(mcp.contains("\"filesystem\""));
        assert!(mcp.contains("\"command\": \"node\""));
        assert!(mcp.contains("\"browser\""));
        assert!(mcp.contains("\"browser-server\""));
        assert!(mcp.contains("\"owner\": \"user\""));
        assert!(!mcp.contains("old-node"));
    }

    proptest! {
        #[test]
        fn skill_import_emit_import_roundtrip_preserves_entity_shape(
            slug in "[a-z][a-z0-9]{0,8}(-[a-z0-9]{1,8}){0,2}",
            body in prop::collection::vec("[A-Za-z0-9 .,]{0,40}", 1..4).prop_map(|lines| lines.join("\n")),
        ) {
            let temp = tempfile::tempdir()
                .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
            let root = temp.path();
            write(
                root.join("CLAUDE.md"),
                &format!("Root instructions\n{body}\n"),
            );
            write(
                root.join(format!(".claude/skills/{slug}/SKILL.md")),
                &format!("---\nname: {slug}\ntags:\n  - generated\n---\n{body}\n"),
            );
            write(
                root.join(format!(".claude/agents/{slug}-agent.md")),
                &format!("---\nname: {slug}-agent\ndescription: Generated\n---\n{body}\n"),
            );

            let adapter = ClaudeAdapter;
            let imported = adapter
                .import(ImportRequest {
                    canonical_dir: root.join(".ai"),
                    runtime_dir: root.join(".claude"),
                    filter: None,
                })
                .unwrap_or_else(|error| panic!("import should succeed: {error}"));
            let emit_entities = emit_entities(imported.entities.clone());

            adapter
                .emit(EmitRequest {
                    runtime_dir: root.join(".claude-roundtrip"),
                    mode: RuntimeMode::Managed,
                    entities: emit_entities,
                })
                .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

            let roundtripped = adapter
                .import(ImportRequest {
                    canonical_dir: root.join(".ai"),
                    runtime_dir: root.join(".claude-roundtrip"),
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
