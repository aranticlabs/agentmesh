use std::fs;
use std::path::{Path, PathBuf};

use agentmesh_adapter_sdk_rust::Adapter;
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
struct RealAdapterRegistry;

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
        "copilot" => {
            let adapter = agentmesh_adapter_copilot::CopilotAdapter;
            call(&adapter).map_err(|source| PipelineError::Adapter {
                runtime: runtime.clone(),
                message: source.to_string(),
            })
        }
        "cursor" => {
            let adapter = agentmesh_adapter_cursor::CursorAdapter;
            call(&adapter).map_err(|source| PipelineError::Adapter {
                runtime: runtime.clone(),
                message: source.to_string(),
            })
        }
        "gemini" => {
            let adapter = agentmesh_adapter_gemini::GeminiAdapter;
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

fn write_seed_file(
    path: &Path,
    contents: impl AsRef<[u8]>,
    action: &'static str,
) -> Result<(), PipelineError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| PipelineError::Io {
            action: "create benchmark fixture directory",
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(path, contents).map_err(|source| PipelineError::Io {
        action,
        path: path.to_path_buf(),
        source,
    })
}

fn seed_repo() -> Result<(TempDir, PathBuf), PipelineError> {
    let temp = TempDir::new().map_err(|source| PipelineError::Io {
        action: "create benchmark temp directory",
        path: std::env::temp_dir(),
        source,
    })?;
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).map_err(|source| PipelineError::Io {
        action: "create benchmark repository",
        path: repo.clone(),
        source,
    })?;
    fs::create_dir_all(repo.join(".codex")).map_err(|source| PipelineError::Io {
        action: "create benchmark Codex runtime directory",
        path: repo.join(".codex"),
        source,
    })?;
    write_seed_file(
        &repo.join(".github/copilot-instructions.md"),
        "Benchmark instructions.\n",
        "write benchmark Copilot instructions",
    )?;
    write_seed_file(
        &repo.join(".github/prompts/performance.prompt.md"),
        "---\ndescription: Performance prompt\nmode: ask\n---\nReview performance.\n",
        "write benchmark Copilot prompt",
    )?;
    write_seed_file(
        &repo.join(".cursor/rules/performance.mdc"),
        "---\ndescription: Performance rule\nalwaysApply: true\n---\nKeep hot paths efficient.\n",
        "write benchmark Cursor rule",
    )?;
    write_seed_file(
        &repo.join("GEMINI.md"),
        "Benchmark instructions.\n",
        "write benchmark Gemini context",
    )?;
    write_seed_file(
        &repo.join(".gemini/commands/performance.toml"),
        "description = \"Performance command\"\nprompt = \"Review performance for {{args}}.\"\n",
        "write benchmark Gemini command",
    )?;
    for index in 0..1000 {
        let slug = format!("skill-{index}");
        let skill_dir = repo.join(".claude/skills").join(&slug);
        fs::create_dir_all(&skill_dir).map_err(|source| PipelineError::Io {
            action: "create benchmark skill directory",
            path: skill_dir.clone(),
            source,
        })?;
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {slug}\n---\nBenchmark skill {index}.\n"),
        )
        .map_err(|source| PipelineError::Io {
            action: "write benchmark skill",
            path: skill_dir.join("SKILL.md"),
            source,
        })?;
    }
    Ok((temp, repo))
}

fn full_sync_benchmark(c: &mut Criterion) {
    c.bench_function("full_sync_1000_entities", |b| {
        b.iter_batched(
            || {
                let fixture = seed_repo().expect("benchmark fixture should be created");
                let registry = RealAdapterRegistry;
                let _summary = sync_with_adapter_registry(
                    &fixture.1,
                    SyncOptions {
                        silent: true,
                        ..SyncOptions::default()
                    },
                    &registry,
                )
                .expect("benchmark setup sync should succeed");
                fixture
            },
            |(_temp, repo)| {
                let registry = RealAdapterRegistry;
                sync_with_adapter_registry(
                    &repo,
                    SyncOptions {
                        silent: true,
                        ..SyncOptions::default()
                    },
                    &registry,
                )
                .expect("benchmark sync should succeed")
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, full_sync_benchmark);
criterion_main!(benches);
