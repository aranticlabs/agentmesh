//! Bundled Claude adapter entry points.

use agentmesh_adapter_sdk_rust::{Adapter, AdapterMetadata, FormatTranslation};
use agentmesh_protocol::EntityType;

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
