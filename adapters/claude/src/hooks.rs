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
    let overlay = request.runtime_dir.join("settings.local.json");
    let matcher = append_matcher("Edit|Write|MultiEdit", request.matcher_extra.as_deref());
    let command = format!(
        "{} sync --trigger=claude-hook --silent",
        request.agentmesh_binary_path.display()
    );
    let mut value = read_json_object(&overlay)?;
    let post_tool_use = ensure_hook_array(&mut value, &["hooks", "PostToolUse"])?;

    if let Some(index) = find_hook_group(post_tool_use, &command) {
        return Ok(InstallHooksResponse {
            hooks_installed: vec![InstalledHook {
                overlay_file: workspace_relative(&workspace_root, &overlay)?,
                entry_path: format!("$.hooks.PostToolUse[{index}]"),
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
        }],
    }));
    let index = post_tool_use.len() - 1;
    write_json_pretty(&overlay, &value)?;

    Ok(InstallHooksResponse {
        hooks_installed: vec![InstalledHook {
            overlay_file: workspace_relative(&workspace_root, &overlay)?,
            entry_path: format!("$.hooks.PostToolUse[{index}]"),
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
    let overlay = request.runtime_dir.join("settings.local.json");
    if !overlay.exists() {
        return Ok(RemoveHooksResponse {
            ok: false,
            removed_count: 0,
            error: Some("Claude hook overlay does not exist".to_string()),
        });
    }

    let mut value = read_json_object(&overlay)?;
    let Some(post_tool_use) = find_hook_array_mut(&mut value, &["hooks", "PostToolUse"]) else {
        return Ok(RemoveHooksResponse {
            ok: false,
            removed_count: 0,
            error: Some("Claude PostToolUse hook array not found".to_string()),
        });
    };

    let mut removed = remove_recorded_entries(
        post_tool_use,
        &request.entry_paths,
        "$.hooks.PostToolUse",
        "claude-hook",
    );
    if removed == 0 {
        removed = remove_matching_entries(post_tool_use, "claude-hook");
    }

    if removed == 0 {
        return Ok(RemoveHooksResponse {
            ok: false,
            removed_count: 0,
            error: Some("AgentMesh Claude hook entry not found".to_string()),
        });
    }

    write_json_pretty(&overlay, &value)?;
    Ok(RemoveHooksResponse {
        ok: true,
        removed_count: removed,
        error: None,
    })
}

fn append_matcher(default: &str, extra: Option<&str>) -> String {
    match extra.map(str::trim).filter(|extra| !extra.is_empty()) {
        Some(extra) => format!("{default}|{extra}"),
        None => default.to_string(),
    }
}
