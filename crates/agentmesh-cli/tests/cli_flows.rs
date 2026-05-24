use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

fn agentmesh_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agentmesh"))
}

fn run_agentmesh(repo: &Path, cache: &Path, args: &[&str]) -> Output {
    match Command::new(agentmesh_bin())
        .arg("--cwd")
        .arg(repo)
        .env("AGENTMESH_CACHE_DIR", cache)
        .args(args)
        .output()
    {
        Ok(output) => output,
        Err(error) => panic!("agentmesh command should run: {error}"),
    }
}

fn run_agentmesh_with_home(repo: &Path, cache: &Path, home: &Path, args: &[&str]) -> Output {
    match Command::new(agentmesh_bin())
        .arg("--cwd")
        .arg(repo)
        .env("AGENTMESH_CACHE_DIR", cache)
        .env("HOME", home)
        .args(args)
        .output()
    {
        Ok(output) => output,
        Err(error) => panic!("agentmesh command should run: {error}"),
    }
}

fn spawn_agentmesh_with_home(repo: &Path, cache: &Path, home: &Path, args: &[&str]) -> Child {
    match Command::new(agentmesh_bin())
        .arg("--cwd")
        .arg(repo)
        .env("AGENTMESH_CACHE_DIR", cache)
        .env("HOME", home)
        .args(args)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => panic!("agentmesh command should spawn: {error}"),
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command should succeed, status {:?}, stdout {}, stderr {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_exit_code(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "command should exit {expected}, stdout {}, stderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout_json(output: &Output) -> Value {
    if !output.status.success() {
        panic!(
            "command should succeed, status {:?}, stderr {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(error) => panic!(
            "stdout should be JSON: {error}; stdout {}",
            String::from_utf8_lossy(&output.stdout)
        ),
    }
}

fn parse_stdout_json(output: &Output) -> Value {
    match serde_json::from_slice(&output.stdout) {
        Ok(value) => value,
        Err(error) => panic!(
            "stdout should be JSON: {error}; stdout {}",
            String::from_utf8_lossy(&output.stdout)
        ),
    }
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    condition()
}

fn write(path: impl AsRef<Path>, content: &str) {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            panic!("parent directory should be created: {error}");
        }
    }
    if let Err(error) = fs::write(path, content) {
        panic!("file should be written: {error}");
    }
}

fn read(path: impl AsRef<Path>) -> String {
    match fs::read_to_string(path.as_ref()) {
        Ok(content) => content,
        Err(error) => panic!("file should be readable: {error}"),
    }
}

fn find_named_file(root: &Path, file_name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(file_name) {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(found) = find_named_file(&path, file_name) {
                return Some(found);
            }
        }
    }
    None
}

fn find_file_containing(root: &Path, needle: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_file() {
            if let Ok(contents) = fs::read_to_string(&path) {
                if contents.contains(needle) {
                    return Some(path);
                }
            }
        } else if path.is_dir() {
            if let Some(found) = find_file_containing(&path, needle) {
                return Some(found);
            }
        }
    }
    None
}

fn pending_record_count(cache: &Path) -> usize {
    let Some(pending_dir) = find_named_file(cache, "pending-syncs") else {
        return 0;
    };
    match fs::read_dir(pending_dir) {
        Ok(entries) => entries.filter_map(Result::ok).count(),
        Err(error) => panic!("pending sync directory should be readable: {error}"),
    }
}

fn fixture_repo_with_both_runtimes(repo: &Path) {
    write(repo.join("CLAUDE.md"), "# Claude instructions\n");
    write(
        repo.join(".claude/skills/security-review/SKILL.md"),
        "---\nname: security-review\n---\nReview security.\n",
    );
    write(
        repo.join(".claude/skills/ops-runbook/SKILL.md"),
        "---\nname: ops-runbook\n---\nDocument operations.\n",
    );
    write(
        repo.join(".claude/agents/code-reviewer.md"),
        "---\nname: code-reviewer\n---\nReview code.\n",
    );
    write(
        repo.join(".claude/agents/planner.md"),
        "---\nname: planner\n---\nPlan work.\n",
    );
    write(
        repo.join(".codex/skills/api-design/SKILL.md"),
        "---\nname: api-design\n---\nDesign APIs.\n",
    );
    write(
        repo.join(".codex/skills/release-notes/SKILL.md"),
        "---\nname: release-notes\n---\nWrite release notes.\n",
    );
    write(
        repo.join(".codex/agents/triage.toml"),
        "name = \"triage\"\ninstructions = \"Triage issues.\"\n",
    );
}

#[test]
fn scan_reports_runtime_entities_without_writing_repository_state() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(repo.join("CLAUDE.md"), "# Claude instructions\n");
    write(repo.join("AGENTS.md"), "# Codex instructions\n");
    write(
        repo.join(".claude/skills/security-review/SKILL.md"),
        "---\nname: security-review\n---\nReview security.\n",
    );
    write(
        repo.join(".claude/agents/code-reviewer.md"),
        "---\nname: code-reviewer\n---\nReview code.\n",
    );
    write(
        repo.join(".codex/skills/api-design/SKILL.md"),
        "---\nname: api-design\n---\nDesign APIs.\n",
    );
    write(
        repo.join(".codex/agents/planner.toml"),
        "name = \"planner\"\ninstructions = \"Plan work.\"\n",
    );

    let json = stdout_json(&run_agentmesh(&repo, &cache, &["scan", "--json"]));

    assert_eq!(json["entity_count"], 6);
    assert_eq!(json["runtimes"][0]["name"], "claude");
    assert_eq!(json["runtimes"][0]["present"], true);
    assert_eq!(json["runtimes"][1]["name"], "codex");
    assert_eq!(json["runtimes"][1]["present"], true);
    assert!(!repo.join(".ai").exists());
    assert!(!repo.join("agentmesh.lock").exists());
}

#[test]
fn init_projects_all_entities_and_installs_detected_runtime_hooks() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    fixture_repo_with_both_runtimes(&repo);

    let init = run_agentmesh(
        &repo,
        &cache,
        &[
            "--silent",
            "init",
            "--yes",
            "--canonical-instructions",
            "CLAUDE.md",
        ],
    );

    assert_success(&init);
    let lockfile = read(repo.join("agentmesh.lock"));
    for id in [
        "instructions:root",
        "skill:api-design",
        "skill:ops-runbook",
        "skill:release-notes",
        "skill:security-review",
        "subagent:code-reviewer",
        "subagent:planner",
        "subagent:triage",
    ] {
        assert!(
            lockfile.contains(&format!("  {id}:")),
            "{id} should be locked"
        );
    }
    assert!(repo.join("AGENTS.md").exists());
    assert!(repo.join(".ai/skills/security-review/SKILL.md").exists());
    assert!(repo.join(".ai/skills/api-design/SKILL.md").exists());
    assert!(repo.join(".ai/subagents/code-reviewer.md").exists());
    assert!(repo.join(".ai/subagents/triage.md").exists());
    assert!(read(repo.join(".claude/settings.local.json")).contains("claude-hook"));
    assert!(read(repo.join(".codex/hooks.json")).contains("codex-hook"));
    assert!(find_named_file(&cache, "integrity.json").is_some());
    assert!(find_named_file(&cache, "hook-ownership.json").is_some());
}

#[test]
fn init_non_tty_divergent_root_instructions_requires_choice_or_yes() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(repo.join("AGENTS.md"), "# Agents instructions\n");
    write(repo.join("CLAUDE.md"), "# Claude instructions\n");

    let init = run_agentmesh(&repo, &cache, &["init", "--skip-hooks"]);

    assert_exit_code(&init, 10);
    assert!(
        String::from_utf8_lossy(&init.stderr).contains("divergent AGENTS.md and CLAUDE.md require"),
        "stderr should explain the missing non-interactive choice: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(!repo.join("agentmesh.lock").exists());
}

#[test]
fn init_yes_accepts_divergent_root_instructions_non_interactively() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(repo.join("AGENTS.md"), "# Agents instructions\n");
    write(repo.join("CLAUDE.md"), "# Claude instructions\n");

    let init = run_agentmesh(
        &repo,
        &cache,
        &["--silent", "init", "--yes", "--skip-hooks"],
    );

    assert_success(&init);
    assert_eq!(read(repo.join("AGENTS.md")), "# Agents instructions\n");
    assert!(repo.join("agentmesh.lock").exists());
}

#[test]
fn init_canonical_instructions_selects_claude_md_in_non_tty() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(repo.join("AGENTS.md"), "# Agents instructions\n");
    write(repo.join("CLAUDE.md"), "# Claude instructions\n");

    let init = run_agentmesh(
        &repo,
        &cache,
        &[
            "--silent",
            "init",
            "--skip-hooks",
            "--canonical-instructions",
            "CLAUDE.md",
        ],
    );

    assert_success(&init);
    assert_eq!(read(repo.join("AGENTS.md")), "# Claude instructions\n");
    assert_eq!(read(repo.join("CLAUDE.md")), "# Claude instructions\n");
}

#[test]
fn claude_hook_trigger_imports_new_skill_and_drains_pending_record() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join(".claude/skills/base/SKILL.md"),
        "---\nname: base\n---\nBase skill.\n",
    );
    write(repo.join(".codex/.keep"), "");
    assert_success(&run_agentmesh(
        &repo,
        &cache,
        &["--silent", "init", "--yes", "--skip-hooks"],
    ));
    write(
        repo.join(".claude/skills/hot-path/SKILL.md"),
        "---\nname: hot-path\n---\nFast sync.\n",
    );

    let sync = run_agentmesh(
        &repo,
        &cache,
        &["--silent", "sync", "--trigger=claude-hook"],
    );

    assert_success(&sync);
    assert!(repo.join(".ai/skills/hot-path/SKILL.md").exists());
    assert!(read(repo.join("agentmesh.lock")).contains("  skill:hot-path:"));

    let drained = wait_until(Duration::from_secs(5), || {
        repo.join(".codex/skills/hot-path/SKILL.md").exists() && pending_record_count(&cache) == 0
    });
    assert!(drained, "background drainer should fan out hook changes");
    assert_eq!(pending_record_count(&cache), 0);
}

#[test]
fn diff_reports_drift_without_writing_and_apply_fans_out() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join(".claude/skills/diffable/SKILL.md"),
        "---\nname: diffable\n---\nOriginal body.\n",
    );
    write(repo.join(".codex/.keep"), "");
    assert_success(&run_agentmesh(
        &repo,
        &cache,
        &["--silent", "init", "--yes", "--skip-hooks"],
    ));
    write(
        repo.join(".claude/skills/diffable/SKILL.md"),
        "---\nname: diffable\n---\nChanged by Claude.\n",
    );

    let diff_output = run_agentmesh(&repo, &cache, &["--silent", "diff", "--json"]);
    assert_exit_code(&diff_output, 1);
    let diff = parse_stdout_json(&diff_output);

    assert_eq!(diff["changed"], true);
    assert_eq!(diff["pending_enqueued"], 0);
    assert!(diff["reviewed_diff_state"].is_string());
    assert!(read(repo.join(".ai/skills/diffable/SKILL.md")).contains("Original body."));
    assert!(read(repo.join(".codex/skills/diffable/SKILL.md")).contains("Original body."));

    let apply = run_agentmesh(&repo, &cache, &["--silent", "apply"]);
    assert_success(&apply);
    assert!(read(repo.join(".ai/skills/diffable/SKILL.md")).contains("Changed by Claude."));
    assert!(read(repo.join(".codex/skills/diffable/SKILL.md")).contains("Changed by Claude."));
    assert_eq!(pending_record_count(&cache), 0);
}

#[test]
fn apply_requires_a_reviewed_diff_state() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join(".claude/skills/apply/SKILL.md"),
        "---\nname: apply\n---\nOriginal body.\n",
    );
    write(repo.join(".codex/.keep"), "");
    assert_success(&run_agentmesh(
        &repo,
        &cache,
        &["--silent", "init", "--yes", "--skip-hooks"],
    ));
    write(
        repo.join(".claude/skills/apply/SKILL.md"),
        "---\nname: apply\n---\nChanged body.\n",
    );

    let apply = run_agentmesh(&repo, &cache, &["--silent", "apply"]);

    assert_exit_code(&apply, 10);
    assert!(read(repo.join(".ai/skills/apply/SKILL.md")).contains("Original body."));
}

#[test]
fn diff_exits_zero_when_clean_and_does_not_enqueue_work() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    if let Err(error) = fs::create_dir_all(&repo) {
        panic!("repo directory should be created: {error}");
    }

    let diff = stdout_json(&run_agentmesh(
        &repo,
        &cache,
        &["--silent", "diff", "--json"],
    ));

    assert_eq!(diff["changed"], false);
    assert_eq!(diff["pending_enqueued"], 0);
    assert_eq!(pending_record_count(&cache), 0);
}

#[test]
fn install_and_uninstall_are_machine_local_and_surgical() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(repo.join("agentmesh.lock"), "version: 1\nschema: 1\n");
    write(
        repo.join(".claude/settings.local.json"),
        r#"{"hooks":{"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo user"}]}]}}"#,
    );
    write(
        repo.join(".codex/hooks.json"),
        r#"{"PostToolUse":[{"matcher":"^Bash$","hooks":[{"type":"command","command":"echo user"}]}]}"#,
    );
    let original_lockfile = read(repo.join("agentmesh.lock"));

    let claude_install = run_agentmesh(
        &repo,
        &cache,
        &["--silent", "install", "--runtime", "claude", "--yes"],
    );
    assert!(claude_install.status.success());
    let codex_install = run_agentmesh(
        &repo,
        &cache,
        &["--silent", "install", "--runtime", "codex", "--yes"],
    );
    assert!(codex_install.status.success());

    let ownership_path = match find_named_file(&cache, "hook-ownership.json") {
        Some(path) => path,
        None => panic!("hook ownership should be recorded"),
    };
    let Some(repo_cache_dir) = ownership_path.parent() else {
        panic!("hook ownership should have a parent directory");
    };
    let claude_overlay = read(repo.join(".claude/settings.local.json"));
    let codex_overlay = read(repo.join(".codex/hooks.json"));
    assert!(claude_overlay.contains("echo user"));
    assert!(claude_overlay.contains("claude-hook"));
    assert!(codex_overlay.contains("echo user"));
    assert!(codex_overlay.contains("codex-hook"));
    assert_eq!(read(repo.join("agentmesh.lock")), original_lockfile);

    let uninstall = run_agentmesh(&repo, &cache, &["--silent", "uninstall", "--yes"]);
    assert!(uninstall.status.success());

    let claude_overlay = read(repo.join(".claude/settings.local.json"));
    let codex_overlay = read(repo.join(".codex/hooks.json"));
    assert!(claude_overlay.contains("echo user"));
    assert!(!claude_overlay.contains("claude-hook"));
    assert!(codex_overlay.contains("echo user"));
    assert!(!codex_overlay.contains("codex-hook"));
    assert!(!repo_cache_dir.exists());
    assert_eq!(read(repo.join("agentmesh.lock")), original_lockfile);
}

#[test]
fn side_effect_commands_require_confirmation_in_non_tty() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(repo.join(".claude/.keep"), "");

    let install = run_agentmesh(&repo, &cache, &["install", "--runtime", "claude"]);

    assert_exit_code(&install, 10);
    assert!(!repo.join(".claude/settings.local.json").exists());

    let upgrade = run_agentmesh(&repo, &cache, &["upgrade"]);
    assert_exit_code(&upgrade, 10);

    let uninstall = run_agentmesh(&repo, &cache, &["uninstall"]);
    assert_exit_code(&uninstall, 10);

    write(
        repo.join("agentmesh.lock"),
        r#"version: 1
schema: 1
entities:
  skill:first:
    type: skill
    locations:
      .ai: skills/first/SKILL.md
    canonical_sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    emitted_native_sha256: {}
    pending_conflict_resolution: true
"#,
    );
    let ack = run_agentmesh(&repo, &cache, &["ack", "skill:first"]);
    assert_exit_code(&ack, 10);
}

#[test]
fn uninstall_purge_removes_repository_state_after_cleaning_hooks() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join(".claude/skills/purge/SKILL.md"),
        "---\nname: purge\n---\nPurge skill.\n",
    );
    write(repo.join(".codex/.keep"), "");
    assert_success(&run_agentmesh(
        &repo,
        &cache,
        &["--silent", "init", "--yes"],
    ));
    assert!(repo.join(".ai").exists());
    assert!(repo.join("agentmesh.lock").exists());

    let uninstall = run_agentmesh(
        &repo,
        &cache,
        &["--silent", "uninstall", "--yes", "--purge"],
    );

    assert_success(&uninstall);
    assert!(!repo.join(".ai").exists());
    assert!(!repo.join("agentmesh.lock").exists());
    if repo.join(".claude/settings.local.json").exists() {
        assert!(!read(repo.join(".claude/settings.local.json")).contains("claude-hook"));
    }
    if repo.join(".codex/hooks.json").exists() {
        assert!(!read(repo.join(".codex/hooks.json")).contains("codex-hook"));
    }
}

#[test]
fn ack_without_id_clears_all_pending_conflict_flags() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join("agentmesh.lock"),
        r#"version: 1
schema: 1
entities:
  skill:first:
    type: skill
    locations:
      .ai: skills/first/SKILL.md
    canonical_sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    emitted_native_sha256: {}
    pending_conflict_resolution: true
  skill:second:
    type: skill
    locations:
      .ai: skills/second/SKILL.md
    canonical_sha256: fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210
    emitted_native_sha256: {}
    pending_conflict_resolution: true
"#,
    );

    let ack = run_agentmesh(&repo, &cache, &["--silent", "ack", "--yes"]);

    assert_success(&ack);
    let lockfile = read(repo.join("agentmesh.lock"));
    assert!(lockfile.contains("  skill:first:"));
    assert!(lockfile.contains("  skill:second:"));
    assert!(!lockfile.contains("pending_conflict_resolution: true"));
}

#[test]
fn ack_with_id_clears_only_the_selected_pending_conflict() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join("agentmesh.lock"),
        r#"version: 1
schema: 1
entities:
  skill:first:
    type: skill
    locations:
      .ai: skills/first/SKILL.md
    canonical_sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    emitted_native_sha256: {}
    pending_conflict_resolution: true
  skill:second:
    type: skill
    locations:
      .ai: skills/second/SKILL.md
    canonical_sha256: fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210
    emitted_native_sha256: {}
    pending_conflict_resolution: true
"#,
    );

    let ack = run_agentmesh(&repo, &cache, &["--silent", "ack", "skill:first", "--yes"]);

    assert_success(&ack);
    let lockfile = read(repo.join("agentmesh.lock"));
    assert!(lockfile.contains("  skill:first:"));
    assert!(lockfile.contains("  skill:second:"));
    let first_block = lockfile.split("  skill:second:").next().unwrap_or_default();
    let second_block = lockfile.split("  skill:second:").nth(1).unwrap_or_default();
    assert!(!first_block.contains("pending_conflict_resolution: true"));
    assert!(second_block.contains("pending_conflict_resolution: true"));
}

#[test]
fn restore_dry_run_and_at_select_preserved_version_without_writing() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join(".ai/skills/recover/SKILL.md"),
        "---\nname: recover\n---\nCurrent body.\n",
    );
    write(
        repo.join("agentmesh.lock"),
        r#"version: 1
schema: 1
entities:
  skill:recover:
    type: skill
    locations:
      .ai: skills/recover/SKILL.md
    canonical_sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    emitted_native_sha256: {}
    pending_conflict_resolution: true
"#,
    );
    let layout = match agentmesh_core::state::CacheLayout::new(&cache, &repo) {
        Ok(layout) => layout,
        Err(error) => panic!("cache layout should build: {error}"),
    };
    if let Err(error) = layout.ensure_dirs() {
        panic!("cache dirs should be created: {error}");
    }
    write(
        layout
            .conflicts_dir
            .join("skill:recover")
            .join("claude-unix-1.md"),
        "---\nname: recover\n---\nOlder body.\n",
    );
    write(
        layout
            .conflicts_dir
            .join("skill:recover")
            .join("claude-unix-2.md"),
        "---\nname: recover\n---\nRestored body.\n",
    );

    let dry_run = run_agentmesh(
        &repo,
        &cache,
        &[
            "restore",
            "skill:recover",
            "--from",
            "claude",
            "--at",
            "unix-2",
            "--dry-run",
        ],
    );

    assert_success(&dry_run);
    assert!(String::from_utf8_lossy(&dry_run.stdout).contains("unix-2"));
    assert!(read(repo.join(".ai/skills/recover/SKILL.md")).contains("Current body."));
}

#[test]
fn restore_at_with_yes_writes_selected_version_and_clears_pending_flag() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join(".ai/skills/recover/SKILL.md"),
        "---\nname: recover\n---\nCurrent body.\n",
    );
    write(
        repo.join("agentmesh.lock"),
        r#"version: 1
schema: 1
entities:
  skill:recover:
    type: skill
    locations:
      .ai: skills/recover/SKILL.md
    canonical_sha256: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
    emitted_native_sha256: {}
    pending_conflict_resolution: true
"#,
    );
    let layout = match agentmesh_core::state::CacheLayout::new(&cache, &repo) {
        Ok(layout) => layout,
        Err(error) => panic!("cache layout should build: {error}"),
    };
    if let Err(error) = layout.ensure_dirs() {
        panic!("cache dirs should be created: {error}");
    }
    write(
        layout
            .conflicts_dir
            .join("skill:recover")
            .join("claude-unix-1.md"),
        "---\nname: recover\n---\nOlder body.\n",
    );
    write(
        layout
            .conflicts_dir
            .join("skill:recover")
            .join("claude-unix-2.md"),
        "---\nname: recover\n---\nRestored body.\n",
    );

    let restore = run_agentmesh(
        &repo,
        &cache,
        &[
            "restore",
            "skill:recover",
            "--from",
            "claude",
            "--at",
            "unix-2",
            "--yes",
        ],
    );

    assert_success(&restore);
    assert!(read(repo.join(".ai/skills/recover/SKILL.md")).contains("Restored body."));
    assert!(!read(repo.join("agentmesh.lock")).contains("pending_conflict_resolution: true"));
}

#[test]
fn git_pre_commit_install_is_additive_and_executable() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(repo.join(".git/hooks/.keep"), "");

    let install = run_agentmesh(
        &repo,
        &cache,
        &["--silent", "install", "--git-pre-commit", "--yes"],
    );

    assert_success(&install);
    let hook = repo.join(".git/hooks/pre-commit");
    let contents = read(&hook);
    assert!(contents.starts_with("#!/bin/sh\nset -eu\n"));
    assert!(contents.contains("AgentMesh"));
    assert!(contents.contains("sync --check --silent"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = match fs::metadata(&hook) {
            Ok(metadata) => metadata,
            Err(error) => panic!("pre-commit hook metadata should be readable: {error}"),
        };
        assert_ne!(metadata.permissions().mode() & 0o111, 0);
    }
}

#[test]
fn upgrade_rewrites_recorded_runtime_hooks_to_current_binary() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join(".claude/skills/upgrade/SKILL.md"),
        "---\nname: upgrade\n---\nUpgrade hook.\n",
    );
    write(repo.join(".codex/.keep"), "");
    assert_success(&run_agentmesh(
        &repo,
        &cache,
        &["--silent", "init", "--yes"],
    ));
    let binary = agentmesh_bin().display().to_string();
    for overlay in [
        repo.join(".claude/settings.local.json"),
        repo.join(".codex/hooks.json"),
    ] {
        let stale = read(&overlay).replace(&binary, "/tmp/old-agentmesh");
        write(&overlay, &stale);
        assert!(read(&overlay).contains("/tmp/old-agentmesh"));
    }

    let upgrade = run_agentmesh(&repo, &cache, &["--silent", "upgrade", "--yes"]);

    assert_success(&upgrade);
    for overlay in [
        repo.join(".claude/settings.local.json"),
        repo.join(".codex/hooks.json"),
    ] {
        let contents = read(&overlay);
        assert!(!contents.contains("/tmp/old-agentmesh"));
        assert!(contents.contains(&binary));
    }
}

#[test]
fn uninstall_stops_a_running_watcher() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    let home = temp.path().join("home");
    write(repo.join(".claude/.keep"), "");
    assert_success(&run_agentmesh_with_home(
        &repo,
        &cache,
        &home,
        &["--silent", "init", "--yes", "--skip-hooks"],
    ));

    let mut watcher = spawn_agentmesh_with_home(
        &repo,
        &cache,
        &home,
        &["--silent", "watch", "--foreground", "--persistent"],
    );
    let started = wait_until(Duration::from_secs(5), || {
        find_named_file(&cache, "watcher.pid").is_some()
    });
    assert!(started, "watcher should write a pid record");

    let uninstall = run_agentmesh(&repo, &cache, &["--silent", "uninstall", "--yes"]);
    assert_success(&uninstall);

    let exited = wait_until(Duration::from_secs(5), || match watcher.try_wait() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => panic!("watcher status should be readable: {error}"),
    });
    if !exited {
        let _ = watcher.kill();
        let _ = watcher.wait();
    }

    assert!(exited, "uninstall should stop the running watcher");
    assert!(find_named_file(&cache, "watcher.pid").is_none());
}

#[test]
fn service_registration_writes_platform_definition() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    let home = temp.path().join("home");
    write(repo.join(".git/.keep"), "");

    let register = run_agentmesh_with_home(
        &repo,
        &cache,
        &home,
        &[
            "--silent",
            "watch",
            "--register-as-service",
            "--persistent",
            "--yes",
        ],
    );

    assert_success(&register);
    let search_root = if cfg!(target_os = "windows") {
        repo.as_path()
    } else {
        home.as_path()
    };
    let service = match find_file_containing(search_root, "agentmesh") {
        Some(path) => path,
        None => panic!("service definition should be written"),
    };
    let contents = read(service);
    assert!(contents.contains("--cwd"));
    assert!(contents.contains(&repo.display().to_string()));
    assert!(contents.contains("watch"));
    assert!(contents.contains("--foreground"));
    assert!(contents.contains("--persistent"));
}

#[test]
fn foreground_watcher_drains_canonical_edit_through_core_sync() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    let home = temp.path().join("home");
    write(
        repo.join(".claude/skills/watched/SKILL.md"),
        "---\nname: watched\n---\nOriginal watcher body.\n",
    );
    write(repo.join(".codex/.keep"), "");
    assert_success(&run_agentmesh_with_home(
        &repo,
        &cache,
        &home,
        &["--silent", "init", "--yes", "--skip-hooks"],
    ));

    let mut watcher = spawn_agentmesh_with_home(
        &repo,
        &cache,
        &home,
        &["--silent", "watch", "--foreground", "--persistent"],
    );
    let started = wait_until(Duration::from_secs(5), || {
        find_named_file(&cache, "watcher.log")
            .map(|path| read(path).contains("start-foreground"))
            .unwrap_or(false)
    });
    assert!(started, "foreground watcher should start");
    thread::sleep(Duration::from_millis(200));
    write(
        repo.join(".ai/skills/watched/SKILL.md"),
        "---\nname: watched\n---\nEdited through canonical file.\n",
    );

    let drained = wait_until(Duration::from_secs(10), || {
        find_named_file(&cache, "watcher.log")
            .map(|path| {
                let log = read(path);
                log.contains("\"trigger\":\"watcher\"") && log.contains("drain-complete")
            })
            .unwrap_or(false)
    });
    let _ = watcher.kill();
    let _ = watcher.wait();

    assert!(drained, "watcher should fan out the canonical edit");
}

#[test]
fn explicit_cache_dir_is_used_for_core_cli_and_watcher_state() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("custom-cache");
    let home = temp.path().join("home");
    write(
        repo.join(".claude/skills/cache/SKILL.md"),
        "---\nname: cache\n---\nCache root.\n",
    );
    write(repo.join(".codex/.keep"), "");

    let init = run_agentmesh_with_home(
        &repo,
        &cache,
        &home,
        &["--silent", "init", "--yes", "--skip-hooks"],
    );
    assert_success(&init);
    let watch = run_agentmesh_with_home(
        &repo,
        &cache,
        &home,
        &["--silent", "watch", "--register-as-service", "--yes"],
    );
    assert_success(&watch);

    assert!(find_named_file(&cache, "integrity.json").is_some());
    assert!(find_named_file(&cache, "watcher.log").is_some());
    assert!(!home.join(".cache/agentmesh").exists());
    assert!(!home.join("Library/Caches/agentmesh").exists());
}

#[test]
fn status_and_doctor_report_integrity_mismatch_against_running_binary() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join(".claude/skills/integrity/SKILL.md"),
        "---\nname: integrity\n---\nIntegrity check.\n",
    );
    write(repo.join(".codex/.keep"), "");
    assert_success(&run_agentmesh(
        &repo,
        &cache,
        &["--silent", "init", "--yes", "--skip-hooks"],
    ));
    let pin_path = match find_named_file(&cache, "integrity.json") {
        Some(path) => path,
        None => panic!("integrity pin should exist"),
    };
    let mut pin = match serde_json::from_str::<Value>(&read(&pin_path)) {
        Ok(value) => value,
        Err(error) => panic!("integrity pin should parse: {error}"),
    };
    pin["binary_path"] = Value::String("/tmp/not-the-running-agentmesh".to_string());
    let pin_json = match serde_json::to_string_pretty(&pin) {
        Ok(value) => value,
        Err(error) => panic!("integrity pin should serialize: {error}"),
    };
    write(&pin_path, &pin_json);

    let status = run_agentmesh(&repo, &cache, &["status", "--json"]);
    assert_exit_code(&status, 3);
    let status_json = parse_stdout_json(&status);
    assert_eq!(status_json["integrity"]["status"], "mismatch");
    assert_eq!(status_json["integrity"]["matches_running_binary"], false);
    assert!(status_json["integrity"]["running_path"].is_string());
    assert!(status_json["integrity"]["running_sha256"].is_string());

    let doctor = run_agentmesh(&repo, &cache, &["doctor", "--integrity-only"]);
    assert_exit_code(&doctor, 3);
    let stdout = String::from_utf8_lossy(&doctor.stdout);
    assert!(stdout.contains("Status:           mismatch"));
    assert!(stdout.contains("Pinned sha256:"));
    assert!(stdout.contains("Running binary:"));
}

#[test]
fn doctor_reports_hook_ownership_mismatch() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join(".claude/settings.local.json"),
        r#"{"hooks":{"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"/bin/agentmesh sync --trigger=claude-hook --silent"}]}]}}"#,
    );

    let doctor = run_agentmesh(&repo, &cache, &["doctor", "--json"]);

    assert_exit_code(&doctor, 3);
    let json = parse_stdout_json(&doctor);
    assert_eq!(json["hook_ownership"]["status"], "mismatch");
    assert!(
        json["hook_ownership"]["issues"][0]
            .as_str()
            .unwrap_or_default()
            .contains("ownership")
    );
}

#[test]
fn sync_check_exit_codes_distinguish_generic_drift_and_strict_conflict() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    write(
        repo.join(".claude/skills/check/SKILL.md"),
        "---\nname: check\n---\nOriginal check body.\n",
    );
    assert_success(&run_agentmesh(
        &repo,
        &cache,
        &["--silent", "init", "--yes", "--skip-hooks"],
    ));
    write(
        repo.join(".claude/skills/check/SKILL.md"),
        "---\nname: check\n---\nChanged check body.\n",
    );

    let drift = run_agentmesh(&repo, &cache, &["--silent", "sync", "--check"]);
    assert_exit_code(&drift, 1);

    write(
        repo.join("agentmesh.config.yaml"),
        "version: 1\nci:\n  fail_on_conflict: true\n",
    );
    let mut lockfile = read(repo.join("agentmesh.lock"));
    lockfile = lockfile.replacen(
        "    emitted_native_sha256:",
        "    pending_conflict_resolution: true\n    emitted_native_sha256:",
        1,
    );
    write(repo.join("agentmesh.lock"), &lockfile);

    let strict = run_agentmesh(&repo, &cache, &["--silent", "sync", "--check"]);
    assert_exit_code(&strict, 2);
}

#[test]
fn scan_ignores_personal_runtime_directories() {
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(error) => panic!("tempdir should be available: {error}"),
    };
    let repo = temp.path().join("repo");
    let cache = temp.path().join("cache");
    let home = temp.path().join("home");
    if let Err(error) = fs::create_dir_all(&repo) {
        panic!("repo directory should be created: {error}");
    }
    write(
        home.join(".claude/skills/private/SKILL.md"),
        "---\nname: private\n---\nPrivate skill.\n",
    );
    write(
        home.join(".codex/skills/private/SKILL.md"),
        "---\nname: private\n---\nPrivate skill.\n",
    );

    let json = stdout_json(&run_agentmesh_with_home(
        &repo,
        &cache,
        &home,
        &["scan", "--json"],
    ));

    assert_eq!(json["entity_count"], 0);
    assert_eq!(json["runtimes"][0]["present"], false);
    assert_eq!(json["runtimes"][1]["present"], false);
    assert!(!repo.join(".ai").exists());
    assert!(!repo.join("agentmesh.lock").exists());
}
