use super::*;

pub(crate) fn install_hooks(
    request: InstallHooksRequest,
) -> agentmesh_adapter_sdk_rust::Result<InstallHooksResponse> {
    if !request.agentmesh_binary_path.is_absolute() {
        return Err(AdapterError::rpc(
            AdapterErrorCode::HookInstallFailed,
            "agentmesh_binary_path must be absolute",
        ));
    }

    let workspace_root = workspace_root_for(&request.runtime_dir)?;
    let overlay = request.runtime_dir.join("hooks.json");
    let matcher = codex_matcher(request.matcher_extra.as_deref());
    let command = format!(
        "{} sync --trigger=codex-hook --silent",
        request.agentmesh_binary_path.display()
    );
    let mut value = read_json_object(&overlay)?;
    let post_tool_use = ensure_hook_array(&mut value, &["PostToolUse"])?;

    if let Some(index) = find_hook_group(post_tool_use, &command) {
        return Ok(InstallHooksResponse {
            hooks_installed: vec![InstalledHook {
                overlay_file: workspace_relative(&workspace_root, &overlay)?,
                entry_path: format!("$.PostToolUse[{index}]"),
                command,
                matcher,
            }],
            fallback_needed: false,
            fallback_reason: None,
        });
    }

    post_tool_use.push(json!({
        "matcher": matcher,
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": 5,
            "statusMessage": "AgentMesh sync",
        }],
    }));
    let index = post_tool_use.len() - 1;
    write_json_pretty(&overlay, &value)?;

    Ok(InstallHooksResponse {
        hooks_installed: vec![InstalledHook {
            overlay_file: workspace_relative(&workspace_root, &overlay)?,
            entry_path: format!("$.PostToolUse[{index}]"),
            command,
            matcher,
        }],
        fallback_needed: false,
        fallback_reason: None,
    })
}

pub(crate) fn remove_hooks(
    request: RemoveHooksRequest,
) -> agentmesh_adapter_sdk_rust::Result<RemoveHooksResponse> {
    let overlay = request.runtime_dir.join("hooks.json");
    if !overlay.exists() {
        return Ok(RemoveHooksResponse {
            ok: false,
            removed_count: 0,
            error: Some("Codex hook overlay does not exist".to_string()),
        });
    }

    let mut value = read_json_object(&overlay)?;
    let removed = {
        let Some(post_tool_use) = find_hook_array_mut(&mut value, &["PostToolUse"]) else {
            return Ok(RemoveHooksResponse {
                ok: false,
                removed_count: 0,
                error: Some("Codex PostToolUse hook array not found".to_string()),
            });
        };

        let mut removed = remove_recorded_entries(
            post_tool_use,
            &request.entry_paths,
            "$.PostToolUse",
            "codex-hook",
        );
        if removed == 0 {
            removed = remove_matching_entries(post_tool_use, "codex-hook");
        }
        removed
    };

    if removed == 0 {
        return Ok(RemoveHooksResponse {
            ok: false,
            removed_count: 0,
            error: Some("AgentMesh Codex hook entry not found".to_string()),
        });
    }

    if codex_hooks_are_empty(&value) {
        fs::remove_file(&overlay).map_err(|source| AdapterError::Io {
            action: "remove file",
            path: overlay.clone(),
            source,
        })?;
    } else {
        write_json_pretty(&overlay, &value)?;
    }

    Ok(RemoveHooksResponse {
        ok: true,
        removed_count: removed,
        error: None,
    })
}

fn codex_matcher(extra: Option<&str>) -> String {
    let mut tools = vec!["Edit", "Write", "MultiEdit"];
    if let Some(extra) = extra.map(str::trim).filter(|extra| !extra.is_empty()) {
        tools.push(extra);
    }
    format!("^({})$", tools.join("|"))
}

fn codex_hooks_are_empty(value: &JsonValue) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object
        .iter()
        .all(|(key, value)| key == "PostToolUse" && value.as_array().is_some_and(Vec::is_empty))
}
