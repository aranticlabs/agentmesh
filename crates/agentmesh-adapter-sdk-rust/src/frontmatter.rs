use std::collections::HashSet;

use serde_norway::{Mapping, Value as YamlValue};

use crate::{AdapterError, Result};

const COMMON_FRONTMATTER_KEYS: &[&str] = &["name", "description", "allowed-tools", "model"];

/// Parsed Markdown frontmatter and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontmatterDocument {
    /// Parsed YAML frontmatter.
    pub frontmatter: Mapping,
    /// Body content after frontmatter.
    pub body: String,
}

/// Splits Markdown into YAML frontmatter and body content.
pub fn parse_frontmatter(markdown: &str) -> Result<FrontmatterDocument> {
    let Some(rest) = markdown.strip_prefix("---\n") else {
        return Ok(FrontmatterDocument {
            frontmatter: Mapping::new(),
            body: markdown.to_string(),
        });
    };
    let Some(end) = rest.find("\n---\n") else {
        return Ok(FrontmatterDocument {
            frontmatter: Mapping::new(),
            body: markdown.to_string(),
        });
    };

    let frontmatter = &rest[..end];
    let body = &rest[end + "\n---\n".len()..];
    Ok(FrontmatterDocument {
        frontmatter: parse_frontmatter_mapping(frontmatter)?,
        body: body.to_string(),
    })
}

/// Serializes Markdown with stable frontmatter key ordering.
pub fn compose_frontmatter(document: &FrontmatterDocument) -> Result<String> {
    let ordered = ordered_frontmatter(&document.frontmatter);
    let frontmatter = yaml_fragment(&YamlValue::Mapping(ordered))?;
    Ok(format!("---\n{frontmatter}---\n{}", document.body))
}

/// Canonicalizes Markdown frontmatter key ordering.
pub fn canonicalize_frontmatter(markdown: &str) -> Result<String> {
    compose_frontmatter(&parse_frontmatter(markdown)?)
}

fn parse_frontmatter_mapping(frontmatter: &str) -> Result<Mapping> {
    if frontmatter.trim().is_empty() {
        return Ok(Mapping::new());
    }

    match serde_norway::from_str::<YamlValue>(frontmatter) {
        Ok(YamlValue::Mapping(mapping)) => Ok(mapping),
        Ok(YamlValue::Null) => Ok(Mapping::new()),
        Ok(_) => Err(AdapterError::FrontmatterNotMapping),
        Err(source) => parse_flat_frontmatter_mapping(frontmatter)
            .ok_or(AdapterError::ParseFrontmatter { source }),
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
            YamlValue::String(key.to_string()),
            YamlValue::String(value.to_string()),
        );
    }

    Some(mapping)
}

fn is_plain_frontmatter_key_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
}

fn ordered_frontmatter(frontmatter: &Mapping) -> Mapping {
    let mut output = Mapping::new();
    let mut emitted = HashSet::new();

    for key in COMMON_FRONTMATTER_KEYS {
        if let Some(value) = frontmatter.get(*key) {
            output.insert(YamlValue::String((*key).to_string()), value.clone());
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
        output.insert(YamlValue::String(key), value);
    }

    output
}

fn yaml_fragment(value: &YamlValue) -> Result<String> {
    let serialized = serde_norway::to_string(value)
        .map_err(|source| AdapterError::SerializeFrontmatter { source })?;
    let without_start = serialized.strip_prefix("---\n").unwrap_or(&serialized);
    let without_end = without_start.strip_suffix("...\n").unwrap_or(without_start);
    Ok(without_end.to_string())
}
