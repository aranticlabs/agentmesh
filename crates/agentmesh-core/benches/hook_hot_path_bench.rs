use std::fs;
use std::path::Path;

use agentmesh_core::{
    AdapterRegistry, RuntimeName, SyncOptions,
    pipeline::{PipelineError, Result as PipelineResult},
    sync_with_adapter_registry,
};
use agentmesh_protocol::{
    DetectResponse, EmitRequest, EmitResponse, ImportRequest, ImportResponse,
};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use tempfile::TempDir;

#[derive(Debug, Clone, Copy)]
struct EmptyRegistry;

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

fn seed_repo() -> Result<(TempDir, std::path::PathBuf), PipelineError> {
    let temp = TempDir::new().map_err(|source| PipelineError::Io {
        action: "create benchmark temp directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join(".ai/skills")).map_err(|source| PipelineError::Io {
        action: "create benchmark skill directory",
        path: repo.join(".ai/skills"),
        source,
    })?;
    fs::write(repo.join("AGENTS.md"), "Benchmark instructions\n").map_err(|source| {
        PipelineError::Io {
            action: "write benchmark instructions",
            path: repo.join("AGENTS.md"),
            source,
        }
    })?;
    for index in 0..150 {
        let skill_dir = repo.join(format!(".ai/skills/skill-{index}"));
        fs::create_dir_all(&skill_dir).map_err(|source| PipelineError::Io {
            action: "create benchmark skill",
            path: skill_dir.clone(),
            source,
        })?;
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: skill-{index}\n---\nBenchmark skill {index}.\n"),
        )
        .map_err(|source| PipelineError::Io {
            action: "write benchmark skill",
            path: skill_dir.join("SKILL.md"),
            source,
        })?;
    }
    Ok((temp, repo))
}

fn hook_hot_path_benchmark(c: &mut Criterion) {
    c.bench_function("hook_trigger_sync_150_entities", |b| {
        b.iter_batched(
            || seed_repo().expect("benchmark fixture should be created"),
            |(_temp, repo)| {
                sync_with_adapter_registry(
                    &repo,
                    SyncOptions {
                        trigger: Some("claude-hook".to_string()),
                        silent: true,
                        ..SyncOptions::default()
                    },
                    &EmptyRegistry,
                )
                .expect("benchmark sync should succeed")
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, hook_hot_path_benchmark);
criterion_main!(benches);
