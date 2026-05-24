//! User-facing configuration data structures.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::RuntimeName;

/// Current configuration schema version.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Bundled JSON Schema for `agentmesh.config.yaml`.
pub const CONFIG_SCHEMA_JSON: &str = include_str!("schemas/agentmesh.config.schema.json");

/// Project configuration shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentmeshConfig {
    /// Optional configuration schema version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Per-runtime behavior overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtimes: BTreeMap<RuntimeName, RuntimeConfig>,
    /// Synchronization behavior overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncConfig>,
    /// Watcher behavior overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watcher: Option<WatcherConfig>,
    /// Capability fallback behavior.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fallbacks: BTreeMap<RuntimeName, BTreeMap<String, CapabilityFallback>>,
    /// Adapter discovery overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub adapters: BTreeMap<RuntimeName, AdapterConfig>,
    /// CI strict-mode behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci: Option<CiConfig>,
    /// Hook installation preferences.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hooks: BTreeMap<RuntimeName, RuntimeHookConfig>,
}

/// Per-runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RuntimeConfig {
    /// Sync mode for this runtime.
    #[serde(default)]
    pub mode: RuntimeMode,
}

/// Runtime synchronization mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeMode {
    /// Import and emit changes in both directions.
    #[default]
    Bidirectional,
    /// Bidirectional mode with extra preservation of runtime config.
    Merge,
    /// Detect and import without writing to this runtime.
    ReadOnly,
    /// Treat this runtime's AgentMesh-managed files as generated output.
    Managed,
    /// Ignore this runtime.
    Disabled,
}

/// Synchronization configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SyncConfig {
    /// Conflict strategy name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_strategy: Option<ConflictStrategy>,
    /// Similarity threshold used by rename detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename_similarity_threshold: Option<f64>,
    /// VCS throttling window in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcs_throttle_ms: Option<u64>,
    /// Additional ignore globs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
}

/// Conflict handling strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictStrategy {
    /// Automatic structured merge and tiebreaking.
    Auto,
}

/// Watcher configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WatcherConfig {
    /// Idle timeout in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_minutes: Option<u64>,
    /// Watcher log level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<LogLevel>,
    /// Debounce window in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debounce_ms: Option<u64>,
}

/// Watcher log level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogLevel {
    /// Error-only logging.
    Error,
    /// Warning and error logging.
    Warn,
    /// Informational logging.
    Info,
    /// Debug logging.
    Debug,
}

/// Capability fallback behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityFallback {
    /// Omit without reporting.
    Skip,
    /// Omit and report.
    Warn,
    /// Render the entity into a runtime-supported document surface.
    RenderAsDoc,
    /// Treat the mismatch as a hard error.
    Fail,
}

/// Adapter discovery configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AdapterConfig {
    /// Adapter executable path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<PathBuf>,
    /// Accepted adapter version range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_constraint: Option<String>,
    /// Trust mode for this adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<String>,
}

/// CI strict-mode configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CiConfig {
    /// Turn conflict-resolution drift into a hard CI failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_on_conflict: Option<bool>,
    /// Turn capability skips into a hard CI failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_on_capability_skip: Option<bool>,
    /// Fail when the lockfile is not clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_clean_lock: Option<bool>,
}

/// Hook installation configuration for a runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RuntimeHookConfig {
    /// Whether hook installation is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Additional matcher expression fragments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher_extra: Option<String>,
}
