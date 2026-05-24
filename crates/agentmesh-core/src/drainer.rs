//! Core pending-sync drainer loop.

use thiserror::Error;

use crate::mutex::{AgentmeshMutex, MutexError};
use crate::pending_queue::{FailureDisposition, PendingQueue, PendingQueueError};
use crate::state::PendingSyncRecord;

/// Drainer result type.
pub type Result<T> = std::result::Result<T, DrainerError>;

/// Processing result returned by a pending sync processor.
pub type ProcessResult = std::result::Result<(), DrainerProcessError>;

/// Errors produced by the drainer loop.
#[derive(Debug, Error)]
pub enum DrainerError {
    /// Pending queue operation failed.
    #[error(transparent)]
    Queue(#[from] PendingQueueError),
    /// Worker lock operation failed.
    #[error(transparent)]
    Mutex(#[from] MutexError),
}

/// Errors produced by pending record processing.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct DrainerProcessError {
    /// Human-readable failure.
    pub message: String,
}

impl DrainerProcessError {
    /// Creates a processing failure from a message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Processor invoked for each pending sync record.
pub trait PendingSyncProcessor {
    /// Processes a single pending record.
    fn process(&mut self, record: &PendingSyncRecord) -> ProcessResult;
}

impl<F> PendingSyncProcessor for F
where
    F: FnMut(&PendingSyncRecord) -> ProcessResult,
{
    fn process(&mut self, record: &PendingSyncRecord) -> ProcessResult {
        self(record)
    }
}

/// Summary returned after a drainer pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct DrainSummary {
    /// Another drainer already holds the worker lock.
    pub already_running: bool,
    /// Records processed successfully.
    pub processed: usize,
    /// Failed records left queued for a later attempt.
    pub retry_scheduled: usize,
    /// Records moved out of the ready queue after too many failures.
    pub failed: usize,
}

/// Drains ready pending records while holding the worker mutex.
pub fn drain_pending(
    queue: &PendingQueue,
    worker_mutex: &AgentmeshMutex,
    processor: &mut impl PendingSyncProcessor,
) -> Result<DrainSummary> {
    let Some(_guard) = worker_mutex.try_acquire()? else {
        return Ok(DrainSummary {
            already_running: true,
            ..DrainSummary::default()
        });
    };

    let mut summary = DrainSummary::default();
    for queued in queue.read_ready()? {
        match processor.process(&queued.record) {
            Ok(()) => {
                queue.delete(&queued)?;
                summary.processed += 1;
            }
            Err(error) => match queue.record_failure(&queued, &error.to_string())? {
                FailureDisposition::Retry { .. } => summary.retry_scheduled += 1,
                FailureDisposition::Failed { .. } => summary.failed += 1,
            },
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::{DrainerProcessError, drain_pending};
    use crate::mutex::AgentmeshMutex;
    use crate::pending_queue::PendingQueue;
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
    fn drains_successful_records_and_deletes_them() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let queue = PendingQueue::new(temp.path().join("pending-syncs"));
        let worker = AgentmeshMutex::new(temp.path().join("locks/worker"));
        if let Err(error) = queue.enqueue(&record("01HXYZ00000000000000000001")) {
            panic!("enqueue should succeed: {error}");
        }
        let seen = RefCell::new(Vec::new());

        let summary = match drain_pending(&queue, &worker, &mut |record: &PendingSyncRecord| {
            seen.borrow_mut().push(record.pending_id.clone());
            Ok(())
        }) {
            Ok(summary) => summary,
            Err(error) => panic!("drain should succeed: {error}"),
        };

        assert_eq!(summary.processed, 1);
        assert_eq!(seen.into_inner(), vec!["01HXYZ00000000000000000001"]);
        let remaining = match queue.read_ready() {
            Ok(remaining) => remaining,
            Err(error) => panic!("read should succeed: {error}"),
        };
        assert!(remaining.is_empty());
    }

    #[test]
    fn failed_records_stay_queued_for_retry() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let queue = PendingQueue::new(temp.path().join("pending-syncs"));
        let worker = AgentmeshMutex::new(temp.path().join("locks/worker"));
        if let Err(error) = queue.enqueue(&record("01HXYZ00000000000000000001")) {
            panic!("enqueue should succeed: {error}");
        }

        let summary = match drain_pending(&queue, &worker, &mut |_record: &PendingSyncRecord| {
            Err(DrainerProcessError::new("processor failed"))
        }) {
            Ok(summary) => summary,
            Err(error) => panic!("drain should succeed: {error}"),
        };

        assert_eq!(summary.retry_scheduled, 1);
        let remaining = match queue.read_ready() {
            Ok(remaining) => remaining,
            Err(error) => panic!("read should succeed: {error}"),
        };
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].record.attempts, 1);
    }

    #[test]
    fn continues_draining_after_a_record_fails() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let queue = PendingQueue::new(temp.path().join("pending-syncs"));
        let worker = AgentmeshMutex::new(temp.path().join("locks/worker"));
        if let Err(error) = queue.enqueue(&record("01HXYZ00000000000000000001")) {
            panic!("enqueue should succeed: {error}");
        }
        if let Err(error) = queue.enqueue(&record("01HXYZ00000000000000000002")) {
            panic!("enqueue should succeed: {error}");
        }

        let summary = match drain_pending(&queue, &worker, &mut |record: &PendingSyncRecord| {
            if record.pending_id.ends_with("1") {
                Err(DrainerProcessError::new("processor failed"))
            } else {
                Ok(())
            }
        }) {
            Ok(summary) => summary,
            Err(error) => panic!("drain should continue after failures: {error}"),
        };

        assert_eq!(summary.retry_scheduled, 1);
        assert_eq!(summary.processed, 1);
        let remaining = match queue.read_ready() {
            Ok(remaining) => remaining,
            Err(error) => panic!("read should succeed: {error}"),
        };
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].record.pending_id, "01HXYZ00000000000000000001");
    }

    #[test]
    fn worker_contention_reports_already_running() {
        let temp = match tempfile::tempdir() {
            Ok(temp) => temp,
            Err(error) => panic!("tempdir should be available: {error}"),
        };
        let queue = PendingQueue::new(temp.path().join("pending-syncs"));
        let worker = AgentmeshMutex::new(temp.path().join("locks/worker"));
        let guard = match worker.acquire() {
            Ok(guard) => guard,
            Err(error) => panic!("worker lock should acquire: {error}"),
        };

        let summary = match drain_pending(&queue, &worker, &mut |_record: &PendingSyncRecord| {
            Err(DrainerProcessError::new("processor should not run"))
        }) {
            Ok(summary) => summary,
            Err(error) => panic!("drain should report contention cleanly: {error}"),
        };

        drop(guard);
        assert!(summary.already_running);
    }
}
