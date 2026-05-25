use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use agentmesh_adapter_sdk_rust::Adapter;
use agentmesh_core::{
    AdapterRegistry, RuntimeName, SyncOptions,
    pipeline::{PipelineError, Result as PipelineResult},
    sync_with_adapter_registry,
};
use agentmesh_protocol::{
    DetectResponse, EmitRequest, EmitResponse, ImportRequest, ImportResponse,
};
use tempfile::TempDir;

const DEFAULT_FULL_SYNC_BUDGET: Duration = Duration::from_secs(10);
const DEFAULT_HOOK_P99_BUDGET: Duration = Duration::from_millis(100);
static PERFORMANCE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Copy)]
struct RealAdapterRegistry;

#[derive(Debug, Clone, Copy)]
struct EmptyRegistry;

impl AdapterRegistry for RealAdapterRegistry {
    fn detect(&self, runtime: &RuntimeName, repo_root: &Path) -> PipelineResult<DetectResponse> {
        with_runtime_adapter(runtime, |adapter| adapter.detect(repo_root))
    }

    fn import(
        &self,
        runtime: &RuntimeName,
        _repo_root: &Path,
        request: ImportRequest,
    ) -> PipelineResult<ImportResponse> {
        with_runtime_adapter(runtime, |adapter| adapter.import(request))
    }

    fn emit(
        &self,
        runtime: &RuntimeName,
        _repo_root: &Path,
        request: EmitRequest,
    ) -> PipelineResult<EmitResponse> {
        with_runtime_adapter(runtime, |adapter| adapter.emit(request))
    }
}

impl AdapterRegistry for EmptyRegistry {
    fn detect(&self, _runtime: &RuntimeName, _repo_root: &Path) -> PipelineResult<DetectResponse> {
        Ok(DetectResponse {
            present: false,
            version: None,
            files: Vec::new(),
        })
    }

    fn import(
        &self,
        _runtime: &RuntimeName,
        _repo_root: &Path,
        _request: ImportRequest,
    ) -> PipelineResult<ImportResponse> {
        Ok(ImportResponse {
            entities: Vec::new(),
            skipped: Vec::new(),
        })
    }

    fn emit(
        &self,
        _runtime: &RuntimeName,
        _repo_root: &Path,
        _request: EmitRequest,
    ) -> PipelineResult<EmitResponse> {
        Ok(EmitResponse {
            files_written: Vec::new(),
            skipped: Vec::new(),
            partial_fidelity: Vec::new(),
        })
    }
}

fn with_runtime_adapter<T>(
    runtime: &RuntimeName,
    call: impl FnOnce(&dyn Adapter) -> agentmesh_adapter_sdk_rust::Result<T>,
) -> PipelineResult<T> {
    match runtime.as_str() {
        "claude" => {
            let adapter = agentmesh_adapter_claude::ClaudeAdapter;
            call(&adapter).map_err(|source| PipelineError::Adapter {
                runtime: runtime.clone(),
                message: source.to_string(),
            })
        }
        "codex" => {
            let adapter = agentmesh_adapter_codex::CodexAdapter;
            call(&adapter).map_err(|source| PipelineError::Adapter {
                runtime: runtime.clone(),
                message: source.to_string(),
            })
        }
        _ => Err(PipelineError::Adapter {
            runtime: runtime.clone(),
            message: "unknown runtime adapter".to_string(),
        }),
    }
}

fn seed_repo() -> Result<(TempDir, PathBuf), PipelineError> {
    let temp = TempDir::new().map_err(|source| PipelineError::Io {
        action: "create performance temp directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).map_err(|source| PipelineError::Io {
        action: "create performance repository",
        path: repo.clone(),
        source,
    })?;
    fs::create_dir_all(repo.join(".codex")).map_err(|source| PipelineError::Io {
        action: "create performance Codex runtime directory",
        path: repo.join(".codex"),
        source,
    })?;
    for index in 0..1000 {
        let slug = format!("skill-{index}");
        let skill_dir = repo.join(".claude/skills").join(&slug);
        fs::create_dir_all(&skill_dir).map_err(|source| PipelineError::Io {
            action: "create performance skill directory",
            path: skill_dir.clone(),
            source,
        })?;
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {slug}\n---\nPerformance skill {index}.\n"),
        )
        .map_err(|source| PipelineError::Io {
            action: "write performance skill",
            path: skill_dir.join("SKILL.md"),
            source,
        })?;
    }
    Ok((temp, repo))
}

fn full_sync_budget() -> Duration {
    std::env::var("AGENTMESH_FULL_SYNC_BUDGET_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_FULL_SYNC_BUDGET)
}

fn hook_p99_budget() -> Duration {
    std::env::var("AGENTMESH_HOOK_P99_BUDGET_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_HOOK_P99_BUDGET)
}

fn performance_test_guard() -> MutexGuard<'static, ()> {
    PERFORMANCE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn seed_hook_repo(entity_count: usize) -> Result<(TempDir, PathBuf), PipelineError> {
    let temp = TempDir::new().map_err(|source| PipelineError::Io {
        action: "create hook performance temp directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join(".ai/skills")).map_err(|source| PipelineError::Io {
        action: "create hook performance skill root",
        path: repo.join(".ai/skills"),
        source,
    })?;
    fs::write(repo.join("AGENTS.md"), "Performance instructions\n").map_err(|source| {
        PipelineError::Io {
            action: "write hook performance instructions",
            path: repo.join("AGENTS.md"),
            source,
        }
    })?;

    for index in 0..entity_count {
        let skill_dir = repo.join(format!(".ai/skills/skill-{index}"));
        fs::create_dir_all(&skill_dir).map_err(|source| PipelineError::Io {
            action: "create hook performance skill",
            path: skill_dir.clone(),
            source,
        })?;
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: skill-{index}\n---\nPerformance skill {index}.\n"),
        )
        .map_err(|source| PipelineError::Io {
            action: "write hook performance skill",
            path: skill_dir.join("SKILL.md"),
            source,
        })?;
    }

    Ok((temp, repo))
}

#[test]
#[ignore = "CI runs this on the reference Linux runner"]
fn full_sync_1000_entities_stays_under_budget() {
    let _guard = performance_test_guard();
    let (_temp, repo) = seed_repo().expect("performance fixture should be created");
    let registry = RealAdapterRegistry;
    let _initial_summary = sync_with_adapter_registry(
        &repo,
        SyncOptions {
            silent: true,
            ..SyncOptions::default()
        },
        &registry,
    )
    .expect("performance setup sync should succeed");

    let started = Instant::now();
    let _summary = sync_with_adapter_registry(
        &repo,
        SyncOptions {
            silent: true,
            ..SyncOptions::default()
        },
        &registry,
    )
    .expect("performance sync should succeed");
    let elapsed = started.elapsed();
    let budget = full_sync_budget();

    assert!(
        elapsed <= budget,
        "full sync took {elapsed:?}, budget is {budget:?}"
    );
}

#[test]
#[ignore = "CI runs this on the reference Linux runner"]
fn hook_trigger_p99_stays_under_budget() {
    let _guard = performance_test_guard();
    const SAMPLES_PER_WINDOW: usize = 100;
    const WINDOWS: usize = 3;
    const WARMUP_SAMPLES: usize = 10;
    let (_temp, repo) = seed_hook_repo(150).expect("hook performance fixture should be created");
    let _initial_summary = sync_with_adapter_registry(
        &repo,
        SyncOptions {
            trigger: Some("claude-hook".to_string()),
            silent: true,
            ..SyncOptions::default()
        },
        &EmptyRegistry,
    )
    .expect("hook performance setup sync should succeed");

    for _ in 0..WARMUP_SAMPLES {
        let _summary = sync_with_adapter_registry(
            &repo,
            SyncOptions {
                trigger: Some("claude-hook".to_string()),
                silent: true,
                ..SyncOptions::default()
            },
            &EmptyRegistry,
        )
        .expect("hook performance sync should succeed");
    }

    let mut p99_windows = Vec::with_capacity(WINDOWS);
    for _ in 0..WINDOWS {
        let mut samples = Vec::with_capacity(SAMPLES_PER_WINDOW);
        for _ in 0..SAMPLES_PER_WINDOW {
            let started = Instant::now();
            let _summary = sync_with_adapter_registry(
                &repo,
                SyncOptions {
                    trigger: Some("claude-hook".to_string()),
                    silent: true,
                    ..SyncOptions::default()
                },
                &EmptyRegistry,
            )
            .expect("hook performance sync should succeed");
            samples.push(started.elapsed());
        }
        samples.sort_unstable();
        let p99_index = ((SAMPLES_PER_WINDOW * 99).div_ceil(100)).saturating_sub(1);
        p99_windows.push(samples[p99_index]);
    }
    p99_windows.sort_unstable();
    let p99 = p99_windows[WINDOWS / 2];
    let budget = hook_p99_budget();

    assert!(
        p99 <= budget,
        "hook-triggered sync p99 was {p99:?}, budget is {budget:?}"
    );
}
