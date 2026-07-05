//! Bundled Gemini CLI adapter entry points.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentmesh_adapter_sdk_rust::{
    Adapter, AdapterError, AdapterMetadata, FormatTranslation, collect_entity_files,
    compose_frontmatter, dir_entry_file_type, hash_files, is_regular_dir, is_regular_file,
    is_safe_relative, max_mtime_string, mtime_string, parse_frontmatter, read_dir_sorted,
    read_json_object, read_to_string, selected, sha256_bytes, skipped_entity, slug_for_entity,
    slugify, workspace_relative, workspace_root_for, write_atomic,
};
use agentmesh_protocol::{
    AdapterErrorCode, DetectResponse, EmitRequest, EmitResponse, EntityFile, EntityFileEncoding,
    EntityType, ImportFilter, ImportRequest, ImportResponse, ImportedEntity, InstallHooksRequest,
    InstallHooksResponse, RemoveHooksRequest, RemoveHooksResponse, RuntimeMode, SkippedPath,
};
use serde_json::{Number as JsonNumber, Value as JsonValue};
use serde_norway::{Mapping as YamlMapping, Value as YamlValue};

const SUPPORTED_ENTITIES: &[EntityType] = &[
    EntityType::Instructions,
    EntityType::Command,
    EntityType::Skill,
];
const ALLOWED_READ_PATHS: &[&str] = &[
    "GEMINI.md",
    "**/GEMINI.md",
    ".gemini/skills/**",
    ".agents/skills/**",
    ".gemini/commands/**",
    ".gemini/settings.json",
    ".gemini/agents/**",
    ".gemini/hooks/**",
    ".gemini/extensions/**",
    "gemini-extension.json",
];
const ALLOWED_WRITE_PATHS: &[&str] = &[
    "GEMINI.md",
    "**/GEMINI.md",
    ".gemini/skills/**",
    ".gemini/commands/**",
];
const MARKDOWN_FORMATS: &[&str] = &["markdown"];
const TOML_FORMATS: &[&str] = &["toml"];
const FORMAT_TRANSLATIONS: &[FormatTranslation] = &[
    FormatTranslation {
        entity_type: EntityType::Instructions,
        formats: MARKDOWN_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::Command,
        formats: TOML_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::Skill,
        formats: MARKDOWN_FORMATS,
    },
];

/// Gemini CLI adapter handle.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiAdapter;

impl Adapter for GeminiAdapter {
    fn metadata(&self) -> AdapterMetadata {
        metadata()
    }

    fn detect(&self, workspace_root: &Path) -> agentmesh_adapter_sdk_rust::Result<DetectResponse> {
        let mut files = Vec::new();
        collect_context_evidence(workspace_root, workspace_root, &mut files)?;
        for path in [
            workspace_root.join(".gemini/skills"),
            workspace_root.join(".gemini/commands"),
        ] {
            let present = if path.is_file() {
                is_regular_file(workspace_root, &path)?
            } else {
                is_regular_dir(workspace_root, &path)?
            };
            if present {
                files.push(workspace_relative(workspace_root, &path)?);
            }
        }

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

        import_context_files(&workspace_root, filter, &mut entities, &mut skipped)?;
        import_skills(
            &workspace_root,
            &workspace_root.join(".gemini/skills"),
            false,
            filter,
            &mut entities,
            &mut skipped,
        )?;
        if has_write_enabled_gemini_evidence(&workspace_root)? {
            import_skills(
                &workspace_root,
                &workspace_root.join(".agents/skills"),
                true,
                filter,
                &mut entities,
                &mut skipped,
            )?;
        }
        import_commands(
            &workspace_root,
            &workspace_root.join(".gemini/commands"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_settings_diagnostics(&workspace_root, filter, &mut skipped)?;
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
                    let target = match gemini_context_target(&workspace_root, &entity) {
                        Ok(target) => target,
                        Err(reason) => {
                            skipped.push(skipped_entity(entity.id, reason));
                            continue;
                        }
                    };
                    validate_gemini_write_path(
                        &workspace_root,
                        &target,
                        WriteSurface::Context,
                        "Gemini context",
                    )?;
                    write_atomic(&target, content.as_bytes())?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                EntityType::Skill => {
                    let slug = slug_for_entity(&entity.id, &entity.frontmatter);
                    let skill_root = gemini_skill_target_root(&workspace_root, &entity, &slug);
                    validate_gemini_write_path(
                        &workspace_root,
                        &skill_root.join("SKILL.md"),
                        WriteSurface::Skill,
                        "Gemini skill",
                    )?;
                    let frontmatter = skill_frontmatter_for_emit(&entity);
                    for (file_path, file) in &entity.files {
                        if !is_safe_relative(file_path) {
                            return Err(AdapterError::rpc(
                                AdapterErrorCode::WorkspaceOutsideBound,
                                format!("unsafe Gemini skill file path {}", file_path.display()),
                            ));
                        }
                        let target = skill_root.join(file_path);
                        validate_gemini_write_path(
                            &workspace_root,
                            &target,
                            WriteSurface::Skill,
                            "Gemini skill",
                        )?;
                        let bytes = if file_path == Path::new("SKILL.md")
                            && file.encoding == EntityFileEncoding::Utf8
                        {
                            render_markdown_with_frontmatter(
                                &file.content,
                                &frontmatter,
                                &entity.overrides,
                            )?
                            .into_bytes()
                        } else {
                            entity_file_bytes(file_path, file)?
                        };
                        write_atomic(&target, &bytes)?;
                        files_written.push(workspace_relative(&workspace_root, &target)?);
                    }
                }
                EntityType::Command => {
                    let Some((file_path, content)) = first_text_file(&entity.files) else {
                        skipped.push(skipped_entity(entity.id, "command entity has no files"));
                        continue;
                    };
                    let slug = entity.id.strip_prefix("command:").unwrap_or(&entity.id);
                    let target = native_source_path(&entity, ".gemini/commands", "toml")
                        .map(|path| workspace_root.join(path))
                        .unwrap_or_else(|| {
                            workspace_root
                                .join(".gemini/commands")
                                .join(command_file_name(slug, "toml"))
                        });
                    validate_gemini_write_path(
                        &workspace_root,
                        &target,
                        WriteSurface::Command,
                        "Gemini command",
                    )?;
                    let frontmatter = command_frontmatter_for_emit(&entity);
                    let rendered =
                        if file_path.extension().and_then(|value| value.to_str()) == Some("toml") {
                            render_existing_toml_command(
                                &content,
                                &frontmatter,
                                &entity.overrides,
                                &file_path,
                            )?
                        } else {
                            render_toml_command(&content, &frontmatter, &entity.overrides)?
                        };
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

/// Returns static metadata for the Gemini CLI adapter.
#[must_use]
pub const fn metadata() -> AdapterMetadata {
    AdapterMetadata {
        name: "gemini",
        runtime_dir: ".gemini",
        supported_entities: SUPPORTED_ENTITIES,
        allowed_read_paths: ALLOWED_READ_PATHS,
        allowed_write_paths: ALLOWED_WRITE_PATHS,
        format_translations: FORMAT_TRANSLATIONS,
    }
}

fn collect_context_evidence(
    workspace_root: &Path,
    dir: &Path,
    files: &mut Vec<PathBuf>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    for entry in read_dir_sorted(dir)? {
        let path = entry.path();
        let file_type = dir_entry_file_type(&entry)?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if should_skip_context_dir(workspace_root, &path) {
                continue;
            }
            collect_context_evidence(workspace_root, &path, files)?;
            continue;
        }
        if file_type.is_file() && is_gemini_context_file(&path) {
            files.push(workspace_relative(workspace_root, &path)?);
        }
    }
    Ok(())
}

fn import_context_files(
    workspace_root: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    import_context_files_in_dir(workspace_root, workspace_root, filter, entities, skipped)
}

fn import_context_files_in_dir(
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
                reason: "symlinked Gemini context path is not supported".to_string(),
            });
            continue;
        }
        if file_type.is_dir() {
            if should_skip_context_dir(workspace_root, &path) {
                continue;
            }
            import_context_files_in_dir(workspace_root, &path, filter, entities, skipped)?;
            continue;
        }
        if !file_type.is_file() || !is_gemini_context_file(&path) {
            continue;
        }
        let source_relative = workspace_relative(workspace_root, &path)?;
        if !selected(filter, std::slice::from_ref(&source_relative)) {
            continue;
        }
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
        if source_relative == Path::new("GEMINI.md") {
            entities.push(ImportedEntity {
                id: "instructions:root".to_string(),
                entity_type: EntityType::Instructions,
                scope: Some("root".to_string()),
                canonical_path: PathBuf::from("AGENTS.md"),
                files: BTreeMap::from([(
                    PathBuf::from("AGENTS.md"),
                    EntityFile::utf8(content.clone()),
                )]),
                frontmatter: BTreeMap::new(),
                canonical_sha256: sha256_bytes(content.as_bytes()),
                source_path: source_relative,
                source_mtime: mtime_string(&path)?,
            });
            continue;
        }
        let Some(scope_dir) = source_relative.parent() else {
            continue;
        };
        let slug = path_slug(scope_dir);
        let scope = format!("{}/**", scope_dir.to_string_lossy().replace('\\', "/"));
        entities.push(ImportedEntity {
            id: format!("instructions:scoped:{slug}"),
            entity_type: EntityType::Instructions,
            scope: Some(scope),
            canonical_path: PathBuf::from("instructions").join(format!("{slug}.md")),
            files: BTreeMap::from([(
                PathBuf::from(format!("{slug}.md")),
                EntityFile::utf8(content.clone()),
            )]),
            frontmatter: BTreeMap::new(),
            canonical_sha256: sha256_bytes(content.as_bytes()),
            source_path: source_relative,
            source_mtime: mtime_string(&path)?,
        });
    }
    Ok(())
}

fn has_write_enabled_gemini_evidence(
    workspace_root: &Path,
) -> agentmesh_adapter_sdk_rust::Result<bool> {
    let mut files = Vec::new();
    collect_context_evidence(workspace_root, workspace_root, &mut files)?;
    if !files.is_empty() {
        return Ok(true);
    }
    for path in [
        workspace_root.join(".gemini/skills"),
        workspace_root.join(".gemini/commands"),
    ] {
        let present = if path.is_file() {
            is_regular_file(workspace_root, &path)?
        } else {
            is_regular_dir(workspace_root, &path)?
        };
        if present {
            return Ok(true);
        }
    }
    Ok(false)
}

fn should_skip_context_dir(workspace_root: &Path, path: &Path) -> bool {
    let relative = path.strip_prefix(workspace_root).unwrap_or(path);
    !gemini_context_parent_allowed(relative)
}

fn import_skills(
    workspace_root: &Path,
    root: &Path,
    shared: bool,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
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

    for entry in read_dir_sorted(root)? {
        let path = entry.path();
        let file_type = dir_entry_file_type(&entry)?;
        if file_type.is_symlink() {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, &path),
                reason: "symlinked Gemini skill path is not supported".to_string(),
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
        let content = match read_to_string(&source_path) {
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
                let prefix = if shared {
                    "invalid shared Gemini skill frontmatter must not be overwritten"
                } else {
                    "invalid Gemini skill frontmatter must not be overwritten"
                };
                skipped.push(SkippedPath {
                    path: source_relative,
                    reason: format!("{prefix}: {error}"),
                });
                continue;
            }
        };
        if !shared {
            let canonical = render_canonical_markdown(
                &content,
                &portable_frontmatter(&frontmatter, &["name", "description"]),
            )?;
            files.insert(PathBuf::from("SKILL.md"), EntityFile::utf8(canonical));
        }

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

fn import_commands(
    workspace_root: &Path,
    root: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
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
    import_commands_inner(workspace_root, root, root, filter, entities, skipped)
}

fn import_commands_inner(
    workspace_root: &Path,
    root: &Path,
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
                reason: "symlinked Gemini command path is not supported".to_string(),
            });
            continue;
        }
        if file_type.is_dir() {
            import_commands_inner(workspace_root, root, &path, filter, entities, skipped)?;
            continue;
        }
        if !file_type.is_file() || path.extension().and_then(|value| value.to_str()) != Some("toml")
        {
            continue;
        }
        let source_relative = workspace_relative(workspace_root, &path)?;
        if !selected(filter, std::slice::from_ref(&source_relative)) {
            continue;
        }
        let slug = command_slug(root, &path);
        match import_toml_command(&path, source_relative.clone(), &slug) {
            Ok(entity) => entities.push(entity),
            Err(error) => skipped.push(SkippedPath {
                path: source_relative,
                reason: error.to_string(),
            }),
        }
    }
    Ok(())
}

fn import_toml_command(
    path: &Path,
    source_path: PathBuf,
    slug: &str,
) -> agentmesh_adapter_sdk_rust::Result<ImportedEntity> {
    let content = read_to_string(path)?;
    let table = toml_table_from_str(&content, path)?;
    let Some(prompt) = table.get("prompt").and_then(toml::Value::as_str) else {
        return Err(AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            "missing required prompt field",
        ));
    };
    if prompt.trim().is_empty() {
        return Err(AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            "missing required prompt field",
        ));
    }
    let frontmatter = table
        .iter()
        .filter(|(key, _)| key.as_str() != "prompt")
        .map(|(key, value)| (key.clone(), toml_to_json(value)))
        .collect::<BTreeMap<_, _>>();
    let canonical = render_markdown_with_frontmatter(
        prompt,
        &portable_frontmatter(&frontmatter, &["description"]),
        &BTreeMap::new(),
    )?;
    let file_key = command_file_name(slug, "md");

    Ok(ImportedEntity {
        id: format!("command:{slug}"),
        entity_type: EntityType::Command,
        scope: None,
        canonical_path: PathBuf::from("commands").join(&file_key),
        files: BTreeMap::from([(file_key, EntityFile::utf8(canonical.clone()))]),
        frontmatter,
        canonical_sha256: sha256_bytes(canonical.as_bytes()),
        source_path,
        source_mtime: mtime_string(path)?,
    })
}

fn import_settings_diagnostics(
    workspace_root: &Path,
    filter: Option<&ImportFilter>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let path = workspace_root.join(".gemini/settings.json");
    let relative = PathBuf::from(".gemini/settings.json");
    if !selected(filter, std::slice::from_ref(&relative))
        || !is_regular_file(workspace_root, &path)?
    {
        return Ok(());
    }
    let value = match read_json_object(&path) {
        Ok(value) => value,
        Err(error) => {
            skipped.push(SkippedPath {
                path: relative,
                reason: error.to_string(),
            });
            return Ok(());
        }
    };
    if value.get("mcpServers").is_some() || value.get("mcp").is_some() {
        skipped.push(SkippedPath {
            path: relative.clone(),
            reason: "project MCP settings are diagnostics-only and must never be emitted"
                .to_string(),
        });
    }
    if value.get("policyPaths").is_some() || value.get("adminPolicyPaths").is_some() {
        skipped.push(SkippedPath {
            path: relative.clone(),
            reason: "project policy settings are diagnostics-only and must never be emitted"
                .to_string(),
        });
    }
    if value
        .get("context")
        .and_then(JsonValue::as_object)
        .is_some_and(|context| context.contains_key("fileName"))
    {
        skipped.push(SkippedPath {
            path: relative.clone(),
            reason: "custom context filenames are detected but only GEMINI.md is emitted"
                .to_string(),
        });
    }
    if value.get("hooks").is_some() {
        skipped.push(SkippedPath {
            path: relative,
            reason: "Gemini hooks are deferred and must never be emitted".to_string(),
        });
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
        &workspace_root.join(".gemini/agents"),
        filter,
        "Gemini subagents are deferred and must never be emitted",
        skipped,
    )?;
    import_diagnostic_file_tree(
        workspace_root,
        &workspace_root.join(".gemini/hooks"),
        filter,
        "Gemini hooks are deferred and must never be emitted",
        skipped,
    )?;
    import_diagnostic_file_tree(
        workspace_root,
        &workspace_root.join(".gemini/extensions"),
        filter,
        "Gemini extensions are deferred and must never be emitted",
        skipped,
    )?;
    import_diagnostic_file(
        workspace_root,
        &workspace_root.join("gemini-extension.json"),
        filter,
        "Gemini extensions are deferred and must never be emitted",
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

fn gemini_context_target(
    workspace_root: &Path,
    entity: &agentmesh_protocol::EmitEntity,
) -> Result<PathBuf, String> {
    if is_root_instruction(&entity.id, entity.scope.as_deref()) {
        return Ok(workspace_root.join("GEMINI.md"));
    }
    if let Some(path) = entity
        .source_path
        .as_deref()
        .filter(|path| is_native_gemini_context_path(path))
    {
        return Ok(workspace_root.join(path));
    }
    let Some(scope) = entity.scope.as_deref().filter(|scope| *scope != "root") else {
        return Err("scoped Gemini context requires a scope".to_string());
    };
    let Some(scope_dir) = scope_dir_from_scope(scope) else {
        return Err("scoped Gemini context scope must end in /**".to_string());
    };
    Ok(workspace_root.join(scope_dir).join("GEMINI.md"))
}

fn scope_dir_from_scope(scope: &str) -> Option<PathBuf> {
    let value = scope.strip_suffix("/**")?.trim_matches('/');
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    if !is_safe_relative(&path) || !gemini_context_path_allowed(&path.join("GEMINI.md")) {
        return None;
    }
    Some(path)
}

fn is_native_gemini_context_path(path: &Path) -> bool {
    is_safe_relative(path)
        && path.file_name().and_then(|value| value.to_str()) == Some("GEMINI.md")
        && gemini_context_path_allowed(path)
}

fn gemini_context_path_allowed(path: &Path) -> bool {
    if path == Path::new("GEMINI.md") {
        return true;
    }
    if !is_safe_relative(path)
        || path.file_name().and_then(|name| name.to_str()) != Some("GEMINI.md")
    {
        return false;
    }
    path.parent().is_some_and(gemini_context_parent_allowed)
}

fn gemini_context_parent_allowed(path: &Path) -> bool {
    path.components().all(|component| {
        let std::path::Component::Normal(part) = component else {
            return false;
        };
        part.to_str()
            .is_some_and(|part| !part.starts_with('.') && part != "target")
    })
}

fn gemini_skill_target_root(
    workspace_root: &Path,
    entity: &agentmesh_protocol::EmitEntity,
    slug: &str,
) -> PathBuf {
    entity
        .source_path
        .as_ref()
        .and_then(|path| direct_skill_source_root(path, ".gemini/skills"))
        .map(|path| workspace_root.join(path))
        .unwrap_or_else(|| workspace_root.join(".gemini/skills").join(slug))
}

fn direct_skill_source_root(path: &Path, root: &str) -> Option<PathBuf> {
    if !is_safe_relative(path)
        || path.file_name().and_then(|value| value.to_str()) != Some("SKILL.md")
        || !path.starts_with(root)
    {
        return None;
    }
    let parent = path.parent()?;
    let relative = parent.strip_prefix(root).ok()?;
    if relative.components().count() == 1 {
        Some(parent.to_path_buf())
    } else {
        None
    }
}

fn skill_frontmatter_for_emit(
    entity: &agentmesh_protocol::EmitEntity,
) -> BTreeMap<String, JsonValue> {
    if entity
        .source_path
        .as_ref()
        .is_some_and(|path| direct_skill_source_root(path, ".gemini/skills").is_some())
    {
        entity.frontmatter.clone()
    } else {
        portable_frontmatter(&entity.frontmatter, &["name", "description"])
    }
}

fn command_frontmatter_for_emit(
    entity: &agentmesh_protocol::EmitEntity,
) -> BTreeMap<String, JsonValue> {
    if entity
        .source_path
        .as_ref()
        .is_some_and(|_| native_source_path(entity, ".gemini/commands", "toml").is_some())
    {
        entity.frontmatter.clone()
    } else {
        portable_frontmatter(&entity.frontmatter, &["description"])
    }
}

fn native_source_path(
    entity: &agentmesh_protocol::EmitEntity,
    native_root: &str,
    extension: &str,
) -> Option<PathBuf> {
    let path = entity.source_path.as_ref()?;
    if is_safe_relative(path)
        && path.starts_with(native_root)
        && path.extension().and_then(|value| value.to_str()) == Some(extension)
    {
        Some(path.clone())
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy)]
enum WriteSurface {
    Context,
    Skill,
    Command,
}

fn validate_gemini_write_path(
    workspace_root: &Path,
    target: &Path,
    surface: WriteSurface,
    label: &str,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let relative = workspace_relative(workspace_root, target)?;
    if !is_safe_relative(&relative) {
        return Err(AdapterError::rpc(
            AdapterErrorCode::WorkspaceOutsideBound,
            format!("unsafe {label} path {}", target.display()),
        ));
    }
    let allowed = match surface {
        WriteSurface::Context => is_native_gemini_context_path(&relative),
        WriteSurface::Skill => relative.starts_with(".gemini/skills"),
        WriteSurface::Command => {
            relative.starts_with(".gemini/commands")
                && relative.extension().and_then(|value| value.to_str()) == Some("toml")
        }
    };
    if !allowed {
        return Err(AdapterError::rpc(
            AdapterErrorCode::WorkspaceOutsideBound,
            format!("{} is outside declared {label} roots", target.display()),
        ));
    }
    let parent = target.parent().ok_or_else(|| {
        AdapterError::rpc(
            AdapterErrorCode::WorkspaceOutsideBound,
            format!("{label} target has no parent: {}", target.display()),
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
                format!("unsafe Gemini path component in {}", path.display()),
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
                    "symlinked Gemini path {} is not supported",
                    current.display()
                ),
            ));
        }
        if require_directory && !metadata.is_dir() {
            return Err(AdapterError::rpc(
                AdapterErrorCode::WorkspaceOutsideBound,
                format!(
                    "Gemini path component is not a directory: {}",
                    current.display()
                ),
            ));
        }
    }
    Ok(())
}

fn render_canonical_markdown(
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

fn render_toml_command(
    content: &str,
    frontmatter: &BTreeMap<String, JsonValue>,
    overrides: &BTreeMap<String, JsonValue>,
) -> agentmesh_adapter_sdk_rust::Result<String> {
    let document = parse_frontmatter(content)?;
    let body = document.body;
    if body.trim().is_empty() {
        return Err(AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            "Gemini command prompt cannot be empty",
        ));
    }
    let mut merged = BTreeMap::new();
    for (key, value) in frontmatter {
        merged.insert(key.clone(), value.clone());
    }
    for (key, value) in overrides {
        merged.insert(key.clone(), value.clone());
    }
    let mut table = toml::map::Map::new();
    for (key, value) in merged {
        if key == "prompt" {
            continue;
        }
        if let Some(value) = json_to_toml(&value) {
            table.insert(key, value);
        }
    }
    table.insert("prompt".to_string(), toml::Value::String(body));
    Ok(serialize_toml_table(&table))
}

fn render_existing_toml_command(
    content: &str,
    frontmatter: &BTreeMap<String, JsonValue>,
    overrides: &BTreeMap<String, JsonValue>,
    path: &Path,
) -> agentmesh_adapter_sdk_rust::Result<String> {
    let mut table = toml_table_from_str(content, path)?;
    insert_missing_toml_metadata(&mut table, frontmatter);
    insert_missing_toml_metadata(&mut table, overrides);
    let Some(prompt) = table.get("prompt").and_then(toml::Value::as_str) else {
        return Err(AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            "missing required prompt field",
        ));
    };
    if prompt.trim().is_empty() {
        return Err(AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            "missing required prompt field",
        ));
    }
    Ok(serialize_toml_table(&table))
}

fn insert_missing_toml_metadata(
    table: &mut toml::map::Map<String, toml::Value>,
    metadata: &BTreeMap<String, JsonValue>,
) {
    for (key, value) in metadata {
        if key == "prompt" || table.contains_key(key) {
            continue;
        }
        if let Some(value) = json_to_toml(value) {
            table.insert(key.clone(), value);
        }
    }
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
    serde_norway::to_value(value).map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!("failed to convert JSON value to YAML: {source}"),
        )
    })
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
    for (key, value) in table {
        if !matches!(value, toml::Value::Table(_)) {
            output.push_str(&quote_toml_key(key));
            output.push_str(" = ");
            output.push_str(&inline_toml_value(value));
            output.push('\n');
        }
    }
    for (key, value) in table {
        let toml::Value::Table(child) = value else {
            continue;
        };
        if !output.is_empty() {
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
    let values = table
        .iter()
        .map(|(key, value)| format!("{} = {}", quote_toml_key(key), inline_toml_value(value)))
        .collect::<Vec<_>>();
    format!("{{ {} }}", values.join(", "))
}

fn quote_toml_key(key: &str) -> String {
    if key
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '_' || character == '-')
    {
        key.to_string()
    } else {
        quote_toml_string(key)
    }
}

fn quote_toml_string(value: &str) -> String {
    let mut output = String::from("\"");
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

fn portable_frontmatter(
    frontmatter: &BTreeMap<String, JsonValue>,
    keys: &[&str],
) -> BTreeMap<String, JsonValue> {
    frontmatter
        .iter()
        .filter(|(key, _)| keys.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn entity_file_bytes(
    path: &Path,
    file: &EntityFile,
) -> agentmesh_adapter_sdk_rust::Result<Vec<u8>> {
    file.decode_bytes().map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!(
                "failed to decode Gemini entity file {}: {source}",
                path.display()
            ),
        )
    })
}

fn first_file_content(files: &BTreeMap<PathBuf, EntityFile>) -> Option<String> {
    files.values().find_map(file_text)
}

fn first_text_file(files: &BTreeMap<PathBuf, EntityFile>) -> Option<(PathBuf, String)> {
    files
        .iter()
        .find_map(|(path, file)| file_text(file).map(|content| (path.clone(), content)))
}

fn file_text(file: &EntityFile) -> Option<String> {
    match file.encoding {
        EntityFileEncoding::Utf8 => Some(file.content.clone()),
        EntityFileEncoding::Base64 => None,
    }
}

fn is_root_instruction(id: &str, scope: Option<&str>) -> bool {
    id == "instructions:root" || scope == Some("root")
}

fn is_gemini_context_file(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("GEMINI.md")
}

fn path_slug(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str().map(slugify),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn command_slug(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut parts = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str().map(ToString::to_string),
            _ => None,
        })
        .collect::<Vec<_>>();
    if let Some(last) = parts.last_mut() {
        let stem = last.strip_suffix(".toml").unwrap_or(last).to_string();
        *last = stem;
    }
    parts
        .into_iter()
        .map(|part| slugify(&part))
        .collect::<Vec<_>>()
        .join(":")
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

    use super::GeminiAdapter;

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
    fn detect_ignores_shared_and_diagnostic_only_surfaces() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(
            root.join(".agents/skills/shared-analysis/SKILL.md"),
            "---\nname: shared-analysis\n---\n# Shared\n",
        );
        write(
            root.join(".gemini/settings.json"),
            r#"{"mcpServers":{"repo":{"command":"repo-tools"}}}"#,
        );

        let detected = GeminiAdapter
            .detect(root)
            .unwrap_or_else(|error| panic!("detect should succeed: {error}"));

        assert!(!detected.present);
        assert!(detected.files.is_empty());
    }

    #[test]
    fn ignores_hidden_nested_context_files() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(root.join(".secrets/GEMINI.md"), "# Root secret\n");
        write(root.join("packages/.git/GEMINI.md"), "# Git\n");
        write(root.join("packages/.secrets/GEMINI.md"), "# Secret\n");
        write(
            root.join("packages/.gemini/GEMINI.md"),
            "# Nested runtime\n",
        );
        write(root.join("packages/api/GEMINI.md"), "# API\n");

        let detected = GeminiAdapter
            .detect(root)
            .unwrap_or_else(|error| panic!("detect should succeed: {error}"));
        let imported = GeminiAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".gemini"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));
        let ids = imported
            .entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();

        assert!(
            detected
                .files
                .contains(&PathBuf::from("packages/api/GEMINI.md"))
        );
        assert_eq!(ids, vec!["instructions:scoped:packages-api"]);
    }

    #[test]
    fn imports_gemini_write_enabled_surfaces_and_diagnostics() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(root.join("GEMINI.md"), "# Root\n@./docs/style.md\n");
        write(root.join("packages/api/GEMINI.md"), "# API\n");
        write(
            root.join(".gemini/skills/release-check/SKILL.md"),
            "---\nname: release-check\ndescription: Release\nowner: platform\n---\n# Release\n",
        );
        write(
            root.join(".gemini/skills/release-check/references/checks.md"),
            "# Checks\n",
        );
        write(
            root.join(".agents/skills/shared-analysis/SKILL.md"),
            "---\nname: shared-analysis\ndescription: Shared\n---\n# Shared\n",
        );
        write(
            root.join(".gemini/commands/git/commit.toml"),
            "description = \"Commit\"\nmode = \"builtin\"\nprompt = \"Draft commit for {{args}}.\"\n",
        );
        write(
            root.join(".gemini/settings.json"),
            r#"{"mcpServers":{"repo":{"command":"repo-tools"}},"policyPaths":[".gemini/policies"],"context":{"fileName":["CONTEXT.md","GEMINI.md"]}}"#,
        );
        write(root.join(".gemini/agents/investigator.md"), "# Agent\n");
        write(root.join("gemini-extension.json"), "{}\n");

        let imported = GeminiAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".gemini"),
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
        assert!(ids.contains(&"skill:release-check"));
        assert!(ids.contains(&"skill:shared-analysis"));
        assert!(ids.contains(&"command:git:commit"));
        let root_context = imported
            .entities
            .iter()
            .find(|entity| entity.id == "instructions:root")
            .unwrap_or_else(|| panic!("root context should import"));
        assert!(
            root_context.files[Path::new("AGENTS.md")]
                .content
                .contains("@./docs/style.md")
        );
        let scoped = imported
            .entities
            .iter()
            .find(|entity| entity.id == "instructions:scoped:packages-api")
            .unwrap_or_else(|| panic!("scoped context should import"));
        assert_eq!(scoped.scope.as_deref(), Some("packages/api/**"));
        let command = imported
            .entities
            .iter()
            .find(|entity| entity.id == "command:git:commit")
            .unwrap_or_else(|| panic!("command should import"));
        assert_eq!(command.frontmatter.get("mode"), Some(&json!("builtin")));
        assert!(
            command.files[Path::new("git/commit.md")]
                .content
                .contains("Draft commit for {{args}}.")
        );
        for expected in [
            "project MCP settings are diagnostics-only",
            "project policy settings are diagnostics-only",
            "custom context filenames are detected",
            "Gemini subagents are deferred",
            "Gemini extensions are deferred",
        ] {
            assert!(
                imported
                    .skipped
                    .iter()
                    .any(|skipped| skipped.reason.contains(expected)),
                "missing diagnostic {expected}; skipped: {:?}",
                imported.skipped
            );
        }
    }

    #[test]
    fn emits_gemini_context_skills_and_commands() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();

        let response = GeminiAdapter
            .emit(EmitRequest {
                runtime_dir: root.join(".gemini"),
                mode: RuntimeMode::Managed,
                entities: vec![
                    EmitEntity {
                        id: "instructions:root".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Instructions,
                        scope: Some("root".to_string()),
                        source_path: None,
                        files: BTreeMap::from([(PathBuf::from("AGENTS.md"), file("# Root\n"))]),
                        frontmatter: BTreeMap::new(),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "instructions:scoped:packages-api".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Instructions,
                        scope: Some("packages/api/**".to_string()),
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("packages-api.md"),
                            file("# API\n"),
                        )]),
                        frontmatter: BTreeMap::new(),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "skill:shared-analysis".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Skill,
                        scope: None,
                        source_path: Some(PathBuf::from(".agents/skills/shared-analysis/SKILL.md")),
                        files: BTreeMap::from([(PathBuf::from("SKILL.md"), file("# Shared\n"))]),
                        frontmatter: BTreeMap::from([
                            ("name".to_string(), json!("shared-analysis")),
                            ("owner".to_string(), json!("platform")),
                        ]),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "command:git:commit".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Command,
                        scope: None,
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("commit.md"),
                            file("---\ndescription: Commit\n---\nDraft commit for {{args}}.\n"),
                        )]),
                        frontmatter: BTreeMap::from([("description".to_string(), json!("Commit"))]),
                        overrides: BTreeMap::from([("mode".to_string(), json!("builtin"))]),
                    },
                ],
            })
            .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

        assert!(response.skipped.is_empty());
        assert!(response.files_written.contains(&PathBuf::from("GEMINI.md")));
        assert!(
            response
                .files_written
                .contains(&PathBuf::from("packages/api/GEMINI.md"))
        );
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".gemini/skills/shared-analysis/SKILL.md"))
        );
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".gemini/commands/git/commit.toml"))
        );
        assert!(read(root.join("GEMINI.md")).contains("# Root"));
        assert!(read(root.join("packages/api/GEMINI.md")).contains("# API"));
        assert!(
            !root
                .join(".agents/skills/shared-analysis/SKILL.md")
                .exists()
        );
        let command = read(root.join(".gemini/commands/git/commit.toml"));
        assert!(command.contains("description = \"Commit\""));
        assert!(command.contains("mode = \"builtin\""));
        assert!(command.contains("prompt = \"Draft commit for {{args}}.\\n\""));
    }

    #[test]
    fn emits_canonical_toml_command_as_gemini_toml() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();

        GeminiAdapter
            .emit(EmitRequest {
                runtime_dir: root.join(".gemini"),
                mode: RuntimeMode::Managed,
                entities: vec![EmitEntity {
                    id: "command:deploy".to_string(),
                    entity_type: agentmesh_protocol::EntityType::Command,
                    scope: None,
                    source_path: None,
                    files: BTreeMap::from([(
                        PathBuf::from("deploy.toml"),
                        file("description = \"Deploy\"\nprompt = \"Deploy {{args}}\"\n"),
                    )]),
                    frontmatter: BTreeMap::new(),
                    overrides: BTreeMap::from([("mode".to_string(), json!("builtin"))]),
                }],
            })
            .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

        let command = read(root.join(".gemini/commands/deploy.toml"));
        assert!(command.contains("description = \"Deploy\""));
        assert!(command.contains("mode = \"builtin\""));
        assert!(command.contains("prompt = \"Deploy {{args}}\""));
        assert!(!command.contains("prompt = \"prompt ="));
    }

    #[test]
    fn invalid_gemini_files_report_diagnostics() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(
            root.join(".gemini/skills/broken/SKILL.md"),
            "---\nname: broken\ndescription: \"unterminated\n---\nBroken\n",
        );
        write(
            root.join(".agents/skills/broken-shared/SKILL.md"),
            "---\nname: broken-shared\ndescription: \"unterminated\n---\nBroken\n",
        );
        write(
            root.join(".gemini/commands/missing-prompt.toml"),
            "description = \"Missing\"\n",
        );

        let imported = GeminiAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".gemini"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));

        assert!(imported.entities.is_empty());
        for expected in [
            "invalid Gemini skill frontmatter must not be overwritten",
            "invalid shared Gemini skill frontmatter must not be overwritten",
            "missing required prompt field",
        ] {
            assert!(
                imported
                    .skipped
                    .iter()
                    .any(|skipped| skipped.reason.contains(expected)),
                "missing diagnostic {expected}; skipped: {:?}",
                imported.skipped
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_gemini_skill_write_root() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside)
            .unwrap_or_else(|error| panic!("outside dir should be created: {error}"));
        std::fs::create_dir_all(root.join(".gemini"))
            .unwrap_or_else(|error| panic!("gemini dir should be created: {error}"));
        symlink(&outside, root.join(".gemini/skills"))
            .unwrap_or_else(|error| panic!("symlink should be created: {error}"));

        let error = GeminiAdapter
            .emit(EmitRequest {
                runtime_dir: root.join(".gemini"),
                mode: RuntimeMode::Managed,
                entities: vec![EmitEntity {
                    id: "skill:escape".to_string(),
                    entity_type: agentmesh_protocol::EntityType::Skill,
                    scope: None,
                    source_path: None,
                    files: BTreeMap::from([(PathBuf::from("SKILL.md"), file("# Escape\n"))]),
                    frontmatter: BTreeMap::new(),
                    overrides: BTreeMap::new(),
                }],
            })
            .expect_err("symlinked Gemini skill root should be rejected");

        assert!(error.to_string().contains("symlinked Gemini path"));
        assert!(!outside.join("escape/SKILL.md").exists());
    }
}
