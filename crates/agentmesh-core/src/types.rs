//! Strongly typed identifiers shared by core state structures.

use std::fmt;
use std::str::FromStr;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::EntityType;

/// Result type for validated identifier constructors.
pub type Result<T> = std::result::Result<T, TypeError>;

/// Validation errors for strongly typed persisted identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TypeError {
    /// Entity identifiers must match one of the supported canonical forms.
    #[error("invalid entity id `{value}`")]
    InvalidEntityId { value: String },
    /// Location keys must be dot-prefixed stable location names.
    #[error("invalid location key `{value}`")]
    InvalidLocationKey { value: String },
    /// Runtime names must be stable lowercase names.
    #[error("invalid runtime name `{value}`")]
    InvalidRuntimeName { value: String },
    /// Hashes must be SHA-256 hex strings.
    #[error("invalid sha256 hash `{value}`")]
    InvalidHash { value: String },
}

/// Stable identifier for a canonical entity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(String);

impl EntityId {
    /// Creates an entity identifier from its persisted spelling.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if is_valid_entity_id(&value) {
            Ok(Self(value))
        } else {
            Err(TypeError::InvalidEntityId { value })
        }
    }

    /// Creates an entity identifier from a type and slug.
    pub fn from_parts(entity_type: EntityType, slug: &str) -> Result<Self> {
        match entity_type {
            EntityType::Instructions if slug == "root" => Self::new("instructions:root"),
            EntityType::Instructions => Self::new(format!("instructions:{slug}")),
            EntityType::Skill => Self::new(format!("skill:{slug}")),
            EntityType::Subagent => Self::new(format!("subagent:{slug}")),
        }
    }

    /// Returns the persisted spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for EntityId {
    type Err = TypeError;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl Serialize for EntityId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EntityId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Runtime or canonical location key used in the lockfile.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocationKey(String);

impl LocationKey {
    /// Creates a location key from its persisted spelling.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        match value.strip_prefix('.') {
            Some(name) if is_stable_name(name) => Ok(Self(value)),
            _ => Err(TypeError::InvalidLocationKey { value }),
        }
    }

    /// Returns the persisted spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LocationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LocationKey {
    type Err = TypeError;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl Serialize for LocationKey {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LocationKey {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Runtime name used in config, lockfile, and machine-local records.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuntimeName(String);

impl RuntimeName {
    /// Creates a runtime name from its persisted spelling.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if is_stable_name(&value) {
            Ok(Self(value))
        } else {
            Err(TypeError::InvalidRuntimeName { value })
        }
    }

    /// Returns the persisted spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RuntimeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RuntimeName {
    type Err = TypeError;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl Serialize for RuntimeName {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RuntimeName {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// Hex-encoded content or binary hash.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash(String);

impl Hash {
    /// Creates a hash value from its persisted hex spelling.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.len() == 64 && value.chars().all(|character| character.is_ascii_hexdigit()) {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(TypeError::InvalidHash { value })
        }
    }

    /// Returns the persisted hex spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Hash {
    type Err = TypeError;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl Serialize for Hash {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

fn is_valid_entity_id(value: &str) -> bool {
    if value == "instructions:root" {
        return true;
    }

    let Some((kind, slug)) = value.split_once(':') else {
        return false;
    };

    matches!(kind, "skill" | "subagent") && is_slug(slug)
}

fn is_stable_name(value: &str) -> bool {
    is_slug(value)
}

fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::{EntityId, Hash, LocationKey, RuntimeName};

    const VALID_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn validates_entity_ids() {
        assert!(EntityId::new("instructions:root").is_ok());
        assert!(EntityId::new("skill:security-review-2").is_ok());
        assert!(EntityId::new("subagent:code-reviewer").is_ok());

        assert!(EntityId::new("instructions:foo").is_err());
        assert!(EntityId::new("skill:Security").is_err());
        assert!(EntityId::new("skill:").is_err());
    }

    #[test]
    fn validates_location_and_runtime_names() {
        assert!(LocationKey::new(".ai").is_ok());
        assert!(LocationKey::new(".claude").is_ok());
        assert!(RuntimeName::new("codex").is_ok());

        assert!(LocationKey::new("claude").is_err());
        assert!(LocationKey::new(".Claude").is_err());
        assert!(RuntimeName::new("codex_hook").is_err());
    }

    #[test]
    fn validates_and_normalizes_hashes() {
        let lowercase = match Hash::new(VALID_HASH) {
            Ok(hash) => hash,
            Err(error) => panic!("valid hash should be accepted: {error}"),
        };
        let uppercase = match Hash::new(VALID_HASH.to_ascii_uppercase()) {
            Ok(hash) => hash,
            Err(error) => panic!("uppercase hex should be accepted: {error}"),
        };

        assert_eq!(lowercase.as_str(), VALID_HASH);
        assert_eq!(uppercase.as_str(), VALID_HASH);
        assert!(Hash::new("abc123").is_err());
    }

    #[test]
    fn rejects_invalid_identifiers_during_deserialization() {
        let error = serde_json::from_str::<EntityId>("\"skill:Bad\"")
            .err()
            .map(|error| error.to_string());
        assert_eq!(error, Some("invalid entity id `skill:Bad`".to_string()));
    }
}
