//! Shared Rust adapter interfaces for bundled AgentMesh adapters.

use agentmesh_protocol::EntityType;
use thiserror::Error;

/// Static format-translation metadata for one entity type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatTranslation {
    /// Entity type covered by this translation declaration.
    pub entity_type: EntityType,
    /// Native formats this adapter can read or write for the entity type.
    pub formats: &'static [&'static str],
}

/// Static metadata exposed by an adapter implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterMetadata {
    /// Canonical adapter name.
    pub name: &'static str,
    /// Runtime dotfolder relative to the workspace root.
    pub runtime_dir: &'static str,
    /// Entity types supported by the adapter.
    pub supported_entities: &'static [EntityType],
    /// Read path globs relative to the workspace root.
    pub allowed_read_paths: &'static [&'static str],
    /// Write path globs relative to the workspace root.
    pub allowed_write_paths: &'static [&'static str],
    /// Format translations declared by the adapter.
    pub format_translations: &'static [FormatTranslation],
}

/// Common trait implemented by bundled adapters.
pub trait Adapter: Send + Sync {
    /// Returns static metadata for this adapter.
    fn metadata(&self) -> AdapterMetadata;
}

/// Adapter SDK result type.
pub type Result<T> = std::result::Result<T, AdapterError>;

/// Errors produced by adapter SDK helpers.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// The stdio serving loop has not been wired into this build.
    #[error("adapter stdio serving is not available in the scaffold build")]
    StdioUnavailable,
}

/// Runs an adapter over the stdio protocol.
pub fn run_adapter<A: Adapter>(adapter: A) -> Result<()> {
    let _ = adapter;
    Err(AdapterError::StdioUnavailable)
}
