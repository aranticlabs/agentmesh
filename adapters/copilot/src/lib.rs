//! Bundled GitHub Copilot adapter entry points.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentmesh_adapter_sdk_rust::{
    Adapter, AdapterError, AdapterMetadata, FormatTranslation, collect_entity_files,
    compose_frontmatter, dir_entry_file_type, hash_files, is_regular_dir, is_regular_file,
    is_safe_relative, max_mtime_string, mtime_string, parse_frontmatter, read_dir_sorted,
    read_to_string, selected, sha256_bytes, skipped_entity, slug_for_entity, slugify,
    workspace_relative, workspace_root_for, write_atomic,
};
use agentmesh_protocol::{
    AdapterErrorCode, DetectResponse, EmitRequest, EmitResponse, EntityFile, EntityFileEncoding,
    EntityType, ImportFilter, ImportRequest, ImportResponse, ImportedEntity, InstallHooksRequest,
    InstallHooksResponse, RemoveHooksRequest, RemoveHooksResponse, RuntimeMode, SkippedPath,
};
use serde_json::Value as JsonValue;
use serde_norway::{Mapping as YamlMapping, Value as YamlValue};

const SUPPORTED_ENTITIES: &[EntityType] = &[
    EntityType::Instructions,
    EntityType::Prompt,
    EntityType::Skill,
    EntityType::Subagent,
];
const ALLOWED_READ_PATHS: &[&str] = &[
    ".github/copilot-instructions.md",
    ".github/instructions/**",
    ".github/prompts/**",
    ".github/skills/**",
    ".github/agents/**",
    ".agents/skills/**",
    ".github/hooks/**",
    "mcp/repository-mcp-settings.json",
    ".github/workflows/copilot-setup-steps.yml",
    "environment/agent-environment.json",
];
const ALLOWED_WRITE_PATHS: &[&str] = &[
    ".github/copilot-instructions.md",
    ".github/instructions/**",
    ".github/prompts/**",
    ".github/skills/**",
    ".agents/skills/**",
    ".github/agents/**",
];
const MARKDOWN_FORMATS: &[&str] = &["markdown"];
const FORMAT_TRANSLATIONS: &[FormatTranslation] = &[
    FormatTranslation {
        entity_type: EntityType::Instructions,
        formats: MARKDOWN_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::Prompt,
        formats: MARKDOWN_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::Skill,
        formats: MARKDOWN_FORMATS,
    },
    FormatTranslation {
        entity_type: EntityType::Subagent,
        formats: MARKDOWN_FORMATS,
    },
];

/// GitHub Copilot adapter handle.
#[derive(Debug, Clone, Copy, Default)]
pub struct CopilotAdapter;

impl Adapter for CopilotAdapter {
    fn metadata(&self) -> AdapterMetadata {
        metadata()
    }

    fn detect(&self, workspace_root: &Path) -> agentmesh_adapter_sdk_rust::Result<DetectResponse> {
        let evidence = [
            workspace_root.join(".github/copilot-instructions.md"),
            workspace_root.join(".github/instructions"),
            workspace_root.join(".github/prompts"),
            workspace_root.join(".github/skills"),
            workspace_root.join(".agents/skills"),
            workspace_root.join(".github/agents"),
        ];
        let mut files = Vec::new();
        for path in evidence {
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

        import_root_instructions(&workspace_root, filter, &mut entities, &mut skipped)?;
        import_scoped_instructions(
            &workspace_root,
            &workspace_root.join(".github/instructions"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_prompts(
            &workspace_root,
            &workspace_root.join(".github/prompts"),
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_skills(
            &workspace_root,
            &workspace_root.join(".github/skills"),
            false,
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_skills(
            &workspace_root,
            &workspace_root.join(".agents/skills"),
            true,
            filter,
            &mut entities,
            &mut skipped,
        )?;
        import_custom_agents(
            &workspace_root,
            &workspace_root.join(".github/agents"),
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
                    if is_root_instruction(&entity.id, entity.scope.as_deref()) {
                        let target = workspace_root.join(".github/copilot-instructions.md");
                        validate_copilot_write_path(
                            &workspace_root,
                            &target,
                            &[workspace_root.join(".github")],
                            Some("md"),
                            "Copilot root instructions",
                        )?;
                        write_atomic(&target, content.as_bytes())?;
                        files_written.push(workspace_relative(&workspace_root, &target)?);
                        continue;
                    }

                    let mut frontmatter = scoped_instruction_frontmatter_for_emit(&entity);
                    if !frontmatter.contains_key("applyTo") {
                        let Some(scope) = entity.scope.as_deref().filter(|scope| *scope != "root")
                        else {
                            skipped.push(skipped_entity(
                                entity.id,
                                "scoped Copilot instructions require applyTo or scope",
                            ));
                            continue;
                        };
                        frontmatter.insert(
                            "applyTo".to_string(),
                            JsonValue::Array(vec![JsonValue::String(scope.to_string())]),
                        );
                    }
                    if let Err(reason) = apply_to_scope(&frontmatter) {
                        skipped.push(skipped_entity(entity.id, reason));
                        continue;
                    }
                    let rendered = render_markdown_with_frontmatter(
                        &content,
                        &frontmatter,
                        &entity.overrides,
                    )?;
                    let target = workspace_root.join(".github/instructions").join(format!(
                        "{}.instructions.md",
                        scoped_instruction_slug(&entity.id)
                    ));
                    validate_copilot_write_path(
                        &workspace_root,
                        &target,
                        &[workspace_root.join(".github/instructions")],
                        Some("md"),
                        "Copilot path-specific instructions",
                    )?;
                    write_atomic(&target, rendered.as_bytes())?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                EntityType::Prompt => {
                    let Some(content) = first_file_content(&entity.files) else {
                        skipped.push(skipped_entity(entity.id, "prompt entity has no files"));
                        continue;
                    };
                    let slug = slug_for_entity(&entity.id, &entity.frontmatter);
                    let frontmatter =
                        frontmatter_for_emit(&entity, ".github/prompts", &["description"]);
                    let rendered = render_markdown_with_frontmatter(
                        &content,
                        &frontmatter,
                        &entity.overrides,
                    )?;
                    let target = workspace_root
                        .join(".github/prompts")
                        .join(format!("{slug}.prompt.md"));
                    validate_copilot_write_path(
                        &workspace_root,
                        &target,
                        &[workspace_root.join(".github/prompts")],
                        Some("md"),
                        "Copilot prompt",
                    )?;
                    write_atomic(&target, rendered.as_bytes())?;
                    files_written.push(workspace_relative(&workspace_root, &target)?);
                }
                EntityType::Skill => {
                    let slug = slug_for_entity(&entity.id, &entity.frontmatter);
                    let skill_root = copilot_skill_target_root(&workspace_root, &entity, &slug);
                    validate_copilot_write_path(
                        &workspace_root,
                        &skill_root.join("SKILL.md"),
                        &[
                            workspace_root.join(".github/skills"),
                            workspace_root.join(".agents/skills"),
                        ],
                        Some("md"),
                        "Copilot skill",
                    )?;
                    let frontmatter = skill_frontmatter_for_emit(&entity);
                    for (file_path, file) in &entity.files {
                        if !is_safe_relative(file_path) {
                            return Err(AdapterError::rpc(
                                AdapterErrorCode::WorkspaceOutsideBound,
                                format!("unsafe Copilot skill file path {}", file_path.display()),
                            ));
                        }
                        let target = skill_root.join(file_path);
                        validate_copilot_write_path(
                            &workspace_root,
                            &target,
                            &[
                                workspace_root.join(".github/skills"),
                                workspace_root.join(".agents/skills"),
                            ],
                            None,
                            "Copilot skill",
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
                EntityType::Subagent => {
                    let Some(content) = first_file_content(&entity.files) else {
                        skipped.push(skipped_entity(entity.id, "subagent entity has no files"));
                        continue;
                    };
                    let slug = slug_for_entity(&entity.id, &entity.frontmatter);
                    let frontmatter =
                        frontmatter_for_emit(&entity, ".github/agents", &["name", "description"]);
                    let rendered = render_markdown_with_frontmatter(
                        &content,
                        &frontmatter,
                        &entity.overrides,
                    )?;
                    let target = workspace_root
                        .join(".github/agents")
                        .join(format!("{slug}.agent.md"));
                    validate_copilot_write_path(
                        &workspace_root,
                        &target,
                        &[workspace_root.join(".github/agents")],
                        Some("md"),
                        "Copilot custom agent",
                    )?;
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

/// Returns static metadata for the GitHub Copilot adapter.
#[must_use]
pub const fn metadata() -> AdapterMetadata {
    AdapterMetadata {
        name: "copilot",
        runtime_dir: ".github",
        supported_entities: SUPPORTED_ENTITIES,
        allowed_read_paths: ALLOWED_READ_PATHS,
        allowed_write_paths: ALLOWED_WRITE_PATHS,
        format_translations: FORMAT_TRANSLATIONS,
    }
}

fn import_root_instructions(
    workspace_root: &Path,
    filter: Option<&ImportFilter>,
    entities: &mut Vec<ImportedEntity>,
    skipped: &mut Vec<SkippedPath>,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let path = workspace_root.join(".github/copilot-instructions.md");
    let source_relative = PathBuf::from(".github/copilot-instructions.md");
    if !selected(filter, std::slice::from_ref(&source_relative)) {
        return Ok(());
    }
    match is_regular_file(workspace_root, &path) {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(error) => {
            skipped.push(SkippedPath {
                path: source_relative,
                reason: error.to_string(),
            });
            return Ok(());
        }
    }
    let content = match read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            skipped.push(SkippedPath {
                path: source_relative,
                reason: error.to_string(),
            });
            return Ok(());
        }
    };
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
    Ok(())
}

fn import_scoped_instructions(
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
    import_scoped_instructions_inner(workspace_root, root, root, filter, entities, skipped)
}

fn import_scoped_instructions_inner(
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
                reason: "symlinked Copilot instruction path is not supported".to_string(),
            });
            continue;
        }
        if file_type.is_dir() {
            import_scoped_instructions_inner(
                workspace_root,
                root,
                &path,
                filter,
                entities,
                skipped,
            )?;
            continue;
        }
        if !file_type.is_file() || !is_copilot_instruction_file(&path) {
            continue;
        }
        let source_relative = workspace_relative(workspace_root, &path)?;
        if !selected(filter, std::slice::from_ref(&source_relative)) {
            continue;
        }
        let slug = path_slug(root, &path);
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
                    reason: format!(
                        "invalid path-specific instruction frontmatter must not be overwritten: {error}"
                    ),
                });
                continue;
            }
        };
        let scope = match apply_to_scope(&frontmatter) {
            Ok(scope) => scope,
            Err(reason) => {
                skipped.push(SkippedPath {
                    path: source_relative,
                    reason,
                });
                continue;
            }
        };
        let canonical_content = render_canonical_markdown(
            &content,
            &portable_frontmatter(&frontmatter, &["description"]),
        )?;
        entities.push(import_markdown_entity(MarkdownImport {
            path: &path,
            id: format!("instructions:scoped:{slug}"),
            entity_type: EntityType::Instructions,
            scope: Some(scope),
            canonical_path: PathBuf::from("instructions").join(format!("{slug}.md")),
            source_path: source_relative,
            content: canonical_content,
            frontmatter,
        })?);
    }
    Ok(())
}

fn import_prompts(
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
    for entry in read_dir_sorted(root)? {
        let path = entry.path();
        let file_type = dir_entry_file_type(&entry)?;
        if file_type.is_symlink() {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, &path),
                reason: "symlinked Copilot prompt path is not supported".to_string(),
            });
            continue;
        }
        if !file_type.is_file() || !is_copilot_prompt_file(&path) {
            continue;
        }
        let source_relative = workspace_relative(workspace_root, &path)?;
        if !selected(filter, std::slice::from_ref(&source_relative)) {
            continue;
        }
        let slug = prompt_slug(&path);
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
                    reason: format!("invalid prompt frontmatter must not be overwritten: {error}"),
                });
                continue;
            }
        };
        let canonical_content = render_canonical_markdown(
            &content,
            &portable_frontmatter(&frontmatter, &["description"]),
        )?;
        entities.push(import_markdown_entity(MarkdownImport {
            path: &path,
            id: format!("prompt:{slug}"),
            entity_type: EntityType::Prompt,
            scope: None,
            canonical_path: PathBuf::from("prompts").join(format!("{slug}.md")),
            source_path: source_relative,
            content: canonical_content,
            frontmatter,
        })?);
    }
    Ok(())
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
                reason: "symlinked Copilot skill path is not supported".to_string(),
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
                    "invalid shared skill frontmatter must not be overwritten"
                } else {
                    "invalid skill frontmatter must not be overwritten"
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

fn import_custom_agents(
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
    for entry in read_dir_sorted(root)? {
        let path = entry.path();
        let file_type = dir_entry_file_type(&entry)?;
        if file_type.is_symlink() {
            skipped.push(SkippedPath {
                path: relative_or_path(workspace_root, &path),
                reason: "symlinked Copilot custom agent path is not supported".to_string(),
            });
            continue;
        }
        if !file_type.is_file() || !is_copilot_agent_file(&path) {
            continue;
        }
        let source_relative = workspace_relative(workspace_root, &path)?;
        if !selected(filter, std::slice::from_ref(&source_relative)) {
            continue;
        }
        let slug = agent_slug(&path);
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
                    reason: format!(
                        "invalid custom-agent frontmatter must not be overwritten: {error}"
                    ),
                });
                continue;
            }
        };
        let canonical_content = render_canonical_markdown(
            &content,
            &portable_frontmatter(&frontmatter, &["name", "description"]),
        )?;
        entities.push(import_markdown_entity(MarkdownImport {
            path: &path,
            id: format!("subagent:{slug}"),
            entity_type: EntityType::Subagent,
            scope: None,
            canonical_path: PathBuf::from("subagents").join(format!("{slug}.md")),
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
        &workspace_root.join(".github/hooks"),
        filter,
        "Copilot hook surfaces are deferred and must never be emitted",
        skipped,
    )?;
    import_diagnostic_file(
        workspace_root,
        &workspace_root.join("mcp/repository-mcp-settings.json"),
        filter,
        "repository MCP settings are deferred and must never be emitted",
        skipped,
    )?;
    import_diagnostic_file(
        workspace_root,
        &workspace_root.join(".github/workflows/copilot-setup-steps.yml"),
        filter,
        "Copilot setup steps configure execution environment and must never be emitted",
        skipped,
    )?;
    import_diagnostic_file(
        workspace_root,
        &workspace_root.join("environment/agent-environment.json"),
        filter,
        "Copilot agent environment variables and secrets are deferred and must never be emitted",
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
    id: String,
    entity_type: EntityType,
    scope: Option<String>,
    canonical_path: PathBuf,
    source_path: PathBuf,
    content: String,
    frontmatter: BTreeMap<String, JsonValue>,
}

fn import_markdown_entity(
    input: MarkdownImport<'_>,
) -> agentmesh_adapter_sdk_rust::Result<ImportedEntity> {
    let file_key = input
        .canonical_path
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| input.canonical_path.clone());
    Ok(ImportedEntity {
        id: input.id,
        entity_type: input.entity_type,
        scope: input.scope,
        canonical_path: input.canonical_path,
        files: BTreeMap::from([(file_key, EntityFile::utf8(input.content.clone()))]),
        frontmatter: input.frontmatter,
        canonical_sha256: sha256_bytes(input.content.as_bytes()),
        source_path: input.source_path,
        source_mtime: mtime_string(input.path)?,
    })
}

fn scoped_instruction_frontmatter_for_emit(
    entity: &agentmesh_protocol::EmitEntity,
) -> BTreeMap<String, JsonValue> {
    let copilot_origin = entity
        .source_path
        .as_ref()
        .is_some_and(|path| path.starts_with(".github/instructions"));
    if copilot_origin {
        return entity.frontmatter.clone();
    }
    let mut frontmatter = portable_frontmatter(&entity.frontmatter, &["description"]);
    if let Some(value) = entity.frontmatter.get("applyTo") {
        frontmatter.insert("applyTo".to_string(), value.clone());
    }
    frontmatter
}

fn frontmatter_for_emit(
    entity: &agentmesh_protocol::EmitEntity,
    native_root: &str,
    portable_keys: &[&str],
) -> BTreeMap<String, JsonValue> {
    let native_origin = entity
        .source_path
        .as_ref()
        .is_some_and(|path| path.starts_with(native_root));
    if native_origin {
        entity.frontmatter.clone()
    } else {
        portable_frontmatter(&entity.frontmatter, portable_keys)
    }
}

fn skill_frontmatter_for_emit(
    entity: &agentmesh_protocol::EmitEntity,
) -> BTreeMap<String, JsonValue> {
    let native_or_shared_origin = entity.source_path.as_ref().is_some_and(|path| {
        direct_skill_source_root(path, ".github/skills").is_some()
            || direct_skill_source_root(path, ".agents/skills").is_some()
    });
    if native_or_shared_origin {
        entity.frontmatter.clone()
    } else {
        portable_frontmatter(&entity.frontmatter, &["name", "description"])
    }
}

fn copilot_skill_target_root(
    workspace_root: &Path,
    entity: &agentmesh_protocol::EmitEntity,
    slug: &str,
) -> PathBuf {
    entity
        .source_path
        .as_ref()
        .and_then(|path| {
            direct_skill_source_root(path, ".github/skills")
                .or_else(|| direct_skill_source_root(path, ".agents/skills"))
        })
        .map(|path| workspace_root.join(path))
        .unwrap_or_else(|| workspace_root.join(".github/skills").join(slug))
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

fn validate_copilot_write_path(
    workspace_root: &Path,
    target: &Path,
    roots: &[PathBuf],
    required_extension: Option<&str>,
    label: &str,
) -> agentmesh_adapter_sdk_rust::Result<()> {
    let relative = workspace_relative(workspace_root, target)?;
    if !is_safe_relative(&relative) {
        return Err(AdapterError::rpc(
            AdapterErrorCode::WorkspaceOutsideBound,
            format!("unsafe {label} path {}", target.display()),
        ));
    }
    if !roots.iter().any(|root| target.starts_with(root)) {
        return Err(AdapterError::rpc(
            AdapterErrorCode::WorkspaceOutsideBound,
            format!("{} is outside declared {label} roots", target.display()),
        ));
    }
    if let Some(extension) = required_extension {
        if target.extension().and_then(|value| value.to_str()) != Some(extension) {
            return Err(AdapterError::rpc(
                AdapterErrorCode::WorkspaceOutsideBound,
                format!("{label} target must use .{extension}: {}", target.display()),
            ));
        }
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
                format!("unsafe Copilot path component in {}", path.display()),
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
                    "symlinked Copilot path {} is not supported",
                    current.display()
                ),
            ));
        }
        if require_directory && !metadata.is_dir() {
            return Err(AdapterError::rpc(
                AdapterErrorCode::WorkspaceOutsideBound,
                format!(
                    "Copilot path component is not a directory: {}",
                    current.display()
                ),
            ));
        }
    }
    Ok(())
}

fn apply_to_scope(frontmatter: &BTreeMap<String, JsonValue>) -> Result<String, String> {
    match frontmatter.get("applyTo") {
        Some(JsonValue::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
        Some(JsonValue::String(_)) | None => Err("missing applyTo frontmatter".to_string()),
        Some(JsonValue::Array(values)) => {
            let mut scopes = Vec::new();
            for value in values {
                let Some(scope) = value.as_str() else {
                    return Err(
                        "applyTo frontmatter must be a string or list of strings".to_string()
                    );
                };
                if scope.trim().is_empty() {
                    return Err("applyTo frontmatter entries must be non-empty strings".to_string());
                }
                scopes.push(scope);
            }
            match scopes.as_slice() {
                [] => Err("missing applyTo frontmatter".to_string()),
                [scope] => Ok((*scope).to_string()),
                _ => Err(
                    "Copilot applyTo with multiple scopes cannot be represented losslessly"
                        .to_string(),
                ),
            }
        }
        Some(_) => Err("applyTo frontmatter must be a string or list of strings".to_string()),
    }
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

fn entity_file_bytes(
    path: &Path,
    file: &EntityFile,
) -> agentmesh_adapter_sdk_rust::Result<Vec<u8>> {
    file.decode_bytes().map_err(|source| {
        AdapterError::rpc(
            AdapterErrorCode::FormatTranslationFailed,
            format!(
                "failed to decode Copilot entity file {}: {source}",
                path.display()
            ),
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

fn is_root_instruction(id: &str, scope: Option<&str>) -> bool {
    id == "instructions:root" || scope == Some("root")
}

fn scoped_instruction_slug(id: &str) -> String {
    id.strip_prefix("instructions:scoped:")
        .unwrap_or("scoped")
        .to_string()
}

fn is_copilot_instruction_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".instructions.md"))
}

fn is_copilot_prompt_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".prompt.md"))
}

fn is_copilot_agent_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".agent.md") || name.ends_with(".md"))
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
    if let Some(last) = parts.last_mut() {
        let stem = last
            .strip_suffix(".instructions.md")
            .or_else(|| last.strip_suffix(".md"))
            .unwrap_or(last)
            .to_string();
        *last = stem;
    }
    parts
        .into_iter()
        .map(|part| slugify(&part))
        .collect::<Vec<_>>()
        .join("-")
}

fn prompt_slug(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    slugify(name.strip_suffix(".prompt.md").unwrap_or(name))
}

fn agent_slug(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let stem = name
        .strip_suffix(".agent.md")
        .or_else(|| name.strip_suffix(".md"))
        .unwrap_or(name);
    slugify(stem)
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

    use super::CopilotAdapter;

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
    fn imports_copilot_write_enabled_surfaces_and_deferred_diagnostics() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(root.join(".github/copilot-instructions.md"), "# Root\n");
        write(
            root.join(".github/instructions/api.instructions.md"),
            "---\ndescription: API\napplyTo:\n  - crates/api/**\nexcludeAgent: false\n---\n# API\n",
        );
        write(
            root.join(".github/prompts/release-notes.prompt.md"),
            "---\ndescription: Release\nmode: ask\n---\nRelease body.\n",
        );
        write(
            root.join(".github/skills/repo-map/SKILL.md"),
            "---\nname: repo-map\ndescription: Map repo\ncategory: discovery\n---\n# Repo Map\n",
        );
        write(
            root.join(".github/skills/repo-map/references/checklist.md"),
            "# Checklist\n",
        );
        write(
            root.join(".agents/skills/shared-workflow/SKILL.md"),
            "---\nname: shared-workflow\ndescription: Shared\nowner: platform\n---\n# Shared\n",
        );
        write(
            root.join(".github/agents/security-reviewer.agent.md"),
            "---\nname: security-reviewer\ndescription: Security\ntools:\n  - codebase\n---\n# Security\n",
        );
        write(root.join(".github/hooks/pre-tool.json"), "{}\n");

        let imported = CopilotAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".github"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));
        let ids = imported
            .entities
            .iter()
            .map(|entity| entity.id.as_str())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"instructions:root"));
        assert!(ids.contains(&"instructions:scoped:api"));
        assert!(ids.contains(&"prompt:release-notes"));
        assert!(ids.contains(&"skill:repo-map"));
        assert!(ids.contains(&"skill:shared-workflow"));
        assert!(ids.contains(&"subagent:security-reviewer"));
        let scoped = imported
            .entities
            .iter()
            .find(|entity| entity.id == "instructions:scoped:api")
            .unwrap_or_else(|| panic!("scoped instructions should import"));
        assert_eq!(scoped.scope.as_deref(), Some("crates/api/**"));
        assert!(scoped.frontmatter.contains_key("excludeAgent"));
        assert!(
            !scoped.files[Path::new("api.md")]
                .content
                .contains("applyTo")
        );
        let native_skill = imported
            .entities
            .iter()
            .find(|entity| entity.id == "skill:repo-map")
            .unwrap_or_else(|| panic!("native skill should import"));
        assert!(native_skill.frontmatter.contains_key("category"));
        assert!(
            !native_skill.files[Path::new("SKILL.md")]
                .content
                .contains("category")
        );
        assert!(imported.skipped.iter().any(|skipped| {
            skipped.path == Path::new(".github/hooks/pre-tool.json")
                && skipped
                    .reason
                    .contains("Copilot hook surfaces are deferred")
        }));
    }

    #[test]
    fn detects_only_write_enabled_copilot_surfaces() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(root.join(".github/hooks/pre-tool.json"), "{}\n");

        let deferred_only = CopilotAdapter
            .detect(root)
            .unwrap_or_else(|error| panic!("detect should succeed: {error}"));
        assert!(!deferred_only.present);

        write(
            root.join(".github/prompts/release.prompt.md"),
            "# Release\n",
        );
        let with_prompt = CopilotAdapter
            .detect(root)
            .unwrap_or_else(|error| panic!("detect should succeed: {error}"));
        assert!(with_prompt.present);
    }

    #[test]
    fn emits_copilot_surfaces_and_normalizes_legacy_agents() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();

        let response = CopilotAdapter
            .emit(EmitRequest {
                runtime_dir: root.join(".github"),
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
                        id: "instructions:scoped:api".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Instructions,
                        scope: Some("crates/api/**".to_string()),
                        source_path: None,
                        files: BTreeMap::from([(PathBuf::from("api.md"), file("# API\n"))]),
                        frontmatter: BTreeMap::from([("description".to_string(), json!("API"))]),
                        overrides: BTreeMap::from([("excludeAgent".to_string(), json!(false))]),
                    },
                    EmitEntity {
                        id: "prompt:release-notes".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Prompt,
                        scope: None,
                        source_path: None,
                        files: BTreeMap::from([(
                            PathBuf::from("release-notes.md"),
                            file("Release body.\n"),
                        )]),
                        frontmatter: BTreeMap::from([(
                            "description".to_string(),
                            json!("Release"),
                        )]),
                        overrides: BTreeMap::from([("mode".to_string(), json!("ask"))]),
                    },
                    EmitEntity {
                        id: "skill:repo-map".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Skill,
                        scope: None,
                        source_path: Some(PathBuf::from(".agents/skills/repo-map/SKILL.md")),
                        files: BTreeMap::from([
                            (PathBuf::from("SKILL.md"), file("# Repo Map\n")),
                            (
                                PathBuf::from("references/checklist.md"),
                                file("# Checklist\n"),
                            ),
                        ]),
                        frontmatter: BTreeMap::from([
                            ("name".to_string(), json!("repo-map")),
                            ("owner".to_string(), json!("platform")),
                        ]),
                        overrides: BTreeMap::new(),
                    },
                    EmitEntity {
                        id: "subagent:performance-reviewer".to_string(),
                        entity_type: agentmesh_protocol::EntityType::Subagent,
                        scope: None,
                        source_path: Some(PathBuf::from(".github/agents/performance-reviewer.md")),
                        files: BTreeMap::from([(
                            PathBuf::from("performance-reviewer.md"),
                            file("# Performance\n"),
                        )]),
                        frontmatter: BTreeMap::from([
                            ("name".to_string(), json!("performance-reviewer")),
                            ("unknownField".to_string(), json!("preserve-me")),
                        ]),
                        overrides: BTreeMap::new(),
                    },
                ],
            })
            .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

        assert!(response.skipped.is_empty());
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".github/copilot-instructions.md"))
        );
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".github/instructions/api.instructions.md"))
        );
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".github/prompts/release-notes.prompt.md"))
        );
        assert!(
            response
                .files_written
                .contains(&PathBuf::from(".agents/skills/repo-map/SKILL.md"))
        );
        assert!(response.files_written.contains(&PathBuf::from(
            ".github/agents/performance-reviewer.agent.md"
        )));
        assert!(read(root.join(".github/instructions/api.instructions.md")).contains("applyTo:"));
        assert!(read(root.join(".github/prompts/release-notes.prompt.md")).contains("mode: ask"));
        assert!(
            read(root.join(".github/agents/performance-reviewer.agent.md"))
                .contains("unknownField: preserve-me")
        );
        assert!(read(root.join(".agents/skills/repo-map/SKILL.md")).contains("owner: platform"));
    }

    #[test]
    fn invalid_write_enabled_files_report_diagnostics() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        write(
            root.join(".github/instructions/missing.instructions.md"),
            "---\ndescription: Missing\n---\nBody\n",
        );
        write(
            root.join(".github/instructions/mixed.instructions.md"),
            "---\napplyTo:\n  - crates/api/**\n  - 42\n---\nBody\n",
        );
        write(
            root.join(".github/prompts/broken.prompt.md"),
            "---\ndescription: \"unterminated\n---\nBroken\n",
        );
        write(
            root.join(".github/skills/broken/SKILL.md"),
            "---\nname: broken\ndescription: \"unterminated\n---\nBroken\n",
        );
        write(
            root.join(".github/agents/broken.agent.md"),
            "---\nname: broken\ndescription: \"unterminated\n---\nBroken\n",
        );

        let imported = CopilotAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".github"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));

        assert!(imported.entities.is_empty());
        for expected in [
            "missing applyTo frontmatter",
            "applyTo frontmatter must be a string or list of strings",
            "invalid prompt frontmatter must not be overwritten",
            "invalid skill frontmatter must not be overwritten",
            "invalid custom-agent frontmatter must not be overwritten",
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

        write(
            root.join(".github/instructions/multi.instructions.md"),
            "---\napplyTo:\n  - crates/api/**\n  - adapters/api/**\n---\nBody\n",
        );
        let imported = CopilotAdapter
            .import(ImportRequest {
                canonical_dir: root.join(".ai"),
                runtime_dir: root.join(".github"),
                filter: None,
            })
            .unwrap_or_else(|error| panic!("import should succeed: {error}"));
        assert!(imported.skipped.iter().any(|skipped| {
            skipped
                .reason
                .contains("Copilot applyTo with multiple scopes cannot be represented losslessly")
        }));
    }

    #[test]
    fn scoped_emit_rejects_multi_scope_apply_to_before_writing() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();

        let response = CopilotAdapter
            .emit(EmitRequest {
                runtime_dir: root.join(".github"),
                mode: RuntimeMode::Managed,
                entities: vec![EmitEntity {
                    id: "instructions:scoped:multi".to_string(),
                    entity_type: agentmesh_protocol::EntityType::Instructions,
                    scope: Some("crates/api/**".to_string()),
                    source_path: None,
                    files: BTreeMap::from([(PathBuf::from("multi.md"), file("# Multi\n"))]),
                    frontmatter: BTreeMap::from([(
                        "applyTo".to_string(),
                        json!(["crates/api/**", "adapters/api/**"]),
                    )]),
                    overrides: BTreeMap::new(),
                }],
            })
            .unwrap_or_else(|error| panic!("emit should succeed with skipped entity: {error}"));

        assert!(response.files_written.is_empty());
        assert!(response.skipped.iter().any(|skipped| {
            skipped.entity_id == "instructions:scoped:multi"
                && skipped.reason.contains(
                    "Copilot applyTo with multiple scopes cannot be represented losslessly",
                )
        }));
        assert!(
            !root
                .join(".github/instructions/multi.instructions.md")
                .exists()
        );
    }

    #[test]
    fn nested_skill_source_path_does_not_preserve_unscanned_root() {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();

        let response = CopilotAdapter
            .emit(EmitRequest {
                runtime_dir: root.join(".github"),
                mode: RuntimeMode::Managed,
                entities: vec![EmitEntity {
                    id: "skill:archived".to_string(),
                    entity_type: agentmesh_protocol::EntityType::Skill,
                    scope: None,
                    source_path: Some(PathBuf::from(".agents/skills/archived/archive/SKILL.md")),
                    files: BTreeMap::from([(PathBuf::from("SKILL.md"), file("# Archived\n"))]),
                    frontmatter: BTreeMap::from([
                        ("name".to_string(), json!("archived")),
                        ("owner".to_string(), json!("platform")),
                    ]),
                    overrides: BTreeMap::new(),
                }],
            })
            .unwrap_or_else(|error| panic!("emit should succeed: {error}"));

        assert!(response.skipped.is_empty());
        assert_eq!(
            response.files_written,
            vec![PathBuf::from(".github/skills/archived/SKILL.md")]
        );
        assert!(root.join(".github/skills/archived/SKILL.md").exists());
        assert!(
            !root
                .join(".agents/skills/archived/archive/SKILL.md")
                .exists()
        );
        assert!(!read(root.join(".github/skills/archived/SKILL.md")).contains("owner"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_copilot_skill_write_root() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir()
            .unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
        let root = temp.path();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside)
            .unwrap_or_else(|error| panic!("outside dir should be created: {error}"));
        std::fs::create_dir_all(root.join(".github"))
            .unwrap_or_else(|error| panic!("github dir should be created: {error}"));
        symlink(&outside, root.join(".github/skills"))
            .unwrap_or_else(|error| panic!("symlink should be created: {error}"));

        let error = CopilotAdapter
            .emit(EmitRequest {
                runtime_dir: root.join(".github"),
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
            .expect_err("symlinked Copilot skill root should be rejected");

        assert!(error.to_string().contains("symlinked Copilot path"));
        assert!(!outside.join("escape/SKILL.md").exists());
    }
}
