//! User-facing configuration data structures.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_norway::Value;
use thiserror::Error;

use crate::types::RuntimeName;

const CONFIG_FILE_NAME: &str = "agentmesh.config.yaml";

/// Configuration result type.
pub type Result<T> = std::result::Result<T, ConfigError>;

/// Errors produced while reading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading configuration failed.
    #[error("failed to read config at {}", path.display())]
    Read {
        /// Config path.
        path: PathBuf,
        /// Source IO error.
        #[source]
        source: std::io::Error,
    },
    /// YAML parsing failed.
    #[error("failed to parse config at {}", path.display())]
    Parse {
        /// Config path.
        path: PathBuf,
        /// Source parse error.
        #[source]
        source: serde_norway::Error,
    },
    /// A known field has an invalid value.
    #[error("invalid config value: {message}")]
    InvalidValue {
        /// Human-readable validation failure.
        message: String,
    },
    /// JSON Schema validation failed.
    #[error("config schema validation failed at {}: {message}", path.display())]
    Schema {
        /// Config path.
        path: PathBuf,
        /// Human-readable validation failure.
        message: String,
    },
}

/// Parsed project configuration with non-fatal compatibility warnings.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub struct ConfigLoad {
    /// Parsed configuration.
    pub config: AgentmeshConfig,
    /// Unknown-key warnings.
    pub warnings: Vec<ConfigWarning>,
}

/// Non-fatal configuration warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    /// Dotted configuration path.
    pub path: String,
    /// Warning text.
    pub message: String,
}

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

impl AgentmeshConfig {
    /// Validates cross-field constraints not handled by serde enum parsing.
    pub fn validate(&self) -> Result<()> {
        if self.version == Some(0) {
            return Err(ConfigError::InvalidValue {
                message: "version must be at least 1".to_string(),
            });
        }

        if let Some(sync) = &self.sync {
            if let Some(threshold) = sync.rename_similarity_threshold {
                if !(0.0..=1.0).contains(&threshold) {
                    return Err(ConfigError::InvalidValue {
                        message: "sync.rename_similarity_threshold must be between 0.0 and 1.0"
                            .to_string(),
                    });
                }
            }
        }

        Ok(())
    }
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

/// Loads configuration from a repository root.
pub fn load_config(repo_root: &Path) -> Result<ConfigLoad> {
    let path = repo_root.join(CONFIG_FILE_NAME);
    match fs::read_to_string(&path) {
        Ok(contents) => parse_config_at(&contents, &path),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(ConfigLoad {
            config: AgentmeshConfig::default(),
            warnings: Vec::new(),
        }),
        Err(source) => Err(ConfigError::Read { path, source }),
    }
}

/// Parses configuration from YAML text.
pub fn parse_config(contents: &str) -> Result<ConfigLoad> {
    parse_config_at(contents, Path::new(CONFIG_FILE_NAME))
}

fn parse_config_at(contents: &str, path: &Path) -> Result<ConfigLoad> {
    let raw = serde_norway::from_str::<Value>(contents).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    validate_json_schema(&raw, path)?;
    let warnings = collect_unknown_key_warnings(&raw);
    let config = serde_norway::from_str::<AgentmeshConfig>(contents).map_err(|source| {
        ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        }
    })?;
    config.validate()?;

    Ok(ConfigLoad { config, warnings })
}

fn validate_json_schema(raw: &Value, path: &Path) -> Result<()> {
    let schema = serde_json::from_str::<JsonValue>(CONFIG_SCHEMA_JSON).map_err(|source| {
        ConfigError::Schema {
            path: path.to_path_buf(),
            message: source.to_string(),
        }
    })?;
    let instance = serde_json::to_value(raw).map_err(|source| ConfigError::Schema {
        path: path.to_path_buf(),
        message: source.to_string(),
    })?;
    let validator = jsonschema::validator_for(&schema).map_err(|source| ConfigError::Schema {
        path: path.to_path_buf(),
        message: source.to_string(),
    })?;

    validator
        .validate(&instance)
        .map_err(|source| ConfigError::Schema {
            path: path.to_path_buf(),
            message: source.to_string(),
        })
}

fn collect_unknown_key_warnings(raw: &Value) -> Vec<ConfigWarning> {
    let mut warnings = Vec::new();
    collect_object_unknowns(
        raw,
        "",
        &[
            "version",
            "runtimes",
            "sync",
            "watcher",
            "fallbacks",
            "adapters",
            "ci",
            "hooks",
        ],
        &mut warnings,
    );

    collect_runtime_maps(raw, "runtimes", &["mode"], &mut warnings);
    collect_object_at(
        raw,
        "sync",
        &[
            "conflict_strategy",
            "rename_similarity_threshold",
            "vcs_throttle_ms",
            "ignore",
        ],
        &mut warnings,
    );
    collect_object_at(
        raw,
        "watcher",
        &["idle_timeout_minutes", "log_level", "debounce_ms"],
        &mut warnings,
    );
    collect_object_at(
        raw,
        "ci",
        &[
            "fail_on_conflict",
            "fail_on_capability_skip",
            "require_clean_lock",
        ],
        &mut warnings,
    );
    collect_runtime_maps(raw, "hooks", &["enabled", "matcher_extra"], &mut warnings);

    warnings
}

fn collect_object_at(raw: &Value, key: &str, allowed: &[&str], warnings: &mut Vec<ConfigWarning>) {
    if let Some(value) = get_mapping_value(raw, key) {
        collect_object_unknowns(value, key, allowed, warnings);
    }
}

fn collect_runtime_maps(
    raw: &Value,
    section: &str,
    allowed: &[&str],
    warnings: &mut Vec<ConfigWarning>,
) {
    let Some(Value::Mapping(mapping)) = get_mapping_value(raw, section) else {
        return;
    };

    for (runtime, value) in mapping.iter() {
        let Some(runtime) = runtime.as_str() else {
            continue;
        };
        collect_object_unknowns(value, &format!("{section}.{runtime}"), allowed, warnings);
    }
}

fn collect_object_unknowns(
    raw: &Value,
    prefix: &str,
    allowed: &[&str],
    warnings: &mut Vec<ConfigWarning>,
) {
    let Value::Mapping(mapping) = raw else {
        return;
    };

    for (key, _) in mapping.iter() {
        let Some(key) = key.as_str() else {
            continue;
        };
        if !allowed.contains(&key) {
            let path = if prefix.is_empty() {
                key.to_string()
            } else {
                format!("{prefix}.{key}")
            };
            warnings.push(ConfigWarning {
                path,
                message: "unknown configuration key; sync will continue".to_string(),
            });
        }
    }
}

fn get_mapping_value<'a>(raw: &'a Value, key: &str) -> Option<&'a Value> {
    let Value::Mapping(mapping) = raw else {
        return None;
    };
    mapping.get(Value::String(key.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{AgentmeshConfig, CapabilityFallback, RuntimeMode, load_config, parse_config};
    use crate::types::RuntimeName;

    #[test]
    fn missing_config_loads_defaults() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };

        let loaded = match load_config(temp.path()) {
            Ok(loaded) => loaded,
            Err(error) => panic!("missing config should load defaults: {error}"),
        };

        assert_eq!(loaded.config, AgentmeshConfig::default());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn parses_valid_config() {
        let loaded = match parse_config(
            r#"
version: 1
runtimes:
  claude:
    mode: read-only
sync:
  conflict_strategy: auto
  rename_similarity_threshold: 0.8
"#,
        ) {
            Ok(loaded) => loaded,
            Err(error) => panic!("valid config should parse: {error}"),
        };
        let claude = match RuntimeName::new("claude") {
            Ok(runtime) => runtime,
            Err(error) => panic!("runtime name should be valid: {error}"),
        };

        assert_eq!(loaded.config.runtimes[&claude].mode, RuntimeMode::ReadOnly);
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn warns_on_unknown_keys() {
        let loaded = match parse_config(
            r#"
runtimez: {}
runtimes:
  codex:
    mode: bidirectional
    extra: true
"#,
        ) {
            Ok(loaded) => loaded,
            Err(error) => panic!("unknown keys should be warnings: {error}"),
        };

        let paths = loaded
            .warnings
            .iter()
            .map(|warning| warning.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["runtimez", "runtimes.codex.extra"]);
    }

    #[test]
    fn rejects_invalid_known_values() {
        let error = parse_config(
            r#"
runtimes:
  claude:
    mode: bidirextional
"#,
        )
        .err();

        assert!(error.is_some());
    }

    #[test]
    fn rejects_out_of_range_threshold() {
        let error = parse_config(
            r#"
sync:
  rename_similarity_threshold: 2.0
"#,
        )
        .err();

        assert!(error.is_some());
    }

    #[test]
    fn rejects_schema_type_mismatches() {
        let error = parse_config(
            r#"
sync:
  ignore: false
"#,
        )
        .err();

        assert!(matches!(error, Some(super::ConfigError::Schema { .. })));
    }

    #[test]
    fn parses_ci_strict_mode_and_fallback_settings() {
        let loaded = match parse_config(
            r#"
ci:
  fail_on_conflict: true
  fail_on_capability_skip: true
  require_clean_lock: true
fallbacks:
  codex:
    image: warn
"#,
        ) {
            Ok(loaded) => loaded,
            Err(error) => panic!("strict CI config should parse: {error}"),
        };
        let codex = match RuntimeName::new("codex") {
            Ok(runtime) => runtime,
            Err(error) => panic!("runtime name should be valid: {error}"),
        };
        let Some(ci) = loaded.config.ci else {
            panic!("CI config should be present");
        };

        assert_eq!(ci.fail_on_conflict, Some(true));
        assert_eq!(ci.fail_on_capability_skip, Some(true));
        assert_eq!(ci.require_clean_lock, Some(true));
        assert_eq!(
            loaded.config.fallbacks[&codex]["image"],
            CapabilityFallback::Warn
        );
    }
}
