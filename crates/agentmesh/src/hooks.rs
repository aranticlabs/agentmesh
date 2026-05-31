use std::fs;
use std::path::{Path, PathBuf};

use agentmesh_adapter_sdk_rust::Adapter;
use agentmesh_protocol::{InstallHooksRequest, RemoveHooksRequest};

use super::*;

pub(crate) fn print_runtime_install_dry_run(context: &CliContext, runtime: &str) -> Result<()> {
    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    let overlay = match runtime {
        "claude" => ".claude/settings.local.json",
        "codex" => ".codex/hooks.json",
        other => {
            return Err(CliError::new(
                format!("unknown bundled runtime: {other}"),
                AgentmeshExitCode::Usage,
            ));
        }
    };
    if !context.silent {
        println!(
            "{} Would install {runtime} sync hook:",
            context.paint(OutputStyle::Info, "→")
        );
        println!("  Overlay: {}", context.repo_root.join(overlay).display());
        println!(
            "  Command: {} sync --trigger={runtime}-hook --silent",
            binary_path.display()
        );
    }
    Ok(())
}

pub(crate) fn print_git_pre_commit_dry_run(context: &CliContext) -> Result<()> {
    let hook = context.repo_root.join(".git/hooks/pre-commit");
    if !context.silent {
        println!(
            "{} Would install git pre-commit hook at {}",
            context.paint(OutputStyle::Info, "→"),
            hook.display()
        );
        println!("  Command: agentmesh sync --check --trigger=git-pre-commit --silent");
    }
    Ok(())
}

pub(crate) fn print_upgrade_dry_run(context: &CliContext) -> Result<()> {
    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    if !context.silent {
        println!(
            "{} Would repin integrity to {}",
            context.paint(OutputStyle::Info, "→"),
            binary_path.display()
        );
        println!(
            "{} Would rewrite recorded runtime hook entries to the current binary path",
            context.paint(OutputStyle::Info, "→")
        );
    }
    Ok(())
}

pub(crate) fn install_detected_runtime_hooks(context: &CliContext) -> Result<()> {
    let claude = agentmesh_adapter_claude::ClaudeAdapter
        .detect(&context.repo_root)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?;
    if claude.present {
        install_runtime_hook(context, "claude")?;
    }

    let codex = agentmesh_adapter_codex::CodexAdapter
        .detect(&context.repo_root)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?;
    if codex.present {
        install_runtime_hook(context, "codex")?;
    }

    Ok(())
}

pub(crate) fn install_git_pre_commit_hook(context: &CliContext, force: bool) -> Result<()> {
    let hook = context.repo_root.join(GIT_PRE_COMMIT_HOOK);
    let saved = context.repo_root.join(GIT_PRE_COMMIT_SAVED);
    let Some(parent) = hook.parent() else {
        return Err(CliError::new(
            "cannot resolve .git/hooks directory",
            AgentmeshExitCode::Io,
        ));
    };
    if !parent.is_dir() {
        return Err(CliError::new(
            "git hooks directory not found; run from a git worktree",
            AgentmeshExitCode::Usage,
        ));
    }

    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    let existing = match fs::read_to_string(&hook) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(CliError::from_io(error)),
    };
    let existing_mode = if existing.is_some() {
        file_mode(&hook)?
    } else {
        None
    };
    let existing_is_agentmesh = existing
        .as_deref()
        .is_some_and(|content| content.contains(GIT_PRE_COMMIT_MARKER));
    let chain_original = if let Some(content) = existing.as_deref() {
        if existing_is_agentmesh {
            saved.exists()
        } else {
            if let Some(framework) = detect_pre_commit_framework(content) {
                if !force {
                    return Err(CliError::new(
                        format!(
                            "detected {framework} managing pre-commit; add AgentMesh to that framework or rerun with --force"
                        ),
                        AgentmeshExitCode::Usage,
                    ));
                }
            }
            if saved.exists() {
                return Err(CliError::new(
                    format!(
                        "{} already exists; remove it or run uninstall before reinstalling",
                        saved.display()
                    ),
                    AgentmeshExitCode::Usage,
                ));
            }
            write_text_atomic_with_mode(&saved, content, existing_mode)?;
            true
        }
    } else {
        false
    };

    write_text_atomic_with_mode(
        &hook,
        &git_pre_commit_body(&binary_path, chain_original),
        hook_wrapper_mode(existing_mode),
    )?;
    record_git_pre_commit_ownership(context, chain_original)?;

    if !context.silent {
        println!(
            "{} Installed git pre-commit sync check at {}",
            check(context, true),
            hook.display()
        );
    }
    Ok(())
}

fn detect_pre_commit_framework(content: &str) -> Option<&'static str> {
    let body = content
        .lines()
        .filter(|line| !line.starts_with("#!"))
        .collect::<Vec<_>>()
        .join("\n");
    if body.contains("# File generated by pre-commit:")
        || body.contains("pre-commit run --hook-stage")
    {
        Some("pre-commit")
    } else if body.contains("husky.sh") || body.contains("_husky.sh") {
        Some("husky")
    } else if body.contains("lefthook run pre-commit") || body.contains("lefthook install") {
        Some("lefthook")
    } else {
        None
    }
}

fn git_pre_commit_body(binary_path: &Path, chain_original: bool) -> String {
    let original = if chain_original {
        format!(
            "\nif [ -x {} ]; then\n  {} \"$@\" || exit $?\nfi\n",
            shell_quote_path(Path::new(GIT_PRE_COMMIT_SAVED)),
            shell_quote_path(Path::new(GIT_PRE_COMMIT_SAVED))
        )
    } else {
        String::new()
    };
    format!(
        "#!/usr/bin/env bash\n# {GIT_PRE_COMMIT_MARKER} - do not edit directly\n\nset -e\n{original}\n{} sync --check --trigger=git-pre-commit --silent\n",
        shell_quote_path(binary_path)
    )
}

pub(crate) fn install_runtime_hook(context: &CliContext, runtime: &str) -> Result<()> {
    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    let response = match runtime {
        "claude" => agentmesh_adapter_claude::ClaudeAdapter.install_hooks(InstallHooksRequest {
            runtime_dir: context.repo_root.join(".claude"),
            agentmesh_binary_path: binary_path,
            matcher_extra: None,
        }),
        "codex" => agentmesh_adapter_codex::CodexAdapter.install_hooks(InstallHooksRequest {
            runtime_dir: context.repo_root.join(".codex"),
            agentmesh_binary_path: binary_path,
            matcher_extra: None,
        }),
        other => {
            return Err(CliError::new(
                format!("unknown bundled runtime: {other}"),
                AgentmeshExitCode::Usage,
            ));
        }
    }
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))?;

    record_hook_ownership(context, runtime, &response.hooks_installed)?;

    if !context.silent {
        println!(
            "{} Installing {runtime} sync hook:",
            context.paint(OutputStyle::Info, "→")
        );
        for hook in &response.hooks_installed {
            println!(
                "  {} Wrote {} [{}]",
                check(context, true),
                hook.overlay_file.display(),
                hook.entry_path
            );
        }
        println!(
            "  {} Recorded ownership in machine-local cache",
            check(context, true)
        );
        if runtime == "codex" {
            println!(
                "  {} Recommend adding .codex/hooks.json to .gitignore",
                context.paint(OutputStyle::Info, "↗")
            );
            print_codex_trust_prompt(context, &response.hooks_installed);
        }
    }

    Ok(())
}

pub(crate) fn rewrite_installed_runtime_hooks(context: &CliContext) -> Result<()> {
    let layout = cache_layout(&context.repo_root)?;
    let ownership = match agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json) {
        Ok(ownership) => ownership,
        Err(agentmesh_core::state::StateError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(());
        }
        Err(error) => return Err(CliError::new(error.to_string(), AgentmeshExitCode::Io)),
    };

    for runtime in ownership.0.keys() {
        match runtime.as_str() {
            "claude" | "codex" => {
                remove_runtime_hook_entries(context, runtime.as_str())?;
                install_runtime_hook(context, runtime.as_str())?;
            }
            GIT_PRE_COMMIT_RUNTIME => rewrite_git_pre_commit_hook(context)?,
            _ => {}
        }
    }

    Ok(())
}

fn rewrite_git_pre_commit_hook(context: &CliContext) -> Result<()> {
    let hook = context.repo_root.join(GIT_PRE_COMMIT_HOOK);
    let content = match fs::read_to_string(&hook) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(CliError::from_io(error)),
    };
    if !content.contains(GIT_PRE_COMMIT_MARKER) {
        return Ok(());
    }
    let binary_path = std::env::current_exe().map_err(CliError::from_io)?;
    let saved = context.repo_root.join(GIT_PRE_COMMIT_SAVED);
    let existing_mode = file_mode(&hook)?;
    write_text_atomic_with_mode(
        &hook,
        &git_pre_commit_body(&binary_path, saved.exists()),
        hook_wrapper_mode(existing_mode),
    )
}

fn print_codex_trust_prompt(context: &CliContext, hooks: &[agentmesh_protocol::InstalledHook]) {
    if let Some(hook) = hooks.first() {
        println!();
        println!(
            "{} Codex requires you to review and trust new command hooks before they run.",
            context.paint(OutputStyle::Warning, "⚠")
        );
        println!("  What to do:");
        println!("  1. Open Codex in this repository.");
        println!(
            "  2. Run any Codex action that uses a tool, such as a file read or shell command."
        );
        println!("  3. When Codex shows the hook trust prompt, approve this command:");
        println!();
        println!("      {}", hook.command);
        println!();
        println!("  This is a one-time Codex security approval. Until approved, AgentMesh still");
        println!("  syncs via the watcher, Claude hooks, and manual `agentmesh sync`, but Codex");
        println!("  will not run its own hook.");
    }
}

fn record_hook_ownership(
    context: &CliContext,
    runtime: &str,
    hooks: &[agentmesh_protocol::InstalledHook],
) -> Result<()> {
    if hooks.is_empty() {
        return Ok(());
    }
    let runtime_name = agentmesh_core::RuntimeName::new(runtime)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    let layout = cache_layout(&context.repo_root)?;
    layout
        .ensure_dirs()
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?;
    let mut ownership = if layout.hook_ownership_json.exists() {
        agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json)
            .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?
    } else {
        agentmesh_core::state::HookOwnership::default()
    };

    let overlay_file = hooks[0].overlay_file.clone();
    let entry_paths = hooks.iter().map(|hook| hook.entry_path.clone()).collect();
    ownership.0.insert(
        runtime_name,
        agentmesh_core::state::HookOwnershipEntry {
            overlay_file,
            entry_paths,
            installed_at: timestamp_string(),
            installer_version: agentmesh_core::VERSION.to_string(),
        },
    );
    agentmesh_core::state::write_hook_ownership(&layout.hook_ownership_json, &ownership)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))
}

fn record_git_pre_commit_ownership(context: &CliContext, saved_original: bool) -> Result<()> {
    let runtime_name = agentmesh_core::RuntimeName::new(GIT_PRE_COMMIT_RUNTIME)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    let layout = cache_layout(&context.repo_root)?;
    layout
        .ensure_dirs()
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?;
    let mut ownership = if layout.hook_ownership_json.exists() {
        agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json)
            .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?
    } else {
        agentmesh_core::state::HookOwnership::default()
    };

    let mut entry_paths = vec!["agentmesh-wrapper".to_string()];
    if saved_original {
        entry_paths.push(GIT_PRE_COMMIT_SAVED.to_string());
    }
    ownership.0.insert(
        runtime_name,
        agentmesh_core::state::HookOwnershipEntry {
            overlay_file: PathBuf::from(GIT_PRE_COMMIT_HOOK),
            entry_paths,
            installed_at: timestamp_string(),
            installer_version: agentmesh_core::VERSION.to_string(),
        },
    );
    agentmesh_core::state::write_hook_ownership(&layout.hook_ownership_json, &ownership)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))
}

pub(crate) fn uninstall_runtime_hooks(context: &CliContext, dry_run: bool) -> Result<()> {
    let layout = cache_layout(&context.repo_root)?;
    if !layout.hook_ownership_json.exists() {
        if !context.silent {
            println!(
                "{} hook-ownership.json missing. Cannot determine which entries to remove.",
                context.paint(OutputStyle::Warning, "⚠")
            );
        }
        return Ok(());
    }

    let ownership = agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json)
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Io))?;
    if !context.silent {
        println!(
            "{} Removing AgentMesh-owned entries on this machine:",
            context.paint(OutputStyle::Info, "→")
        );
    }

    for (runtime, entry) in ownership.0 {
        if runtime.as_str() == GIT_PRE_COMMIT_RUNTIME {
            uninstall_git_pre_commit_hook(context, &entry, dry_run)?;
            continue;
        }
        if dry_run {
            if !context.silent {
                println!(
                    "    {} Would remove {} hook(s) from {}",
                    context.paint(OutputStyle::Info, "→"),
                    entry.entry_paths.len(),
                    entry.overlay_file.display()
                );
            }
            continue;
        }

        let response =
            remove_runtime_hook_entries_with_paths(context, runtime.as_str(), entry.entry_paths)?;

        if !context.silent {
            if response.ok {
                println!(
                    "    {} Removed {} hook(s) from {}",
                    check(context, true),
                    response.removed_count,
                    entry.overlay_file.display()
                );
            } else if let Some(error) = response.error {
                println!(
                    "    {} {}: {error}",
                    context.paint(OutputStyle::Warning, "⚠"),
                    runtime.as_str()
                );
            }
        }
    }

    Ok(())
}

fn uninstall_git_pre_commit_hook(
    context: &CliContext,
    entry: &agentmesh_core::state::HookOwnershipEntry,
    dry_run: bool,
) -> Result<()> {
    let hook = context.repo_root.join(&entry.overlay_file);
    let saved = context.repo_root.join(GIT_PRE_COMMIT_SAVED);
    if dry_run {
        if !context.silent {
            let action = if saved.exists() { "restore" } else { "remove" };
            println!(
                "    {} Would {action} git pre-commit hook at {}",
                context.paint(OutputStyle::Info, "→"),
                hook.display()
            );
        }
        return Ok(());
    }

    if saved.exists() {
        fs::rename(&saved, &hook).map_err(CliError::from_io)?;
        if !context.silent {
            println!(
                "    {} Restored original git pre-commit hook",
                check(context, true)
            );
        }
        return Ok(());
    }

    match fs::read_to_string(&hook) {
        Ok(content) if content.contains(GIT_PRE_COMMIT_MARKER) => {
            fs::remove_file(&hook).map_err(CliError::from_io)?;
            if !context.silent {
                println!(
                    "    {} Removed git pre-commit hook at {}",
                    check(context, true),
                    hook.display()
                );
            }
        }
        Ok(_) => {
            if !context.silent {
                println!(
                    "    {} Git pre-commit hook changed after install; leaving it untouched",
                    context.paint(OutputStyle::Warning, "⚠")
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(CliError::from_io(error)),
    }
    Ok(())
}

fn remove_runtime_hook_entries(context: &CliContext, runtime: &str) -> Result<()> {
    let layout = cache_layout(&context.repo_root)?;
    let ownership = match agentmesh_core::state::read_hook_ownership(&layout.hook_ownership_json) {
        Ok(ownership) => ownership,
        Err(agentmesh_core::state::StateError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(());
        }
        Err(error) => return Err(CliError::new(error.to_string(), AgentmeshExitCode::Io)),
    };
    let runtime_name = agentmesh_core::RuntimeName::new(runtime.to_string())
        .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Usage))?;
    let Some(entry) = ownership.0.get(&runtime_name) else {
        return Ok(());
    };
    remove_runtime_hook_entries_with_paths(context, runtime, entry.entry_paths.clone()).map(|_| ())
}

fn remove_runtime_hook_entries_with_paths(
    context: &CliContext,
    runtime: &str,
    entry_paths: Vec<String>,
) -> Result<agentmesh_protocol::RemoveHooksResponse> {
    match runtime {
        "claude" => agentmesh_adapter_claude::ClaudeAdapter.remove_hooks(RemoveHooksRequest {
            runtime_dir: context.repo_root.join(".claude"),
            entry_paths,
        }),
        "codex" => agentmesh_adapter_codex::CodexAdapter.remove_hooks(RemoveHooksRequest {
            runtime_dir: context.repo_root.join(".codex"),
            entry_paths,
        }),
        _ => Ok(agentmesh_protocol::RemoveHooksResponse {
            ok: true,
            removed_count: 0,
            error: None,
        }),
    }
    .map_err(|error| CliError::new(error.to_string(), AgentmeshExitCode::Adapter))
}

fn shell_quote_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn write_text_atomic_with_mode(path: &Path, content: &str, mode: Option<u32>) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(CliError::new(
            format!("cannot resolve parent directory for {}", path.display()),
            AgentmeshExitCode::Io,
        ));
    };
    fs::create_dir_all(parent).map_err(CliError::from_io)?;
    let temp = parent.join(format!(".agentmesh-{}.tmp", std::process::id()));
    fs::write(&temp, content).map_err(CliError::from_io)?;
    set_file_mode(&temp, mode)?;
    fs::rename(&temp, path).map_err(CliError::from_io)
}

fn file_mode(path: &Path) -> Result<Option<u32>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::metadata(path).map_err(CliError::from_io)?;
        Ok(Some(metadata.permissions().mode() & 0o777))
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(None)
    }
}

fn hook_wrapper_mode(existing_mode: Option<u32>) -> Option<u32> {
    #[cfg(unix)]
    {
        Some(existing_mode.unwrap_or(0o600) | 0o100)
    }

    #[cfg(not(unix))]
    {
        let _ = existing_mode;
        None
    }
}

fn set_file_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Some(mode) = mode {
            let mut permissions = fs::metadata(path).map_err(CliError::from_io)?.permissions();
            permissions.set_mode(mode);
            fs::set_permissions(path, permissions).map_err(CliError::from_io)?;
        }
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = mode;
    }

    Ok(())
}
