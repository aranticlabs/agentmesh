//! Wire-level shared types for the AgentMesh adapter protocol.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Adapter protocol version supported by this workspace.
pub const PROTOCOL_VERSION: u32 = 1;

/// Canonical entity categories exchanged across adapter boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntityType {
    /// Project-wide instructions.
    Instructions,
    /// A named skill with optional supporting files.
    Skill,
    /// A delegated task agent.
    Subagent,
}

impl EntityType {
    /// Returns the stable protocol spelling for the entity type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Instructions => "instructions",
            Self::Skill => "skill",
            Self::Subagent => "subagent",
        }
    }
}

impl fmt::Display for EntityType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A protocol version value exchanged during adapter initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

impl ProtocolVersion {
    /// Returns the current protocol version.
    #[must_use]
    pub const fn current() -> Self {
        Self(PROTOCOL_VERSION)
    }
}
