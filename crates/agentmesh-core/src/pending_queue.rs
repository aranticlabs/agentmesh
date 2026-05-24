//! Per-record pending sync queue.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;
use ulid::Ulid;

use crate::state::{PendingSyncRecord, StateError, read_json, write_json};

const MAX_ATTEMPTS: u8 = 3;

/// Pending queue result type.
pub type Result<T> = std::result::Result<T, PendingQueueError>;

/// Errors produced by pending queue operations.
#[derive(Debug, Error)]
pub enum PendingQueueError {
    /// A filesystem operation failed.
    #[error("failed to {action} pending record at {}", path.display())]
    Io {
        /// Operation being performed.
        action: &'static str,
        /// Path involved in the operation.
        path: PathBuf,
        /// Source IO error.
        #[source]
        source: std::io::Error,
    },
    /// State serialization or parsing failed.
    #[error(transparent)]
    State(#[from] StateError),
}

/// One pending record read from disk with its source path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedRecord {
    /// Path to the pending record JSON file.
    pub path: PathBuf,
    /// Parsed pending sync record.
    pub record: PendingSyncRecord,
}

/// Queue rooted at `pending-syncs/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingQueue {
    dir: PathBuf,
}

impl PendingQueue {
    /// Creates a queue wrapper for a pending-sync directory.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Returns the queue directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Generates a monotonic pending record ID.
    #[must_use]
    pub fn new_pending_id() -> String {
        Ulid::new().to_string()
    }

    /// Writes one pending record atomically.
    pub fn enqueue(&self, record: &PendingSyncRecord) -> Result<PathBuf> {
        fs::create_dir_all(&self.dir).map_err(|source| PendingQueueError::Io {
            action: "create directory for",
            path: self.dir.clone(),
            source,
        })?;
        let path = self.record_path(&record.pending_id);
        write_json(&path, record)?;
        Ok(path)
    }

    /// Reads ready records in ULID filename order.
    pub fn read_ready(&self) -> Result<Vec<QueuedRecord>> {
        let mut paths = Vec::new();
        match fs::read_dir(&self.dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|source| PendingQueueError::Io {
                        action: "read directory entry for",
                        path: self.dir.clone(),
                        source,
                    })?;
                    let path = entry.path();
                    if is_ready_record_path(&path) {
                        paths.push(path);
                    }
                }
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(PendingQueueError::Io {
                    action: "read directory for",
                    path: self.dir.clone(),
                    source,
                });
            }
        }
        paths.sort();

        paths
            .into_iter()
            .map(|path| {
                let record = read_json(&path)?;
                Ok(QueuedRecord { path, record })
            })
            .collect()
    }

    /// Deletes a processed pending record.
    pub fn delete(&self, queued: &QueuedRecord) -> Result<()> {
        match fs::remove_file(&queued.path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(PendingQueueError::Io {
                action: "delete",
                path: queued.path.clone(),
                source,
            }),
        }
    }

    /// Records a failed processing attempt and marks the record failed after three attempts.
    pub fn record_failure(
        &self,
        queued: &QueuedRecord,
        error_message: &str,
    ) -> Result<FailureDisposition> {
        let mut record = queued.record.clone();
        record.attempts = record.attempts.saturating_add(1);
        record.last_error = Some(error_message.to_string());

        if record.attempts >= MAX_ATTEMPTS {
            write_json(&queued.path, &record)?;
            let failed_path = queued.path.with_file_name(format!(
                "failed-{}",
                queued
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("pending.json")
            ));
            fs::rename(&queued.path, &failed_path).map_err(|source| PendingQueueError::Io {
                action: "mark failed",
                path: queued.path.clone(),
                source,
            })?;
            Ok(FailureDisposition::Failed { path: failed_path })
        } else {
            write_json(&queued.path, &record)?;
            Ok(FailureDisposition::Retry {
                attempts: record.attempts,
            })
        }
    }

    fn record_path(&self, pending_id: &str) -> PathBuf {
        self.dir.join(format!("{pending_id}.json"))
    }
}

/// Outcome of recording a processing failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureDisposition {
    /// Record remains queued for another attempt.
    Retry {
        /// Number of failed attempts recorded so far.
        attempts: u8,
    },
    /// Record was renamed out of the ready queue.
    Failed {
        /// Failed record path.
        path: PathBuf,
    },
}

fn is_ready_record_path(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("json")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| !name.starts_with("failed-"))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::{FailureDisposition, PendingQueue};
    use crate::state::{PendingAction, PendingSyncRecord};
    use crate::types::RuntimeName;

    fn runtime_name(value: &str) -> RuntimeName {
        match RuntimeName::new(value) {
            Ok(runtime) => runtime,
            Err(error) => panic!("runtime name should be valid: {error}"),
        }
    }

    fn record(pending_id: &str) -> PendingSyncRecord {
        PendingSyncRecord {
            pending_id: pending_id.to_string(),
            source_runtime: runtime_name("claude"),
            action: PendingAction::Write,
            entity_type: None,
            entity_root: PathBuf::from(".claude/skills/demo"),
            changed_paths: vec![PathBuf::from(".claude/skills/demo/SKILL.md")],
            rename_from: None,
            content_hashes: BTreeMap::new(),
            mtime: "2026-05-24T14:32:11.123Z".to_string(),
            trigger: "claude-hook".to_string(),
            created_at: "2026-05-24T14:32:11.130Z".to_string(),
            attempts: 0,
            last_error: None,
        }
    }

    #[test]
    fn reads_records_in_filename_order() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let queue = PendingQueue::new(temp.path());

        if let Err(error) = queue.enqueue(&record("01HXYZ00000000000000000002")) {
            panic!("enqueue should succeed: {error}");
        }
        if let Err(error) = queue.enqueue(&record("01HXYZ00000000000000000001")) {
            panic!("enqueue should succeed: {error}");
        }

        let records = match queue.read_ready() {
            Ok(records) => records,
            Err(error) => panic!("read should succeed: {error}"),
        };
        let ids = records
            .iter()
            .map(|queued| queued.record.pending_id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            ids,
            vec!["01HXYZ00000000000000000001", "01HXYZ00000000000000000002"]
        );
    }

    #[test]
    fn deletes_processed_record() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let queue = PendingQueue::new(temp.path());
        if let Err(error) = queue.enqueue(&record("01HXYZ00000000000000000001")) {
            panic!("enqueue should succeed: {error}");
        }
        let records = match queue.read_ready() {
            Ok(records) => records,
            Err(error) => panic!("read should succeed: {error}"),
        };

        if let Err(error) = queue.delete(&records[0]) {
            panic!("delete should succeed: {error}");
        }

        let records = match queue.read_ready() {
            Ok(records) => records,
            Err(error) => panic!("read should succeed: {error}"),
        };
        assert!(records.is_empty());
    }

    #[test]
    fn retries_then_marks_failed() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let queue = PendingQueue::new(temp.path());
        if let Err(error) = queue.enqueue(&record("01HXYZ00000000000000000001")) {
            panic!("enqueue should succeed: {error}");
        }

        for expected_attempts in [1, 2] {
            let records = match queue.read_ready() {
                Ok(records) => records,
                Err(error) => panic!("read should succeed: {error}"),
            };
            let disposition = match queue.record_failure(&records[0], "test failure") {
                Ok(disposition) => disposition,
                Err(error) => panic!("failure should record: {error}"),
            };
            assert_eq!(
                disposition,
                FailureDisposition::Retry {
                    attempts: expected_attempts
                }
            );
        }

        let records = match queue.read_ready() {
            Ok(records) => records,
            Err(error) => panic!("read should succeed: {error}"),
        };
        let disposition = match queue.record_failure(&records[0], "test failure") {
            Ok(disposition) => disposition,
            Err(error) => panic!("failure should record: {error}"),
        };

        assert!(matches!(disposition, FailureDisposition::Failed { .. }));
        let records = match queue.read_ready() {
            Ok(records) => records,
            Err(error) => panic!("read should succeed: {error}"),
        };
        assert!(records.is_empty());
    }
}
