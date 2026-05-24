//! Bundled Codex adapter entry points.

use agentmesh_adapter_sdk_rust::{Adapter, AdapterMetadata, FormatTranslation};
use agentmesh_protocol::EntityType;

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
