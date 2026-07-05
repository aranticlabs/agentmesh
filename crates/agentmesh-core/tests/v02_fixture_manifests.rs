use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use agentmesh_adapter_sdk_rust::Adapter;
use agentmesh_core::identity::derive_entity_id_as;
use agentmesh_core::{EntityId, EntityType};
use agentmesh_protocol::{EmitEntity, EmitRequest, ImportRequest, RuntimeMode};
use serde::Deserialize;

const MINIMUM_SOURCE_CHECK_DATE: &str = "2026-06-18";
const REQUIRED_WRITE_SURFACES: &[&str] = &[
    "copilot.repository-instructions",
    "copilot.path-specific-instructions",
    "copilot.prompt-file",
    "copilot.skill",
    "copilot.shared-skill",
    "copilot.custom-agent",
    "cursor.project-rule",
    "claude.project-rule",
    "claude.project-command",
    "claude.project-hooks",
    "claude.project-mcp",
    "claude.project-permissions",
    "codex.root-instructions",
    "codex.scoped-instructions",
    "codex.skill",
    "codex.shared-skill",
    "codex.subagent",
    "codex.project-hook",
    "codex.project-mcp",
    "codex.permission-policy",
    "gemini.root-context",
    "gemini.nested-context",
    "gemini.skill",
    "gemini.shared-skill",
    "gemini.project-command",
];
const REQUIRED_READ_ONLY_SURFACES: &[&str] = &[
    "codex.inline-config-hooks",
    "codex.experimental-rules",
    "gemini.project-mcp-settings",
    "gemini.policy-settings",
];
const REQUIRED_DEFERRED_SURFACES: &[&str] = &[
    "copilot.hooks",
    "copilot.repository-mcp",
    "copilot.setup-steps",
    "copilot.agent-environment",
    "cursor.skills",
    "cursor.hooks",
    "cursor.commands",
    "cursor.subagents",
    "cursor.mcp",
    "codex.custom-prompts",
    "codex.project-commands",
    "gemini.subagents",
    "gemini.hooks",
    "gemini.extensions",
    "gemini.custom-context-filenames",
];
const FIXTURE_GROUPS: &[&str] = &[
    "adapters/copilot/fixtures/instructions",
    "adapters/copilot/fixtures/prompts",
    "adapters/copilot/fixtures/skills",
    "adapters/copilot/fixtures/agents",
    "adapters/copilot/fixtures/deferred",
    "adapters/cursor/fixtures/rules",
    "adapters/cursor/fixtures/deferred",
    "adapters/claude/fixtures/rules",
    "adapters/claude/fixtures/commands",
    "adapters/claude/fixtures/settings",
    "adapters/claude/fixtures/mcp",
    "adapters/codex/fixtures/instructions",
    "adapters/codex/fixtures/skills",
    "adapters/codex/fixtures/agents",
    "adapters/codex/fixtures/hooks",
    "adapters/codex/fixtures/config",
    "adapters/codex/fixtures/diagnostics",
    "adapters/codex/fixtures/deferred",
    "adapters/gemini/fixtures/context",
    "adapters/gemini/fixtures/skills",
    "adapters/gemini/fixtures/commands",
    "adapters/gemini/fixtures/diagnostics",
];
const SUPPORT_LEVELS: &[&str] = &["write_enabled", "read_only", "deferred"];
const CASES: &[&str] = &[
    "minimum_valid",
    "all_supported_metadata",
    "unknown_metadata",
    "invalid_syntax",
    "invalid_syntax_not_applicable",
    "round_trip",
    "sync_check",
    "doctor",
    "read_only_diagnostic",
    "deferred_detection",
];
const REQUIRED_WRITE_CASES: &[&str] = &[
    "minimum_valid",
    "all_supported_metadata",
    "unknown_metadata",
    "round_trip",
    "sync_check",
    "doctor",
];
const ENTITY_TYPES: &[&str] = &[
    "instructions:root",
    "instructions:scoped",
    "rule",
    "prompt",
    "command",
    "hook",
    "mcp_binding",
    "permission_policy",
    "skill",
    "subagent",
];

#[derive(Debug, Deserialize)]
struct Manifest {
    fixture: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    name: String,
    surface: String,
    cases: Vec<String>,
    source_url: String,
    source_checked_on: String,
    support_level: String,
    canonical_entity_type: String,
    fixture_file_path: PathBuf,
    fixture_root_path: Option<PathBuf>,
    expected_canonical_id: String,
    expected_emitted_path: Option<PathBuf>,
    #[serde(default)]
    expected_files: Vec<PathBuf>,
    #[serde(default)]
    expected_diagnostics: Vec<String>,
    #[serde(default)]
    expected_behaviors: Vec<String>,
}

#[test]
fn fixture_manifests_cover_declared_runtime_surfaces() {
    let workspace = workspace_root();
    let mut fixture_count = 0;
    let mut write_surface_cases = surface_case_map(REQUIRED_WRITE_SURFACES);
    let mut read_only_surface_cases = surface_case_map(REQUIRED_READ_ONLY_SURFACES);
    let mut deferred_surface_cases = surface_case_map(REQUIRED_DEFERRED_SURFACES);

    for group in FIXTURE_GROUPS {
        let group_root = workspace.join(group);
        let manifest_path = group_root.join("manifest.toml");
        let manifest = parse_manifest(&manifest_path);
        let mut seen_names = BTreeSet::new();
        assert!(
            !manifest.fixture.is_empty(),
            "{} must contain at least one fixture entry",
            manifest_path.display()
        );

        for fixture in manifest.fixture {
            fixture_count += 1;
            assert!(
                seen_names.insert(fixture.name.clone()),
                "{} duplicates fixture name {}",
                manifest_path.display(),
                fixture.name
            );
            validate_fixture(&group_root, &manifest_path, &fixture);
            record_surface_cases(
                &mut write_surface_cases,
                &mut read_only_surface_cases,
                &mut deferred_surface_cases,
                &manifest_path,
                &fixture,
            );
        }
    }

    assert_write_surface_coverage(write_surface_cases);
    assert_surface_case(read_only_surface_cases, "read_only_diagnostic", "read-only");
    assert_surface_case(deferred_surface_cases, "deferred_detection", "deferred");

    assert!(
        fixture_count >= FIXTURE_GROUPS.len(),
        "expected fixture entries for all declared groups, found {fixture_count} entries"
    );
}

fn parse_manifest(path: &Path) -> Manifest {
    let content = fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("manifest should be readable at {}: {error}", path.display())
    });
    toml::from_str(&content)
        .unwrap_or_else(|error| panic!("manifest should parse at {}: {error}", path.display()))
}

fn validate_fixture(group_root: &Path, manifest_path: &Path, fixture: &Fixture) {
    assert_field(manifest_path, fixture, "name", &fixture.name);
    assert_field(manifest_path, fixture, "surface", &fixture.surface);
    assert!(
        valid_surface_id(&fixture.surface),
        "{} fixture {} has invalid surface {}",
        manifest_path.display(),
        fixture.name,
        fixture.surface
    );
    assert!(
        !fixture.cases.is_empty(),
        "{} fixture {} must declare at least one case",
        manifest_path.display(),
        fixture.name
    );
    for case in &fixture.cases {
        assert!(
            CASES.contains(&case.as_str()),
            "{} fixture {} has unsupported case {}",
            manifest_path.display(),
            fixture.name,
            case
        );
    }
    assert_field(manifest_path, fixture, "source_url", &fixture.source_url);
    assert!(
        fixture.source_url.starts_with("https://"),
        "{} fixture {} source_url must be HTTPS",
        manifest_path.display(),
        fixture.name
    );
    assert!(
        valid_iso_date(&fixture.source_checked_on)
            && fixture.source_checked_on.as_str() >= MINIMUM_SOURCE_CHECK_DATE,
        "{} fixture {} has stale or invalid source_checked_on {}",
        manifest_path.display(),
        fixture.name,
        fixture.source_checked_on
    );
    assert!(
        SUPPORT_LEVELS.contains(&fixture.support_level.as_str()),
        "{} fixture {} has unsupported support_level {}",
        manifest_path.display(),
        fixture.name,
        fixture.support_level
    );
    assert!(
        ENTITY_TYPES.contains(&fixture.canonical_entity_type.as_str()),
        "{} fixture {} has unsupported canonical_entity_type {}",
        manifest_path.display(),
        fixture.name,
        fixture.canonical_entity_type
    );
    assert!(
        valid_canonical_id(&fixture.expected_canonical_id),
        "{} fixture {} has invalid expected_canonical_id {}",
        manifest_path.display(),
        fixture.name,
        fixture.expected_canonical_id
    );
    assert!(
        EntityId::new(&fixture.expected_canonical_id).is_ok(),
        "{} fixture {} expected_canonical_id {} is not accepted by core",
        manifest_path.display(),
        fixture.name,
        fixture.expected_canonical_id
    );
    validate_path_derived_id(manifest_path, fixture);
    assert_safe_relative(
        manifest_path,
        &fixture.name,
        "fixture_file_path",
        &fixture.fixture_file_path,
    );
    assert!(
        group_root.join(&fixture.fixture_file_path).is_file(),
        "{} fixture {} missing file {}",
        manifest_path.display(),
        fixture.name,
        fixture.fixture_file_path.display()
    );
    let fixture_root = if let Some(root_path) = &fixture.fixture_root_path {
        assert_safe_relative(manifest_path, &fixture.name, "fixture_root_path", root_path);
        let root = group_root.join(root_path);
        assert!(
            root.is_dir(),
            "{} fixture {} missing root {}",
            manifest_path.display(),
            fixture.name,
            root_path.display()
        );
        root
    } else {
        group_root.to_path_buf()
    };
    for expected_file in &fixture.expected_files {
        assert_safe_relative(
            manifest_path,
            &fixture.name,
            "expected_files",
            expected_file,
        );
        assert!(
            fixture_root.join(expected_file).is_file(),
            "{} fixture {} missing expected file {} under {}",
            manifest_path.display(),
            fixture.name,
            expected_file.display(),
            fixture_root.display()
        );
    }

    if fixture.support_level == "write_enabled" {
        if fixture.cases.iter().any(|case| case == "invalid_syntax") {
            assert!(
                fixture.expected_emitted_path.is_none(),
                "{} fixture {} must not emit invalid syntax samples",
                manifest_path.display(),
                fixture.name
            );
            assert!(
                !fixture.expected_diagnostics.is_empty(),
                "{} fixture {} must declare diagnostics for invalid syntax",
                manifest_path.display(),
                fixture.name
            );
        } else if let Some(emitted_path) = &fixture.expected_emitted_path {
            assert_safe_relative(
                manifest_path,
                &fixture.name,
                "expected_emitted_path",
                emitted_path,
            );
        } else {
            assert!(
                !fixture.expected_diagnostics.is_empty(),
                "{} fixture {} must declare diagnostics when emit is blocked",
                manifest_path.display(),
                fixture.name
            );
        }
    } else {
        assert!(
            fixture.expected_emitted_path.is_none(),
            "{} fixture {} must not declare expected_emitted_path",
            manifest_path.display(),
            fixture.name
        );
        assert!(
            !fixture.expected_diagnostics.is_empty(),
            "{} fixture {} must declare diagnostics for non-write-enabled surfaces",
            manifest_path.display(),
            fixture.name
        );
    }
    if fixture
        .cases
        .iter()
        .any(|case| case == "invalid_syntax_not_applicable")
    {
        assert!(
            !fixture.expected_behaviors.is_empty(),
            "{} fixture {} must explain non-applicable syntax validation",
            manifest_path.display(),
            fixture.name
        );
    }
}

fn assert_field(manifest_path: &Path, fixture: &Fixture, field: &str, value: &str) {
    assert!(
        !value.trim().is_empty(),
        "{} fixture {} has empty {field}",
        manifest_path.display(),
        fixture.name
    );
}

fn assert_safe_relative(manifest_path: &Path, fixture_name: &str, field: &str, path: &Path) {
    assert!(
        !path.as_os_str().is_empty() && !path.is_absolute(),
        "{} fixture {fixture_name} has invalid {field} {}",
        manifest_path.display(),
        path.display()
    );
    assert!(
        !path.to_string_lossy().contains('\\'),
        "{} fixture {fixture_name} has platform-specific {field} {}",
        manifest_path.display(),
        path.display()
    );
    assert!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "{} fixture {fixture_name} has unsafe {field} {}",
        manifest_path.display(),
        path.display()
    );
}

fn valid_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn valid_canonical_id(value: &str) -> bool {
    !value.is_empty()
        && value.split(':').all(|part| {
            !part.is_empty()
                && part.split('-').all(|segment| {
                    !segment.is_empty()
                        && segment.chars().all(|character| {
                            character.is_ascii_lowercase() || character.is_ascii_digit()
                        })
                })
        })
}

fn valid_surface_id(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').all(|part| {
            !part.is_empty()
                && part.split('-').all(|segment| {
                    !segment.is_empty()
                        && segment.chars().all(|character| {
                            character.is_ascii_lowercase() || character.is_ascii_digit()
                        })
                })
        })
}

fn validate_path_derived_id(manifest_path: &Path, fixture: &Fixture) {
    if is_adapter_derived_identity(fixture) {
        return;
    }
    let Some(runtime_path) = runtime_relative_fixture_path(&fixture.fixture_file_path) else {
        assert!(
            fixture.support_level == "deferred",
            "{} fixture {} has no runtime-relative path for identity derivation",
            manifest_path.display(),
            fixture.name
        );
        return;
    };
    let Some(entity_type) = production_entity_type(&fixture.canonical_entity_type) else {
        return;
    };

    match derive_entity_id_as(entity_type, &runtime_path) {
        Ok(actual) => assert_eq!(
            actual.as_str(),
            fixture.expected_canonical_id,
            "{} fixture {} expected_canonical_id does not match production derivation from {}",
            manifest_path.display(),
            fixture.name,
            runtime_path.display()
        ),
        Err(error) => assert!(
            fixture.support_level == "deferred",
            "{} fixture {} path {} did not derive a production ID: {error}",
            manifest_path.display(),
            fixture.name,
            runtime_path.display()
        ),
    }
}

fn runtime_relative_fixture_path(path: &Path) -> Option<PathBuf> {
    let parts = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();

    for (index, part) in parts.iter().enumerate() {
        if matches!(
            *part,
            ".mcp.json"
                | ".ai"
                | ".agents"
                | ".claude"
                | ".codex"
                | ".github"
                | ".gemini"
                | ".cursor"
        ) {
            return Some(pathbuf_from_parts(&parts[index..]));
        }
    }

    if parts
        .first()
        .is_some_and(|part| matches!(*part, "environment" | "mcp"))
    {
        return Some(pathbuf_from_parts(&parts));
    }

    for (index, part) in parts.iter().enumerate() {
        if matches!(*part, "AGENTS.md" | "CLAUDE.md" | "GEMINI.md") {
            let start = if index > 0 && matches!(parts[0], "root" | "nested") {
                1
            } else {
                index
            };
            return Some(pathbuf_from_parts(&parts[start..]));
        }
    }

    None
}

fn pathbuf_from_parts(parts: &[&str]) -> PathBuf {
    parts.iter().fold(PathBuf::new(), |mut path, part| {
        path.push(part);
        path
    })
}

fn production_entity_type(value: &str) -> Option<EntityType> {
    match value {
        "instructions:root" | "instructions:scoped" => Some(EntityType::Instructions),
        "rule" => Some(EntityType::Rule),
        "prompt" => Some(EntityType::Prompt),
        "command" => Some(EntityType::Command),
        "hook" => Some(EntityType::Hook),
        "mcp_binding" => Some(EntityType::McpBinding),
        "permission_policy" => Some(EntityType::PermissionPolicy),
        "skill" => Some(EntityType::Skill),
        "subagent" => Some(EntityType::Subagent),
        _ => None,
    }
}

fn is_adapter_derived_identity(fixture: &Fixture) -> bool {
    matches!(
        fixture.surface.as_str(),
        "copilot.hooks"
            | "copilot.repository-mcp"
            | "copilot.setup-steps"
            | "copilot.agent-environment"
            | "cursor.skills"
            | "cursor.hooks"
            | "cursor.commands"
            | "cursor.subagents"
            | "cursor.mcp"
            | "codex.inline-config-hooks"
            | "codex.custom-prompts"
            | "codex.project-commands"
            | "gemini.custom-context-filenames"
            | "gemini.subagents"
            | "gemini.hooks"
            | "gemini.extensions"
    ) || (matches!(
        fixture.surface.as_str(),
        "claude.project-rule" | "cursor.project-rule"
    ) && fixture.canonical_entity_type == "instructions:scoped")
}

fn surface_case_map(surfaces: &[&str]) -> BTreeMap<String, BTreeSet<String>> {
    surfaces
        .iter()
        .map(|surface| ((*surface).to_string(), BTreeSet::new()))
        .collect()
}

fn record_surface_cases(
    write_surfaces: &mut BTreeMap<String, BTreeSet<String>>,
    read_only_surfaces: &mut BTreeMap<String, BTreeSet<String>>,
    deferred_surfaces: &mut BTreeMap<String, BTreeSet<String>>,
    manifest_path: &Path,
    fixture: &Fixture,
) {
    let cases = match fixture.support_level.as_str() {
        "write_enabled" => write_surfaces.get_mut(&fixture.surface),
        "read_only" => read_only_surfaces.get_mut(&fixture.surface),
        "deferred" => deferred_surfaces.get_mut(&fixture.surface),
        _ => None,
    };
    let Some(cases) = cases else {
        panic!(
            "{} fixture {} declares unexpected surface {} for {}",
            manifest_path.display(),
            fixture.name,
            fixture.surface,
            fixture.support_level
        );
    };
    cases.extend(fixture.cases.iter().cloned());
}

fn assert_write_surface_coverage(surfaces: BTreeMap<String, BTreeSet<String>>) {
    for (surface, cases) in surfaces {
        for required_case in REQUIRED_WRITE_CASES {
            assert!(
                cases.contains(*required_case),
                "write-enabled surface {surface} is missing case {required_case}"
            );
        }
        assert!(
            cases.contains("invalid_syntax") || cases.contains("invalid_syntax_not_applicable"),
            "write-enabled surface {surface} is missing invalid syntax coverage"
        );
    }
}

fn assert_surface_case(
    surfaces: BTreeMap<String, BTreeSet<String>>,
    required_case: &str,
    support_label: &str,
) {
    for (surface, cases) in surfaces {
        assert!(
            cases.contains(required_case) && cases.contains("doctor"),
            "{support_label} surface {surface} is missing {required_case} or doctor coverage"
        );
    }
}

#[test]
fn implemented_runtime_fixtures_match_adapter_behavior() {
    let workspace = workspace_root();
    for group in FIXTURE_GROUPS.iter().filter(|group| {
        group.starts_with("adapters/copilot")
            || group.starts_with("adapters/claude")
            || group.starts_with("adapters/codex")
            || group.starts_with("adapters/cursor")
            || group.starts_with("adapters/gemini")
    }) {
        let group_root = workspace.join(group);
        let manifest = parse_manifest(&group_root.join("manifest.toml"));
        for fixture in manifest.fixture {
            validate_phase2_fixture_behavior(&group_root, &fixture);
        }
    }
}

fn validate_phase2_fixture_behavior(group_root: &Path, fixture: &Fixture) {
    let Some(runtime) = fixture.surface.split_once('.').map(|(runtime, _)| runtime) else {
        panic!(
            "fixture {} has invalid surface {}",
            fixture.name, fixture.surface
        );
    };
    let source_workspace = fixture_workspace(group_root, fixture);
    let temp =
        tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir should be available: {error}"));
    let repo = temp.path().join("repo");
    copy_dir_all(&source_workspace, &repo)
        .unwrap_or_else(|error| panic!("fixture workspace should copy: {error}"));
    seed_runtime_presence_marker(runtime, fixture, &repo);

    let imported = import_with_adapter(runtime, &repo)
        .unwrap_or_else(|error| panic!("{} import should run: {error}", fixture.name));
    assert_expected_diagnostics(fixture, &imported.skipped);
    let imported_entity = imported
        .entities
        .iter()
        .find(|entity| entity.id == fixture.expected_canonical_id);

    if fixture.support_level == "write_enabled"
        && !fixture.cases.iter().any(|case| case == "invalid_syntax")
    {
        let entity = imported_entity.unwrap_or_else(|| {
            panic!(
                "fixture {} did not import expected entity {}; imported ids: {:?}; skipped: {:?}",
                fixture.name,
                fixture.expected_canonical_id,
                imported
                    .entities
                    .iter()
                    .map(|entity| entity.id.as_str())
                    .collect::<Vec<_>>(),
                imported.skipped
            )
        });
        assert_eq!(
            entity.entity_type,
            production_entity_type(&fixture.canonical_entity_type)
                .unwrap_or_else(|| panic!("fixture {} has unsupported type", fixture.name))
        );
        let expected_path = fixture.expected_emitted_path.as_ref().unwrap_or_else(|| {
            panic!(
                "write-enabled fixture {} must declare expected_emitted_path",
                fixture.name
            )
        });
        let emit_repo = temp.path().join("emit");
        fs::create_dir_all(&emit_repo)
            .unwrap_or_else(|error| panic!("emit repo should be created: {error}"));
        emit_with_adapter(runtime, &emit_repo, entity.clone())
            .unwrap_or_else(|error| panic!("{} emit should run: {error}", fixture.name));
        assert!(
            emit_repo.join(expected_path).is_file(),
            "fixture {} did not emit expected path {}",
            fixture.name,
            expected_path.display()
        );
    } else {
        assert!(
            imported_entity.is_none(),
            "fixture {} should not import canonical entity {}",
            fixture.name,
            fixture.expected_canonical_id
        );
        assert!(
            !imported.skipped.is_empty(),
            "fixture {} should report skipped diagnostics",
            fixture.name
        );
    }
}

fn seed_runtime_presence_marker(runtime: &str, fixture: &Fixture, repo: &Path) {
    if runtime == "gemini" && fixture.surface == "gemini.shared-skill" {
        fs::write(repo.join("GEMINI.md"), "# Gemini\n")
            .unwrap_or_else(|error| panic!("Gemini marker should be written: {error}"));
    }
}

fn assert_expected_diagnostics(fixture: &Fixture, skipped: &[agentmesh_protocol::SkippedPath]) {
    for expected in &fixture.expected_diagnostics {
        assert!(
            skipped
                .iter()
                .any(|skipped| skipped.reason.contains(expected)),
            "fixture {} did not report expected diagnostic {:?}; skipped: {:?}",
            fixture.name,
            expected,
            skipped
        );
    }
}

fn import_with_adapter(
    runtime: &str,
    repo: &Path,
) -> Result<agentmesh_protocol::ImportResponse, agentmesh_adapter_sdk_rust::AdapterError> {
    match runtime {
        "claude" => agentmesh_adapter_claude::ClaudeAdapter.import(ImportRequest {
            canonical_dir: repo.join(".ai"),
            runtime_dir: repo.join(".claude"),
            filter: None,
        }),
        "codex" => agentmesh_adapter_codex::CodexAdapter.import(ImportRequest {
            canonical_dir: repo.join(".ai"),
            runtime_dir: repo.join(".codex"),
            filter: None,
        }),
        "copilot" => agentmesh_adapter_copilot::CopilotAdapter.import(ImportRequest {
            canonical_dir: repo.join(".ai"),
            runtime_dir: repo.join(".github"),
            filter: None,
        }),
        "cursor" => agentmesh_adapter_cursor::CursorAdapter.import(ImportRequest {
            canonical_dir: repo.join(".ai"),
            runtime_dir: repo.join(".cursor"),
            filter: None,
        }),
        "gemini" => agentmesh_adapter_gemini::GeminiAdapter.import(ImportRequest {
            canonical_dir: repo.join(".ai"),
            runtime_dir: repo.join(".gemini"),
            filter: None,
        }),
        other => panic!("unsupported implemented fixture runtime {other}"),
    }
}

fn emit_with_adapter(
    runtime: &str,
    repo: &Path,
    entity: agentmesh_protocol::ImportedEntity,
) -> Result<agentmesh_protocol::EmitResponse, agentmesh_adapter_sdk_rust::AdapterError> {
    let request = EmitRequest {
        runtime_dir: repo.join(format!(".{runtime}")),
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
    };
    match runtime {
        "claude" => agentmesh_adapter_claude::ClaudeAdapter.emit(request),
        "codex" => agentmesh_adapter_codex::CodexAdapter.emit(request),
        "copilot" => agentmesh_adapter_copilot::CopilotAdapter.emit(EmitRequest {
            runtime_dir: repo.join(".github"),
            ..request
        }),
        "cursor" => agentmesh_adapter_cursor::CursorAdapter.emit(request),
        "gemini" => agentmesh_adapter_gemini::GeminiAdapter.emit(request),
        other => panic!("unsupported implemented fixture runtime {other}"),
    }
}

fn fixture_workspace(group_root: &Path, fixture: &Fixture) -> PathBuf {
    let Some(runtime_path) = runtime_relative_fixture_path(&fixture.fixture_file_path) else {
        assert_eq!(
            fixture.support_level, "deferred",
            "fixture {} should have runtime path",
            fixture.name
        );
        let parent = fixture
            .fixture_file_path
            .parent()
            .unwrap_or_else(|| Path::new(""));
        return group_root.join(parent);
    };
    let runtime_component_count = runtime_path.components().count();
    let mut workspace_relative = fixture.fixture_file_path.clone();
    for _ in 0..runtime_component_count {
        assert!(
            workspace_relative.pop(),
            "fixture {} path {} does not contain runtime path {}",
            fixture.name,
            fixture.fixture_file_path.display(),
            runtime_path.display()
        );
    }
    group_root.join(workspace_relative)
}

fn copy_dir_all(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target_path = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), target_path)?;
        }
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(workspace) = manifest_dir.parent().and_then(Path::parent) else {
        panic!(
            "workspace root should be two directories above {}",
            manifest_dir.display()
        );
    };
    workspace.to_path_buf()
}
