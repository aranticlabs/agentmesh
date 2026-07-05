//! Entity identity derivation and rename detection.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use similar::TextDiff;
use thiserror::Error;

use crate::EntityType;
use crate::types::{EntityId, Hash, TypeError};

/// Identity result type.
pub type Result<T> = std::result::Result<T, IdentityError>;

/// Errors produced while deriving entity identity.
#[derive(Debug, Error)]
pub enum IdentityError {
    /// A path could not be mapped to a supported entity.
    #[error("path is not a supported entity location: {}", path.display())]
    UnsupportedPath {
        /// Path that could not be classified.
        path: PathBuf,
    },
    /// A filesystem path segment was not valid UTF-8.
    #[error("path contains a non-UTF-8 segment: {}", path.display())]
    NonUtf8Path {
        /// Path containing the invalid segment.
        path: PathBuf,
    },
    /// A path segment cannot produce a stable slug.
    #[error("path segment cannot be converted to a stable slug: {value}")]
    InvalidSlug {
        /// Segment that failed slug conversion.
        value: String,
    },
    /// The derived identifier failed validation.
    #[error(transparent)]
    InvalidEntityId(#[from] TypeError),
}

/// Candidate path and content considered for rename detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameCandidate {
    /// Candidate entity root or entry file path.
    pub path: PathBuf,
    /// Candidate content hash.
    pub hash: Hash,
    /// Candidate textual content when available.
    pub contents: Option<String>,
}

/// Successful rename match.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub struct DetectedRename {
    /// Previous known path.
    pub from: PathBuf,
    /// New candidate path.
    pub to: PathBuf,
    /// Similarity score in the range `0.0..=1.0`.
    pub score: f32,
}

/// Derives the deterministic entity ID for a first-seen path.
pub fn derive_entity_id(path: &Path) -> Result<EntityId> {
    derive_entity_id_for_type(None, path)
}

/// Derives the deterministic entity ID for a first-seen path and entity type.
pub fn derive_entity_id_as(entity_type: EntityType, path: &Path) -> Result<EntityId> {
    derive_entity_id_for_type(Some(entity_type), path)
}

fn derive_entity_id_for_type(entity_type: Option<EntityType>, path: &Path) -> Result<EntityId> {
    let parts = utf8_components(path)?;

    match parts.as_slice() {
        ["AGENTS.md"] | ["CLAUDE.md"] | ["GEMINI.md"] | [".github", "copilot-instructions.md"]
            if entity_type.is_none_or(|kind| kind == EntityType::Instructions) =>
        {
            EntityId::new("instructions:root").map_err(Into::into)
        }
        [scope @ .., "AGENTS.md" | "GEMINI.md"]
            if !scope.is_empty()
                && entity_type.is_none_or(|kind| kind == EntityType::Instructions) =>
        {
            let slug = slugify_segments(scope)?;
            EntityId::new(format!("instructions:scoped:{slug}")).map_err(Into::into)
        }
        [".github", "instructions", instruction_file]
            if entity_type.is_none_or(|kind| kind == EntityType::Instructions) =>
        {
            let slug = slugify(strip_suffix(instruction_file, ".instructions.md"))?;
            EntityId::new(format!("instructions:scoped:{slug}")).map_err(Into::into)
        }
        [".github", "instructions", nested @ .., instruction_file]
            if !nested.is_empty()
                && entity_type.is_none_or(|kind| kind == EntityType::Instructions) =>
        {
            let slug =
                slugify_path_with_stem(nested, strip_suffix(instruction_file, ".instructions.md"))?;
            EntityId::new(format!("instructions:scoped:{slug}")).map_err(Into::into)
        }
        [".ai", "instructions", instruction_file]
            if entity_type.is_none_or(|kind| kind == EntityType::Instructions) =>
        {
            let slug = slugify(file_stem(instruction_file))?;
            EntityId::new(format!("instructions:scoped:{slug}")).map_err(Into::into)
        }
        [".ai", "instructions", nested @ .., instruction_file]
            if !nested.is_empty()
                && entity_type.is_none_or(|kind| kind == EntityType::Instructions) =>
        {
            let slug = slugify_path_with_stem(nested, file_stem(instruction_file))?;
            EntityId::new(format!("instructions:scoped:{slug}")).map_err(Into::into)
        }
        [
            ".ai" | ".agents" | ".github" | ".gemini",
            "skills",
            skill_name,
            ..,
        ]
        | [".claude" | ".codex", "skills", skill_name, ..]
            if entity_type.is_none_or(|kind| kind == EntityType::Skill) =>
        {
            EntityId::from_parts(EntityType::Skill, &slugify(skill_name)?).map_err(Into::into)
        }
        [".ai", "subagents", file_name]
            if entity_type.is_none_or(|kind| kind == EntityType::Subagent) =>
        {
            EntityId::from_parts(EntityType::Subagent, &slugify(file_stem(file_name))?)
                .map_err(Into::into)
        }
        [".claude" | ".codex", "agents", file_name]
            if entity_type.is_none_or(|kind| kind == EntityType::Subagent) =>
        {
            EntityId::from_parts(EntityType::Subagent, &slugify(file_stem(file_name))?)
                .map_err(Into::into)
        }
        [".github", "agents", file_name]
            if entity_type.is_none_or(|kind| kind == EntityType::Subagent) =>
        {
            let stem = file_name
                .strip_suffix(".agent.md")
                .or_else(|| file_name.strip_suffix(".md"))
                .unwrap_or(file_name);
            EntityId::from_parts(EntityType::Subagent, &slugify(stem)?).map_err(Into::into)
        }
        [".cursor", "rules", rule_file]
            if entity_type.is_none_or(|kind| kind == EntityType::Rule) =>
        {
            EntityId::from_parts(EntityType::Rule, &slugify(file_stem(rule_file))?)
                .map_err(Into::into)
        }
        [".ai", "rules", rule_file] if entity_type.is_none_or(|kind| kind == EntityType::Rule) => {
            EntityId::from_parts(EntityType::Rule, &slugify(file_stem(rule_file))?)
                .map_err(Into::into)
        }
        [".ai", "rules", nested @ .., rule_file]
            if !nested.is_empty() && entity_type.is_none_or(|kind| kind == EntityType::Rule) =>
        {
            EntityId::from_parts(
                EntityType::Rule,
                &slugify_path_with_stem(nested, file_stem(rule_file))?,
            )
            .map_err(Into::into)
        }
        [".cursor", "rules", nested @ .., rule_file]
            if !nested.is_empty() && entity_type.is_none_or(|kind| kind == EntityType::Rule) =>
        {
            EntityId::from_parts(
                EntityType::Rule,
                &slugify_path_with_stem(nested, file_stem(rule_file))?,
            )
            .map_err(Into::into)
        }
        [".claude", "rules", rule_file]
            if entity_type.is_some_and(|kind| kind == EntityType::Instructions) =>
        {
            EntityId::from_parts(EntityType::Instructions, &slugify(file_stem(rule_file))?)
                .map_err(Into::into)
        }
        [".claude", "rules", nested @ .., rule_file]
            if !nested.is_empty()
                && entity_type.is_some_and(|kind| kind == EntityType::Instructions) =>
        {
            EntityId::from_parts(
                EntityType::Instructions,
                &slugify_path_with_stem(nested, file_stem(rule_file))?,
            )
            .map_err(Into::into)
        }
        [".claude", "rules", rule_file]
            if entity_type.is_none_or(|kind| kind == EntityType::Rule) =>
        {
            EntityId::from_parts(EntityType::Rule, &slugify(file_stem(rule_file))?)
                .map_err(Into::into)
        }
        [".claude", "rules", nested @ .., rule_file]
            if !nested.is_empty() && entity_type.is_none_or(|kind| kind == EntityType::Rule) =>
        {
            EntityId::from_parts(
                EntityType::Rule,
                &slugify_path_with_stem(nested, file_stem(rule_file))?,
            )
            .map_err(Into::into)
        }
        [".github", "prompts", prompt_file]
            if entity_type.is_none_or(|kind| kind == EntityType::Prompt) =>
        {
            EntityId::from_parts(
                EntityType::Prompt,
                &slugify(strip_suffix(prompt_file, ".prompt.md"))?,
            )
            .map_err(Into::into)
        }
        [".ai", "prompts", prompt_file]
            if entity_type.is_none_or(|kind| kind == EntityType::Prompt) =>
        {
            EntityId::from_parts(EntityType::Prompt, &slugify(file_stem(prompt_file))?)
                .map_err(Into::into)
        }
        [".ai", "prompts", nested @ .., prompt_file]
            if !nested.is_empty() && entity_type.is_none_or(|kind| kind == EntityType::Prompt) =>
        {
            EntityId::from_parts(
                EntityType::Prompt,
                &slugify_path_with_stem(nested, file_stem(prompt_file))?,
            )
            .map_err(Into::into)
        }
        [".ai", "commands", command @ ..]
            if !command.is_empty()
                && entity_type.is_none_or(|kind| kind == EntityType::Command) =>
        {
            EntityId::from_parts(EntityType::Command, &command_slug(command)?).map_err(Into::into)
        }
        [".claude" | ".gemini", "commands", command @ ..]
            if !command.is_empty()
                && entity_type.is_none_or(|kind| kind == EntityType::Command) =>
        {
            EntityId::from_parts(EntityType::Command, &command_slug(command)?).map_err(Into::into)
        }
        [".ai", "hooks", file_name] if entity_type.is_none_or(|kind| kind == EntityType::Hook) => {
            EntityId::from_parts(EntityType::Hook, &slugify(file_stem(file_name))?)
                .map_err(Into::into)
        }
        [".ai", "hooks", nested @ .., file_name]
            if !nested.is_empty() && entity_type.is_none_or(|kind| kind == EntityType::Hook) =>
        {
            EntityId::from_parts(
                EntityType::Hook,
                &slugify_path_with_stem(nested, file_stem(file_name))?,
            )
            .map_err(Into::into)
        }
        [".ai", "mcp-bindings", file_name]
            if entity_type.is_none_or(|kind| kind == EntityType::McpBinding) =>
        {
            EntityId::from_parts(EntityType::McpBinding, &slugify(file_stem(file_name))?)
                .map_err(Into::into)
        }
        [".ai", "mcp-bindings", nested @ .., file_name]
            if !nested.is_empty()
                && entity_type.is_none_or(|kind| kind == EntityType::McpBinding) =>
        {
            EntityId::from_parts(
                EntityType::McpBinding,
                &slugify_path_with_stem(nested, file_stem(file_name))?,
            )
            .map_err(Into::into)
        }
        [".ai", "permission-policies", file_name]
            if entity_type.is_none_or(|kind| kind == EntityType::PermissionPolicy) =>
        {
            EntityId::from_parts(
                EntityType::PermissionPolicy,
                &slugify(file_stem(file_name))?,
            )
            .map_err(Into::into)
        }
        [".ai", "permission-policies", nested @ .., file_name]
            if !nested.is_empty()
                && entity_type.is_none_or(|kind| kind == EntityType::PermissionPolicy) =>
        {
            EntityId::from_parts(
                EntityType::PermissionPolicy,
                &slugify_path_with_stem(nested, file_stem(file_name))?,
            )
            .map_err(Into::into)
        }
        [".claude", "settings.json"] if entity_type.is_none_or(|kind| kind == EntityType::Hook) => {
            EntityId::from_parts(EntityType::Hook, "claude-project").map_err(Into::into)
        }
        [".gemini", "settings.json"] if entity_type.is_none_or(|kind| kind == EntityType::Hook) => {
            EntityId::from_parts(EntityType::Hook, "gemini-project").map_err(Into::into)
        }
        [".codex", "hooks.json"] if entity_type.is_none_or(|kind| kind == EntityType::Hook) => {
            EntityId::from_parts(EntityType::Hook, "codex-project").map_err(Into::into)
        }
        [".mcp.json"] if entity_type.is_none_or(|kind| kind == EntityType::McpBinding) => {
            EntityId::from_parts(EntityType::McpBinding, "project").map_err(Into::into)
        }
        [".gemini", "settings.json"]
            if entity_type.is_some_and(|kind| kind == EntityType::McpBinding) =>
        {
            EntityId::from_parts(EntityType::McpBinding, "gemini-project").map_err(Into::into)
        }
        [".codex", "config.toml"]
            if entity_type.is_some_and(|kind| kind == EntityType::McpBinding) =>
        {
            EntityId::from_parts(EntityType::McpBinding, "codex-project").map_err(Into::into)
        }
        [".claude", "settings.json"]
            if entity_type.is_some_and(|kind| kind == EntityType::PermissionPolicy) =>
        {
            EntityId::from_parts(EntityType::PermissionPolicy, "claude-project").map_err(Into::into)
        }
        [".gemini", "settings.json"]
            if entity_type.is_some_and(|kind| kind == EntityType::PermissionPolicy) =>
        {
            EntityId::from_parts(EntityType::PermissionPolicy, "gemini-project").map_err(Into::into)
        }
        [".codex", "config.toml"]
            if entity_type.is_some_and(|kind| kind == EntityType::PermissionPolicy) =>
        {
            EntityId::from_parts(EntityType::PermissionPolicy, "codex-project").map_err(Into::into)
        }
        [".codex", "rules", file_name]
            if entity_type.is_some_and(|kind| kind == EntityType::PermissionPolicy) =>
        {
            EntityId::from_parts(
                EntityType::PermissionPolicy,
                &slugify(file_stem(file_name))?,
            )
            .map_err(Into::into)
        }
        _ => Err(IdentityError::UnsupportedPath {
            path: path.to_path_buf(),
        }),
    }
}

/// Resolves an ID collision by appending the first available numeric suffix.
pub fn resolve_collision(base: &EntityId, existing: &BTreeSet<EntityId>) -> Result<EntityId> {
    if !existing.contains(base) {
        return Ok(base.clone());
    }

    let (prefix, slug) =
        base.as_str()
            .split_once(':')
            .ok_or_else(|| IdentityError::UnsupportedPath {
                path: PathBuf::from(base.as_str()),
            })?;

    for suffix in 2_u32.. {
        let candidate = EntityId::new(format!("{prefix}:{slug}-{suffix}"))?;
        if !existing.contains(&candidate) {
            return Ok(candidate);
        }
    }

    Err(IdentityError::UnsupportedPath {
        path: PathBuf::from(base.as_str()),
    })
}

/// Parses an optional identity pin marker from the first few body lines.
pub fn parse_pin_marker(contents: &str) -> Result<Option<EntityId>> {
    for line in contents.lines().take(5) {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("<!--") else {
            continue;
        };
        let Some(inner) = rest.strip_suffix("-->") else {
            continue;
        };
        let inner = inner.trim();
        if let Some(id) = inner.strip_prefix("agentmesh:id=") {
            return EntityId::new(id.trim()).map(Some).map_err(Into::into);
        }
    }

    Ok(None)
}

/// Finds the best rename candidate by exact hash first, then textual similarity.
pub fn detect_rename(
    previous_path: &Path,
    previous_hash: &Hash,
    previous_contents: Option<&str>,
    candidates: &[RenameCandidate],
    threshold: f32,
) -> Option<DetectedRename> {
    let mut best: Option<DetectedRename> = None;

    for candidate in candidates {
        let score = if &candidate.hash == previous_hash {
            1.0
        } else {
            match (previous_contents, candidate.contents.as_deref()) {
                (Some(previous), Some(current)) => TextDiff::from_chars(previous, current).ratio(),
                _ => 0.0,
            }
        };

        if score < threshold {
            continue;
        }

        let replace = best
            .as_ref()
            .map(|current| {
                score > current.score
                    || ((score - current.score).abs() <= f32::EPSILON
                        && candidate.path < current.to)
            })
            .unwrap_or(true);

        if replace {
            best = Some(DetectedRename {
                from: previous_path.to_path_buf(),
                to: candidate.path.clone(),
                score,
            });
        }
    }

    best
}

fn utf8_components(path: &Path) -> Result<Vec<&str>> {
    path.iter()
        .map(|segment| {
            segment.to_str().ok_or_else(|| IdentityError::NonUtf8Path {
                path: path.to_path_buf(),
            })
        })
        .collect()
}

fn file_stem(file_name: &str) -> &str {
    Path::new(file_name)
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or(file_name)
}

fn strip_suffix<'a>(value: &'a str, suffix: &str) -> &'a str {
    value.strip_suffix(suffix).unwrap_or(value)
}

fn slugify_segments(segments: &[&str]) -> Result<String> {
    slugify(&segments.join("-"))
}

fn slugify_path_with_stem(segments: &[&str], stem: &str) -> Result<String> {
    let mut parts = segments.to_vec();
    parts.push(stem);
    slugify_segments(&parts)
}

fn command_slug(command: &[&str]) -> Result<String> {
    let Some((file_name, namespace)) = command.split_last() else {
        return Err(IdentityError::InvalidSlug {
            value: String::new(),
        });
    };
    let command_name = slugify(file_stem(file_name))?;
    if namespace.is_empty() {
        return Ok(command_name);
    }
    let namespace = slugify_segments(namespace)?;
    Ok(format!("{namespace}:{command_name}"))
}

fn slugify(value: &str) -> Result<String> {
    let mut slug = String::new();
    let mut previous_dash = false;

    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }

    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        Err(IdentityError::InvalidSlug {
            value: value.to_string(),
        })
    } else {
        Ok(slug)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::{
        RenameCandidate, derive_entity_id, derive_entity_id_as, detect_rename, parse_pin_marker,
        resolve_collision,
    };
    use crate::EntityType;
    use crate::types::{EntityId, Hash};

    fn hash(value: &str) -> Hash {
        match Hash::new(value) {
            Ok(hash) => hash,
            Err(error) => panic!("test hash should be valid: {error}"),
        }
    }

    fn entity_id(value: &str) -> EntityId {
        match EntityId::new(value) {
            Ok(entity_id) => entity_id,
            Err(error) => panic!("test entity id should be valid: {error}"),
        }
    }

    #[test]
    fn derives_ids_from_supported_paths() {
        let cases = [
            ("AGENTS.md", "instructions:root"),
            ("CLAUDE.md", "instructions:root"),
            ("GEMINI.md", "instructions:root"),
            ("packages/api/AGENTS.md", "instructions:scoped:packages-api"),
            ("packages/api/GEMINI.md", "instructions:scoped:packages-api"),
            (
                ".github/instructions/api-review.instructions.md",
                "instructions:scoped:api-review",
            ),
            (
                ".claude/skills/Security Review/SKILL.md",
                "skill:security-review",
            ),
            (
                ".codex/skills/security-review/SKILL.md",
                "skill:security-review",
            ),
            (".ai/skills/api-design/SKILL.md", "skill:api-design"),
            (
                ".agents/skills/shared-analysis/SKILL.md",
                "skill:shared-analysis",
            ),
            (".claude/agents/code-reviewer.md", "subagent:code-reviewer"),
            (".codex/agents/code-reviewer.toml", "subagent:code-reviewer"),
            (
                ".github/agents/code-reviewer.agent.md",
                "subagent:code-reviewer",
            ),
            (
                ".github/agents/performance-reviewer.md",
                "subagent:performance-reviewer",
            ),
            (".ai/subagents/code-reviewer.md", "subagent:code-reviewer"),
            (".cursor/rules/security.mdc", "rule:security"),
            (".claude/rules/security.md", "rule:security"),
            (
                ".github/prompts/release-notes.prompt.md",
                "prompt:release-notes",
            ),
            (".claude/commands/git/commit.md", "command:git:commit"),
            (".gemini/commands/review.toml", "command:review"),
            (".claude/settings.json", "hook:claude-project"),
            (".gemini/settings.json", "hook:gemini-project"),
            (".codex/hooks.json", "hook:codex-project"),
            (".mcp.json", "mcp-binding:project"),
        ];

        for (path, expected) in cases {
            let actual = match derive_entity_id(Path::new(path)) {
                Ok(actual) => actual,
                Err(error) => panic!("path should derive an id: {error}"),
            };
            assert_eq!(actual.as_str(), expected);
        }
    }

    #[test]
    fn derives_typed_ids_for_multi_entity_config_paths() {
        let cases = [
            (
                EntityType::Instructions,
                ".claude/rules/api.md",
                "instructions:scoped:api",
            ),
            (
                EntityType::McpBinding,
                ".gemini/settings.json",
                "mcp-binding:gemini-project",
            ),
            (
                EntityType::McpBinding,
                ".codex/config.toml",
                "mcp-binding:codex-project",
            ),
            (
                EntityType::PermissionPolicy,
                ".claude/settings.json",
                "permission-policy:claude-project",
            ),
            (
                EntityType::PermissionPolicy,
                ".gemini/settings.json",
                "permission-policy:gemini-project",
            ),
            (
                EntityType::PermissionPolicy,
                ".codex/config.toml",
                "permission-policy:codex-project",
            ),
            (
                EntityType::PermissionPolicy,
                ".codex/rules/strict.rules",
                "permission-policy:strict",
            ),
        ];

        for (entity_type, path, expected) in cases {
            let actual = match derive_entity_id_as(entity_type, Path::new(path)) {
                Ok(actual) => actual,
                Err(error) => panic!("path should derive a typed id: {error}"),
            };
            assert_eq!(actual.as_str(), expected);
        }
    }

    #[test]
    fn resolves_collisions_with_numeric_suffixes() {
        let existing = BTreeSet::from([
            entity_id("skill:security-review"),
            entity_id("skill:security-review-2"),
        ]);
        let resolved = match resolve_collision(&entity_id("skill:security-review"), &existing) {
            Ok(resolved) => resolved,
            Err(error) => panic!("collision should resolve: {error}"),
        };

        assert_eq!(resolved.as_str(), "skill:security-review-3");
    }

    #[test]
    fn resolves_namespaced_command_collisions_on_command_segment() {
        let existing = BTreeSet::from([
            entity_id("command:git:commit"),
            entity_id("command:git:commit-2"),
        ]);
        let resolved = match resolve_collision(&entity_id("command:git:commit"), &existing) {
            Ok(resolved) => resolved,
            Err(error) => panic!("collision should resolve: {error}"),
        };

        assert_eq!(resolved.as_str(), "command:git:commit-3");
    }

    #[test]
    fn parses_optional_pin_marker() {
        let marker = match parse_pin_marker(
            r#"
<!-- agentmesh:id=skill:custom-review -->
# Skill
"#,
        ) {
            Ok(marker) => marker,
            Err(error) => panic!("marker parse should succeed: {error}"),
        };

        assert_eq!(
            marker.map(|id| id.to_string()),
            Some("skill:custom-review".to_string())
        );
    }

    #[test]
    fn detects_exact_hash_rename() {
        let shared_hash = hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let detected = detect_rename(
            Path::new(".claude/skills/old/SKILL.md"),
            &shared_hash,
            None,
            &[RenameCandidate {
                path: Path::new(".claude/skills/new/SKILL.md").to_path_buf(),
                hash: shared_hash.clone(),
                contents: None,
            }],
            0.8,
        );

        assert_eq!(
            detected.map(|rename| rename.to),
            Some(Path::new(".claude/skills/new/SKILL.md").to_path_buf())
        );
    }

    #[test]
    fn detects_similarity_rename_above_threshold() {
        let previous_hash =
            hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let candidate_hash =
            hash("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let detected = detect_rename(
            Path::new(".claude/skills/old/SKILL.md"),
            &previous_hash,
            Some("one\ntwo\nthree\n"),
            &[RenameCandidate {
                path: Path::new(".claude/skills/new/SKILL.md").to_path_buf(),
                hash: candidate_hash,
                contents: Some("one\ntwo\nthree\nfour\n".to_string()),
            }],
            0.7,
        );

        assert!(detected.is_some());
    }

    #[test]
    fn ignores_similarity_rename_below_threshold() {
        let previous_hash =
            hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let candidate_hash =
            hash("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let detected = detect_rename(
            Path::new(".claude/skills/old/SKILL.md"),
            &previous_hash,
            Some("alpha\nbeta\ngamma\n"),
            &[RenameCandidate {
                path: Path::new(".claude/skills/new/SKILL.md").to_path_buf(),
                hash: candidate_hash,
                contents: Some("totally\ndifferent\ncontent\n".to_string()),
            }],
            0.9,
        );

        assert!(detected.is_none());
    }

    #[test]
    fn chooses_stable_path_when_rename_scores_tie() {
        let previous_hash =
            hash("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let candidate_hash =
            hash("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let detected = detect_rename(
            Path::new(".claude/skills/old/SKILL.md"),
            &previous_hash,
            Some("same\nbody\n"),
            &[
                RenameCandidate {
                    path: Path::new(".claude/skills/zeta/SKILL.md").to_path_buf(),
                    hash: candidate_hash.clone(),
                    contents: Some("same\nbody\n".to_string()),
                },
                RenameCandidate {
                    path: Path::new(".claude/skills/alpha/SKILL.md").to_path_buf(),
                    hash: candidate_hash,
                    contents: Some("same\nbody\n".to_string()),
                },
            ],
            0.8,
        );

        assert_eq!(
            detected.map(|rename| rename.to),
            Some(Path::new(".claude/skills/alpha/SKILL.md").to_path_buf())
        );
    }
}
