//! Persisted repository lockfile data structures.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::EntityType;
use crate::types::{EntityId, Hash, LocationKey, RuntimeName};

/// Current lockfile content version.
pub const LOCKFILE_CONTENT_VERSION: u32 = 1;

/// Current lockfile schema version.
pub const LOCKFILE_SCHEMA_VERSION: u32 = 1;

/// Highest lockfile schema version this build can read.
pub const MAX_SUPPORTED_LOCKFILE_SCHEMA: u32 = LOCKFILE_SCHEMA_VERSION;

/// AgentMesh repository lockfile shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lockfile {
    /// Lockfile content version.
    pub version: u32,
    /// Lockfile schema version.
    pub schema: u32,
    /// Entity registry keyed by entity ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entities: BTreeMap<EntityId, LockfileEntity>,
    /// Runtime-specific rendering overrides keyed by entity ID and runtime name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<EntityId, BTreeMap<RuntimeName, OverrideEntry>>,
    /// Active adapter declarations keyed by runtime name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub adapters: BTreeMap<RuntimeName, AdapterDeclaration>,
}

impl Lockfile {
    /// Creates an empty lockfile at the current schema.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            version: LOCKFILE_CONTENT_VERSION,
            schema: LOCKFILE_SCHEMA_VERSION,
            entities: BTreeMap::new(),
            overrides: BTreeMap::new(),
            adapters: BTreeMap::new(),
        }
    }
}

impl Default for Lockfile {
    fn default() -> Self {
        Self::empty()
    }
}

/// A single entity entry in the repository lockfile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockfileEntity {
    /// Canonical entity type.
    #[serde(rename = "type")]
    pub entity_type: EntityType,
    /// Scope for instruction entities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Runtime and canonical locations for this entity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub locations: BTreeMap<LocationKey, PathBuf>,
    /// Hash of the canonical entity representation.
    pub canonical_sha256: Hash,
    /// Hashes of the bytes last emitted to each location.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub emitted_native_sha256: BTreeMap<LocationKey, Hash>,
    /// Bounded import and sync lineage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lineage: Vec<LineageEntry>,
    /// Whether this entity has an unacknowledged conflict resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_conflict_resolution: Option<bool>,
    /// Bounded rename history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rename_history: Vec<RenameRecord>,
    /// Optional explicit identity marker discovered in user content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_pin: Option<EntityId>,
}

/// Lineage entry describing how an entity entered the canonical model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageEntry {
    /// Source path that supplied the entity.
    pub imported_from: PathBuf,
    /// ISO-8601 timestamp for the import.
    pub at: String,
    /// Actor that performed the import.
    pub by: String,
}

/// Rename record stored for identity continuity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameRecord {
    /// Previous path.
    pub from: PathBuf,
    /// Current path.
    pub to: PathBuf,
    /// ISO-8601 timestamp for the rename.
    pub at: String,
}

/// Runtime-specific override values applied during emit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct OverrideEntry(pub BTreeMap<String, Value>);

/// Adapter declaration persisted in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterDeclaration {
    /// Adapter loading mode.
    pub mode: AdapterMode,
    /// Negotiated adapter protocol version.
    pub protocol_version: u32,
    /// Entity types the adapter supports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<EntityType>,
    /// Hook kinds installed by this adapter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookKind>,
}

/// Adapter loading mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterMode {
    /// First-party adapter linked into the single AgentMesh binary.
    Bundled,
}

/// Runtime hook category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HookKind {
    /// Hook that runs after a runtime tool use.
    PostToolUse,
}
