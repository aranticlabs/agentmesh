//! Machine-local state data structures.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::EntityType;
use crate::types::{EntityId, Hash, RuntimeName};

/// Machine-local state result type.
pub type Result<T> = std::result::Result<T, StateError>;

/// Errors produced while reading or writing machine-local state.
#[derive(Debug, Error)]
pub enum StateError {
    /// A filesystem operation failed.
    #[error("failed to {action} at {}", path.display())]
    Io {
        /// Operation being performed.
        action: &'static str,
        /// Path involved in the operation.
        path: PathBuf,
        /// Source IO error.
        #[source]
        source: std::io::Error,
    },
    /// JSON serialization failed.
    #[error("failed to serialize JSON for {}", path.display())]
    SerializeJson {
        /// Path involved in the operation.
        path: PathBuf,
        /// Source serialization error.
        #[source]
        source: serde_json::Error,
    },
    /// JSON deserialization failed.
    #[error("failed to parse JSON at {}", path.display())]
    DeserializeJson {
        /// Path involved in the operation.
        path: PathBuf,
        /// Source parse error.
        #[source]
        source: serde_json::Error,
    },
    /// Hash validation failed.
    #[error(transparent)]
    InvalidHash(#[from] crate::types::TypeError),
}

/// Machine-local cache paths for one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct CacheLayout {
    /// Root directory for this repository's machine-local state.
    pub root: PathBuf,
    /// Binary integrity pin path.
    pub integrity_json: PathBuf,
    /// Hook ownership path.
    pub hook_ownership_json: PathBuf,
    /// Watcher PID path.
    pub watcher_pid: PathBuf,
    /// Watcher log path.
    pub watcher_log: PathBuf,
    /// Directory containing advisory lock files.
    pub locks_dir: PathBuf,
    /// Short-held state mutex path.
    pub state_lock: PathBuf,
    /// Long-held worker mutex path.
    pub worker_lock: PathBuf,
    /// Full-sync mutex path.
    pub sync_in_progress_lock: PathBuf,
    /// Directory containing pending sync records.
    pub pending_syncs_dir: PathBuf,
    /// Pending reviewed diff state path.
    pub pending_diff_json: PathBuf,
    /// Directory containing preserved conflict versions.
    pub conflicts_dir: PathBuf,
    /// Directory containing cached parsed representations.
    pub parsed_dir: PathBuf,
    /// Directory containing adapter scratch state.
    pub adapter_state_dir: PathBuf,
}

impl CacheLayout {
    /// Creates cache paths under an explicit cache root.
    pub fn new(cache_root: &Path, repo_root: &Path) -> Result<Self> {
        let root = cache_root.join(repo_cache_key(repo_root)?);
        let locks_dir = root.join("locks");

        Ok(Self {
            integrity_json: root.join("integrity.json"),
            hook_ownership_json: root.join("hook-ownership.json"),
            watcher_pid: root.join("watcher.pid"),
            watcher_log: root.join("watcher.log"),
            state_lock: locks_dir.join("state"),
            worker_lock: locks_dir.join("worker"),
            sync_in_progress_lock: locks_dir.join("sync_in_progress"),
            pending_syncs_dir: root.join("pending-syncs"),
            pending_diff_json: root.join("pending-diff.json"),
            conflicts_dir: root.join("conflicts"),
            parsed_dir: root.join("parsed"),
            adapter_state_dir: root.join("adapter-state"),
            root,
            locks_dir,
        })
    }

    /// Creates every directory the cache layout uses.
    pub fn ensure_dirs(&self) -> Result<()> {
        for path in [
            &self.root,
            &self.locks_dir,
            &self.pending_syncs_dir,
            &self.conflicts_dir,
            &self.parsed_dir,
            &self.adapter_state_dir,
        ] {
            fs::create_dir_all(path).map_err(|source| StateError::Io {
                action: "create directory",
                path: path.clone(),
                source,
            })?;
        }

        Ok(())
    }
}

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
    /// Number of failed processing attempts.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempts: u8,
    /// Last processing error, when the record has failed at least once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
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

fn is_zero(value: &u8) -> bool {
    *value == 0
}

/// Computes the repository cache key from the absolute repository path.
pub fn repo_cache_key(repo_root: &Path) -> Result<String> {
    let absolute = absolute_path(repo_root)?;
    let digest = blake3::hash(&path_bytes(absolute.as_os_str()));
    Ok(digest.to_hex().chars().take(16).collect())
}

/// Returns the cache directory used for preserved versions of one entity.
#[must_use]
pub fn conflict_entity_dir(conflicts_dir: &Path, entity_id: &EntityId) -> PathBuf {
    conflicts_dir.join(entity_cache_segment(entity_id))
}

/// Returns the file name used for one preserved conflict version.
#[must_use]
pub fn conflict_version_file_name(runtime: &RuntimeName, timestamp: &str) -> String {
    format!(
        "{}-{}.md",
        runtime.as_str(),
        conflict_timestamp_file_segment(timestamp)
    )
}

/// Returns the cache-safe timestamp segment used in preserved conflict filenames.
#[must_use]
pub fn conflict_timestamp_file_segment(timestamp: &str) -> String {
    timestamp.replace(':', "-")
}

fn entity_cache_segment(entity_id: &EntityId) -> String {
    entity_id.as_str().replace(':', "--")
}

/// Computes a SHA-256 hash over in-memory bytes.
pub fn sha256_bytes(bytes: &[u8]) -> Result<Hash> {
    let digest = Sha256::digest(bytes);
    Hash::new(hex_lower(digest.as_ref())).map_err(StateError::from)
}

/// Computes a SHA-256 hash over a file's bytes.
pub fn sha256_file(path: &Path) -> Result<Hash> {
    let bytes = fs::read(path).map_err(|source| StateError::Io {
        action: "read file",
        path: path.to_path_buf(),
        source,
    })?;
    sha256_bytes(&bytes)
}

/// Reads a JSON state file.
pub fn read_json<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = fs::read(path).map_err(|source| StateError::Io {
        action: "read file",
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| StateError::DeserializeJson {
        path: path.to_path_buf(),
        source,
    })
}

/// Writes a JSON state file atomically.
pub fn write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| StateError::SerializeJson {
        path: path.to_path_buf(),
        source,
    })?;
    write_atomic(path, &bytes)
}

/// Reads an integrity pin.
pub fn read_integrity_pin(path: &Path) -> Result<IntegrityPin> {
    read_json(path)
}

/// Writes an integrity pin atomically.
pub fn write_integrity_pin(path: &Path, pin: &IntegrityPin) -> Result<()> {
    write_json(path, pin)
}

/// Reads hook ownership records.
pub fn read_hook_ownership(path: &Path) -> Result<HookOwnership> {
    read_json(path)
}

/// Writes hook ownership records atomically.
pub fn write_hook_ownership(path: &Path, ownership: &HookOwnership) -> Result<()> {
    write_json(path, ownership)
}

pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(StateError::Io {
            action: "resolve parent directory",
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path has no parent directory",
            ),
        });
    };

    fs::create_dir_all(parent).map_err(|source| StateError::Io {
        action: "create directory",
        path: parent.to_path_buf(),
        source,
    })?;

    let mut temp = NamedTempFile::new_in(parent).map_err(|source| StateError::Io {
        action: "create temporary file",
        path: parent.to_path_buf(),
        source,
    })?;
    temp.write_all(contents).map_err(|source| StateError::Io {
        action: "write temporary file",
        path: path.to_path_buf(),
        source,
    })?;
    temp.as_file_mut()
        .sync_all()
        .map_err(|source| StateError::Io {
            action: "sync temporary file",
            path: path.to_path_buf(),
            source,
        })?;
    temp.persist(path).map_err(|error| StateError::Io {
        action: "replace file",
        path: path.to_path_buf(),
        source: error.error,
    })?;

    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        let current_dir = std::env::current_dir().map_err(|source| StateError::Io {
            action: "read current directory",
            path: PathBuf::from("."),
            source,
        })?;
        Ok(current_dir.join(path))
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(unix)]
fn path_bytes(path: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_bytes().to_vec()
}

#[cfg(windows)]
fn path_bytes(path: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    path.encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>()
}

#[cfg(not(any(unix, windows)))]
fn path_bytes(path: &OsStr) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        CacheLayout, HookOwnership, conflict_version_file_name, repo_cache_key, sha256_bytes,
        write_json,
    };
    use crate::types::RuntimeName;

    fn runtime_name(value: &str) -> RuntimeName {
        match RuntimeName::new(value) {
            Ok(runtime) => runtime,
            Err(error) => panic!("runtime name should be valid: {error}"),
        }
    }

    #[test]
    fn cache_key_is_stable_for_the_same_path() {
        let first = repo_cache_key(Path::new("/tmp/agentmesh-demo"));
        let second = repo_cache_key(Path::new("/tmp/agentmesh-demo"));

        assert_eq!(first.ok(), second.ok());
    }

    #[test]
    fn cache_layout_uses_expected_repository_subpaths() {
        let layout = match CacheLayout::new(Path::new("/tmp/cache"), Path::new("/tmp/repo")) {
            Ok(layout) => layout,
            Err(error) => panic!("cache layout should build: {error}"),
        };

        assert!(layout.integrity_json.ends_with("integrity.json"));
        assert!(layout.state_lock.ends_with("locks/state"));
        assert!(layout.pending_syncs_dir.ends_with("pending-syncs"));
    }

    #[test]
    fn conflict_version_file_names_are_cache_safe() {
        assert_eq!(
            conflict_version_file_name(&runtime_name("codex"), "2026-05-24T14:32:11Z"),
            "codex-2026-05-24T14-32-11Z.md"
        );
    }

    #[test]
    fn sha256_hashes_bytes() {
        let hash = match sha256_bytes(b"agentmesh") {
            Ok(hash) => hash,
            Err(error) => panic!("hashing should succeed: {error}"),
        };

        assert_eq!(
            hash.as_str(),
            "3f584baa09d4137b21b3f1cacdab0be79c2004ce602a3b0a6414f42747837aaa"
        );
    }

    #[test]
    fn writes_json_atomically() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let path = temp.path().join("hook-ownership.json");
        let ownership = HookOwnership::default();

        if let Err(error) = write_json(&path, &ownership) {
            panic!("json write should succeed: {error}");
        }

        assert!(path.exists());
    }

    #[test]
    fn cache_layout_can_be_recreated_after_deletion() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let cache_root = temp.path().join("cache");
        let repo_root = temp.path().join("repo");
        let layout = match CacheLayout::new(&cache_root, &repo_root) {
            Ok(layout) => layout,
            Err(error) => panic!("cache layout should build: {error}"),
        };
        if let Err(error) = layout.ensure_dirs() {
            panic!("cache dirs should be created: {error}");
        }
        if let Err(error) = std::fs::remove_dir_all(&layout.root) {
            panic!("cache root should be removable: {error}");
        }

        let recreated = match CacheLayout::new(&cache_root, &repo_root) {
            Ok(layout) => layout,
            Err(error) => panic!("cache layout should rebuild: {error}"),
        };
        if let Err(error) = recreated.ensure_dirs() {
            panic!("cache dirs should be recreated: {error}");
        }

        assert!(recreated.pending_syncs_dir.is_dir());
        assert!(recreated.conflicts_dir.is_dir());
        assert!(recreated.adapter_state_dir.is_dir());
    }
}
