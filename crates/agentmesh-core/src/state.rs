//! Machine-local state data structures.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::EntityType;
use crate::types::{Hash, RuntimeName};

/// Binary integrity pin stored in the machine-local cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrityPin {
    /// Absolute path to the trusted binary.
    pub binary_path: PathBuf,
    /// SHA-256 hash of the trusted binary.
    pub binary_sha256: Hash,
    /// Version reported by the binary at pin time.
    pub binary_version: String,
    /// ISO-8601 timestamp when the pin was written.
    pub pinned_at: String,
}

/// Hook ownership records keyed by runtime name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct HookOwnership(pub BTreeMap<RuntimeName, HookOwnershipEntry>);

/// Hook entries installed for a runtime on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookOwnershipEntry {
    /// Runtime overlay file relative to the repository root.
    pub overlay_file: PathBuf,
    /// JSONPath or TOML-path entries installed by AgentMesh.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_paths: Vec<String>,
    /// ISO-8601 timestamp when the entry was installed.
    pub installed_at: String,
    /// Version that installed the entry.
    pub installer_version: String,
}

/// Pending sync event written to the drainer queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSyncRecord {
    /// ULID that orders pending records.
    pub pending_id: String,
    /// Runtime that observed the source change.
    pub source_runtime: RuntimeName,
    /// File action represented by this record.
    pub action: PendingAction,
    /// Entity type, when known at enqueue time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_type: Option<EntityType>,
    /// Entity root relative to the repository root.
    pub entity_root: PathBuf,
    /// Changed paths relative to the repository root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_paths: Vec<PathBuf>,
    /// Previous path for rename actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename_from: Option<PathBuf>,
    /// Content hashes captured at enqueue time.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub content_hashes: BTreeMap<PathBuf, Hash>,
    /// Source mtime as an ISO-8601 timestamp.
    pub mtime: String,
    /// Trigger that created the pending record.
    pub trigger: String,
    /// ISO-8601 timestamp when the record was created.
    pub created_at: String,
}

/// Pending sync action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PendingAction {
    /// File or directory write.
    Write,
    /// File or directory delete.
    Delete,
    /// File or directory rename.
    Rename,
}
