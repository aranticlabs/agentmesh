//! Structured merge support for canonical Markdown entities.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_norway::{Mapping, Value};
use thiserror::Error;

use crate::state::{StateError, conflict_entity_dir, conflict_version_file_name, write_atomic};
use crate::types::{EntityId, RuntimeName};

const COMMON_FRONTMATTER_KEYS: &[&str] = &["name", "description", "allowed-tools", "model"];
const SET_LIKE_KEYS: &[&str] = &["allowed-tools", "tags", "categories"];

/// Merge result type.
pub type Result<T> = std::result::Result<T, MergeError>;

/// Errors produced while merging canonical entities.
#[derive(Debug, Error)]
pub enum MergeError {
    /// Frontmatter could not be parsed.
    #[error("failed to parse frontmatter")]
    ParseFrontmatter {
        /// Source YAML parse error.
        #[source]
        source: serde_norway::Error,
    },
    /// Frontmatter must be a mapping when present.
    #[error("frontmatter must be a mapping")]
    FrontmatterNotMapping,
    /// YAML serialization failed.
    #[error("failed to serialize frontmatter")]
    SerializeFrontmatter {
        /// Source YAML serialization error.
        #[source]
        source: serde_norway::Error,
    },
    /// Conflict preservation failed.
    #[error(transparent)]
    State(#[from] StateError),
}

/// One side of a two-way divergence from an ancestor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeSide {
    /// The current canonical side.
    Current,
    /// The incoming runtime side.
    Incoming,
}

/// Merge status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeStatus {
    /// Structured merge completed without unresolved overlap.
    Clean,
    /// Tiebreaker selected a winner after unresolved overlap.
    Tiebreaker {
        /// Winning side.
        winner: MergeSide,
        /// Losing side.
        loser: MergeSide,
    },
}

/// Result of merging two divergent canonical versions.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct MergeResult {
    /// Merged canonical Markdown.
    pub merged: String,
    /// Whether the merge was clean or tiebreaker-resolved.
    pub status: MergeStatus,
}

/// Splits Markdown into parsed frontmatter and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownParts {
    /// Parsed YAML frontmatter.
    pub frontmatter: Mapping,
    /// Body after the frontmatter delimiter.
    pub body: String,
}

/// Canonicalizes frontmatter key ordering while preserving body text.
pub fn canonicalize_markdown(input: &str) -> Result<String> {
    let parts = split_markdown(input)?;
    compose_markdown(&parts.frontmatter, &parts.body)
}

/// Performs a structured three-way merge in canonical Markdown space.
pub fn merge_markdown(
    ancestor: &str,
    current: &str,
    incoming: &str,
    current_mtime: SystemTime,
    incoming_mtime: SystemTime,
) -> Result<MergeResult> {
    let ancestor = split_markdown(ancestor)?;
    let current = split_markdown(current)?;
    let incoming = split_markdown(incoming)?;

    let frontmatter = merge_frontmatter(
        &ancestor.frontmatter,
        &current.frontmatter,
        &incoming.frontmatter,
    );
    let body = merge_body(&ancestor.body, &current.body, &incoming.body);

    match (frontmatter, body) {
        (StructuredMerge::Clean(frontmatter), StructuredMerge::Clean(body)) => Ok(MergeResult {
            merged: compose_markdown(&frontmatter, &body)?,
            status: MergeStatus::Clean,
        }),
        _ => {
            let (winner, loser, merged) = if incoming_mtime > current_mtime {
                (MergeSide::Incoming, MergeSide::Current, incoming)
            } else {
                (MergeSide::Current, MergeSide::Incoming, current)
            };

            Ok(MergeResult {
                merged: compose_markdown(&merged.frontmatter, &merged.body)?,
                status: MergeStatus::Tiebreaker { winner, loser },
            })
        }
    }
}

/// Preserves a losing conflict version under an entity-specific cache directory.
pub fn preserve_losing_version(
    conflicts_dir: &Path,
    entity_id: &EntityId,
    runtime: &RuntimeName,
    timestamp: &str,
    contents: &str,
) -> Result<PathBuf> {
    let path = conflict_entity_dir(conflicts_dir, entity_id)
        .join(conflict_version_file_name(runtime, timestamp));
    write_atomic(&path, contents.as_bytes())?;
    Ok(path)
}

fn split_markdown(input: &str) -> Result<MarkdownParts> {
    let Some(rest) = input.strip_prefix("---\n") else {
        return Ok(MarkdownParts {
            frontmatter: Mapping::new(),
            body: input.to_string(),
        });
    };
    let Some(end) = rest.find("\n---\n") else {
        return Ok(MarkdownParts {
            frontmatter: Mapping::new(),
            body: input.to_string(),
        });
    };

    let frontmatter = &rest[..end];
    let body = &rest[end + "\n---\n".len()..];

    Ok(MarkdownParts {
        frontmatter: parse_frontmatter_mapping(frontmatter)?,
        body: body.to_string(),
    })
}

pub(crate) fn parse_frontmatter_mapping(frontmatter: &str) -> Result<Mapping> {
    if frontmatter.trim().is_empty() {
        return Ok(Mapping::new());
    }

    match serde_norway::from_str::<Value>(frontmatter) {
        Ok(Value::Mapping(mapping)) => Ok(mapping),
        Ok(Value::Null) => Ok(Mapping::new()),
        Ok(_) => Err(MergeError::FrontmatterNotMapping),
        Err(source) => parse_flat_frontmatter_mapping(frontmatter)
            .ok_or(MergeError::ParseFrontmatter { source }),
    }
}

fn parse_flat_frontmatter_mapping(frontmatter: &str) -> Option<Mapping> {
    let mut mapping = Mapping::new();

    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.chars().next().is_some_and(char::is_whitespace) {
            return None;
        }

        let (key, value) = line.split_once(':')?;
        let key = key.trim();
        if key.is_empty() || !key.chars().all(is_plain_frontmatter_key_char) {
            return None;
        }

        let value = value.trim();
        if value
            .chars()
            .next()
            .is_some_and(|character| matches!(character, '"' | '\'' | '[' | '{' | '|' | '>'))
        {
            return None;
        }

        mapping.insert(
            Value::String(key.to_string()),
            Value::String(value.to_string()),
        );
    }

    Some(mapping)
}

fn is_plain_frontmatter_key_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
}

fn compose_markdown(frontmatter: &Mapping, body: &str) -> Result<String> {
    let ordered = ordered_frontmatter(frontmatter);
    let frontmatter = yaml_fragment(&Value::Mapping(ordered))?;
    Ok(format!("---\n{frontmatter}---\n{body}"))
}

fn yaml_fragment(value: &Value) -> Result<String> {
    let serialized = serde_norway::to_string(value)
        .map_err(|source| MergeError::SerializeFrontmatter { source })?;
    let without_start = serialized.strip_prefix("---\n").unwrap_or(&serialized);
    let without_end = without_start.strip_suffix("...\n").unwrap_or(without_start);
    Ok(without_end.to_string())
}

fn ordered_frontmatter(frontmatter: &Mapping) -> Mapping {
    let mut output = Mapping::new();
    let mut emitted = HashSet::new();

    for key in COMMON_FRONTMATTER_KEYS {
        if let Some(value) = frontmatter.get(*key) {
            output.insert(Value::String((*key).to_string()), value.clone());
            emitted.insert((*key).to_string());
        }
    }

    let mut remaining = frontmatter
        .iter()
        .filter_map(|(key, value)| key.as_str().map(|key| (key.to_string(), value.clone())))
        .filter(|(key, _)| !emitted.contains(key))
        .collect::<Vec<_>>();
    remaining.sort_by(|left, right| left.0.cmp(&right.0));

    for (key, value) in remaining {
        output.insert(Value::String(key), value);
    }

    output
}

enum StructuredMerge<T> {
    Clean(T),
    Conflict,
}

fn merge_frontmatter(
    ancestor: &Mapping,
    current: &Mapping,
    incoming: &Mapping,
) -> StructuredMerge<Mapping> {
    let mut keys = BTreeSet::new();
    collect_keys(ancestor, &mut keys);
    collect_keys(current, &mut keys);
    collect_keys(incoming, &mut keys);

    let mut merged = Mapping::new();
    for key in keys {
        let key_value = Value::String(key.clone());
        let ancestor_value = ancestor.get(key.as_str());
        let current_value = current.get(key.as_str());
        let incoming_value = incoming.get(key.as_str());

        let merged_value = merge_value(&key, ancestor_value, current_value, incoming_value);
        match merged_value {
            ValueMerge::Value(Some(value)) => {
                merged.insert(key_value, value);
            }
            ValueMerge::Value(None) => {}
            ValueMerge::Conflict => return StructuredMerge::Conflict,
        }
    }

    StructuredMerge::Clean(merged)
}

fn collect_keys(mapping: &Mapping, keys: &mut BTreeSet<String>) {
    for key in mapping.keys() {
        if let Some(key) = key.as_str() {
            keys.insert(key.to_string());
        }
    }
}

enum ValueMerge {
    Value(Option<Value>),
    Conflict,
}

fn merge_value(
    key: &str,
    ancestor: Option<&Value>,
    current: Option<&Value>,
    incoming: Option<&Value>,
) -> ValueMerge {
    if current == incoming {
        return ValueMerge::Value(current.cloned());
    }
    if current == ancestor {
        return ValueMerge::Value(incoming.cloned());
    }
    if incoming == ancestor {
        return ValueMerge::Value(current.cloned());
    }
    if SET_LIKE_KEYS.contains(&key) {
        if let (
            Some(Value::Sequence(ancestor)),
            Some(Value::Sequence(current)),
            Some(Value::Sequence(incoming)),
        ) = (ancestor, current, incoming)
        {
            return ValueMerge::Value(Some(merge_set_like_sequence(ancestor, current, incoming)));
        }
    }

    ValueMerge::Conflict
}

fn merge_set_like_sequence(ancestor: &[Value], current: &[Value], incoming: &[Value]) -> Value {
    let ancestor_set = ancestor.iter().collect::<HashSet<_>>();
    let current_set = current.iter().collect::<HashSet<_>>();
    let incoming_set = incoming.iter().collect::<HashSet<_>>();

    let mut output = Vec::new();
    let mut seen = HashSet::new();
    for value in ancestor {
        if current_set.contains(value) && incoming_set.contains(value) {
            output.push(value.clone());
            seen.insert(value.clone());
        }
    }
    for value in current.iter().chain(incoming.iter()) {
        if !ancestor_set.contains(value) && seen.insert(value.clone()) {
            output.push(value.clone());
        }
    }

    Value::Sequence(output)
}

fn merge_body(ancestor: &str, current: &str, incoming: &str) -> StructuredMerge<String> {
    if current == incoming {
        return StructuredMerge::Clean(current.to_string());
    }
    if current == ancestor {
        return StructuredMerge::Clean(incoming.to_string());
    }
    if incoming == ancestor {
        return StructuredMerge::Clean(current.to_string());
    }

    let ancestor_lines = split_lines(ancestor);
    let current_lines = split_lines(current);
    let incoming_lines = split_lines(incoming);
    let current_change = changed_range(&ancestor_lines, &current_lines);
    let incoming_change = changed_range(&ancestor_lines, &incoming_lines);

    if current_change.end <= incoming_change.start {
        return StructuredMerge::Clean(apply_non_overlapping_changes(
            &ancestor_lines,
            &current_change,
            &incoming_change,
        ));
    }
    if incoming_change.end <= current_change.start {
        return StructuredMerge::Clean(apply_non_overlapping_changes(
            &ancestor_lines,
            &incoming_change,
            &current_change,
        ));
    }

    StructuredMerge::Conflict
}

#[derive(Debug, Clone)]
struct LineChange {
    start: usize,
    end: usize,
    replacement: Vec<String>,
}

fn changed_range(ancestor: &[String], changed: &[String]) -> LineChange {
    let mut prefix = 0;
    while prefix < ancestor.len() && prefix < changed.len() && ancestor[prefix] == changed[prefix] {
        prefix += 1;
    }

    let mut suffix = 0;
    while suffix < ancestor.len().saturating_sub(prefix)
        && suffix < changed.len().saturating_sub(prefix)
        && ancestor[ancestor.len() - 1 - suffix] == changed[changed.len() - 1 - suffix]
    {
        suffix += 1;
    }

    LineChange {
        start: prefix,
        end: ancestor.len() - suffix,
        replacement: changed[prefix..changed.len() - suffix].to_vec(),
    }
}

fn apply_non_overlapping_changes(
    ancestor: &[String],
    first: &LineChange,
    second: &LineChange,
) -> String {
    let mut output = String::new();
    push_lines(&mut output, &ancestor[..first.start]);
    push_lines(&mut output, &first.replacement);
    push_lines(&mut output, &ancestor[first.end..second.start]);
    push_lines(&mut output, &second.replacement);
    push_lines(&mut output, &ancestor[second.end..]);
    output
}

fn split_lines(value: &str) -> Vec<String> {
    value.split_inclusive('\n').map(str::to_string).collect()
}

fn push_lines(output: &mut String, lines: &[String]) {
    for line in lines {
        output.push_str(line);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    use super::{
        MergeSide, MergeStatus, canonicalize_markdown, merge_markdown, preserve_losing_version,
    };
    use crate::types::{EntityId, RuntimeName};

    fn entity_id(value: &str) -> EntityId {
        match EntityId::new(value) {
            Ok(entity_id) => entity_id,
            Err(error) => panic!("test entity id should be valid: {error}"),
        }
    }

    fn runtime_name(value: &str) -> RuntimeName {
        match RuntimeName::new(value) {
            Ok(runtime) => runtime,
            Err(error) => panic!("test runtime name should be valid: {error}"),
        }
    }

    #[test]
    fn canonicalizes_common_frontmatter_order() {
        let canonical = match canonicalize_markdown(
            r#"---
zeta: true
description: Audit code
name: security-review
allowed-tools:
- Read
---
Body
"#,
        ) {
            Ok(canonical) => canonical,
            Err(error) => panic!("canonicalization should succeed: {error}"),
        };

        assert!(
            canonical
                .starts_with("---\nname: security-review\ndescription: Audit code\nallowed-tools:")
        );
    }

    #[test]
    fn canonicalizes_flat_frontmatter_value_with_extra_colon() {
        let canonical = match canonicalize_markdown(
            "---\nname: implementation-auditor\ndescription: It performs a strict audit: enumerates scoped work.\nmodel: opus\n---\nBody\n",
        ) {
            Ok(canonical) => canonical,
            Err(error) => panic!("canonicalization should succeed: {error}"),
        };

        assert!(canonical.contains("name: implementation-auditor"));
        assert!(canonical.contains("audit: enumerates scoped work"));
        assert!(canonical.ends_with("Body\n"));
    }

    #[test]
    fn malformed_structured_frontmatter_still_fails() {
        let input = "---\nname: demo\nmetadata: {unterminated\n---\nBody\n";
        assert!(canonicalize_markdown(input).is_err());
    }

    #[test]
    fn merges_non_overlapping_frontmatter_and_body_changes() {
        let ancestor = "---\nname: security-review\ndescription: Audit code\n---\n# Title\n\nFocus on OWASP.\n";
        let current = "---\nname: security-review\ndescription: Audit code with OWASP focus\n---\n# Title\n\nFocus on OWASP.\n";
        let incoming = "---\nname: security-review\ndescription: Audit code\n---\n# Title\n\nFocus on OWASP.\n\nUse headings.\n";

        let merged = match merge_markdown(
            ancestor,
            current,
            incoming,
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        ) {
            Ok(merged) => merged,
            Err(error) => panic!("merge should succeed: {error}"),
        };

        assert_eq!(merged.status, MergeStatus::Clean);
        assert!(
            merged
                .merged
                .contains("description: Audit code with OWASP focus")
        );
        assert!(merged.merged.contains("Use headings."));
    }

    #[test]
    fn merges_set_like_frontmatter_lists() {
        let ancestor = "---\nname: security-review\nallowed-tools:\n- Read\n---\n";
        let current = "---\nname: security-review\nallowed-tools:\n- Read\n- Grep\n---\n";
        let incoming = "---\nname: security-review\nallowed-tools:\n- Read\n- Bash\n---\n";

        let merged = match merge_markdown(
            ancestor,
            current,
            incoming,
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH,
        ) {
            Ok(merged) => merged,
            Err(error) => panic!("merge should succeed: {error}"),
        };

        assert_eq!(merged.status, MergeStatus::Clean);
        assert!(merged.merged.contains("- Grep"));
        assert!(merged.merged.contains("- Bash"));
    }

    #[test]
    fn tiebreaks_overlapping_body_changes() {
        let ancestor = "---\nname: security-review\n---\nFocus on OWASP.\n";
        let current = "---\nname: security-review\n---\nFocus on OWASP and CWE.\n";
        let incoming = "---\nname: security-review\n---\nFocus on OWASP 2026.\n";

        let merged = match merge_markdown(
            ancestor,
            current,
            incoming,
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH + Duration::from_secs(4),
        ) {
            Ok(merged) => merged,
            Err(error) => panic!("merge should succeed: {error}"),
        };

        assert_eq!(
            merged.status,
            MergeStatus::Tiebreaker {
                winner: MergeSide::Incoming,
                loser: MergeSide::Current
            }
        );
        assert!(merged.merged.contains("OWASP 2026"));
    }

    #[test]
    fn current_side_wins_when_incoming_is_not_newer() {
        let ancestor = "---\nname: security-review\n---\nFocus on OWASP.\n";
        let current = "---\nname: security-review\n---\nFocus on OWASP and CWE.\n";
        let incoming = "---\nname: security-review\n---\nFocus on OWASP 2026.\n";

        let merged = match merge_markdown(
            ancestor,
            current,
            incoming,
            SystemTime::UNIX_EPOCH + Duration::from_secs(4),
            SystemTime::UNIX_EPOCH,
        ) {
            Ok(merged) => merged,
            Err(error) => panic!("merge should succeed: {error}"),
        };

        assert_eq!(
            merged.status,
            MergeStatus::Tiebreaker {
                winner: MergeSide::Current,
                loser: MergeSide::Incoming
            }
        );
        assert!(merged.merged.contains("OWASP and CWE"));
    }

    #[test]
    fn preserves_losing_version() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };

        let path = match preserve_losing_version(
            temp.path(),
            &entity_id("skill:security-review"),
            &runtime_name("codex"),
            "2026-05-24T14:32:11Z",
            "losing content",
        ) {
            Ok(path) => path,
            Err(error) => panic!("preservation should succeed: {error}"),
        };

        assert!(path.starts_with(Path::new(temp.path())));
        assert!(path.ends_with("codex-2026-05-24T14-32-11Z.md"));
        assert!(path.exists());
    }
}
