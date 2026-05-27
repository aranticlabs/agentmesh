# AgentMesh

AgentMesh synchronizes project-level AI runtime context across coding tools.

The v0.1 binary is a local-first Rust CLI with bundled Claude Code and Codex adapters. It
normalizes project instructions, skills, and subagents into a shared repository model, then renders
those entities back into each runtime's native file layout.

Documentation: [agentmesh.sh](https://agentmesh.sh)

## Install

**macOS / Linux:**

```bash
curl -fsSL https://agentmesh.sh/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://agentmesh.sh/install.ps1 | iex
```

Upgrade and uninstall options are documented at
[agentmesh.sh/docs/installation/curl](https://agentmesh.sh/docs/installation/curl).

For local development, build from source:

```bash
cargo build --workspace
./target/debug/agentmesh --help
```

## Quickstart

**Prerequisites:** a git repository at your project root, and at least one supported runtime
present or planned (Claude Code with `.claude/` and/or `CLAUDE.md`, or Codex with `.codex/` and/or
`AGENTS.md`).

Preview detection without writing:

```bash
agentmesh scan
```

Initialize AgentMesh from your project root. This detects runtimes, imports entities into the
canonical `.ai/` model, propagates to other runtimes, installs hooks, and writes `agentmesh.lock`:

```bash
cd /path/to/your/repo
agentmesh init
```

If `AGENTS.md` and `CLAUDE.md` differ, `init` asks which agent memory file to use as
the starting version for initial setup. After setup, sync is bidirectional. For scripts or CI:

```bash
agentmesh init --canonical-instructions=AGENTS.md -y
```

Verify health:

```bash
agentmesh status
agentmesh doctor
```

Commit the shared state teammates need:

```bash
git add AGENTS.md .ai/ agentmesh.lock
git commit -m "chore: initialize AgentMesh sync"
```

Do not commit machine-local hook files (`.claude/settings.local.json`, `.codex/hooks.json`). Each
teammate runs `agentmesh init` on their machine. Add `.codex/hooks.json` to `.gitignore`.

Codex requires a one-time trust approval for the command hook before it runs. Sync still works via
the watcher daemon and manual `agentmesh sync` until then.

| Situation                  | Command                              |
| -------------------------- | ------------------------------------ |
| Added a runtime after init | `agentmesh install --runtime <name>` |
| Upgraded the binary        | `agentmesh upgrade`                  |
| Commit-time drift check    | `agentmesh install --git-pre-commit` |
| CI pipeline                | `agentmesh sync --check`             |

Full walkthrough: [agentmesh.sh/quickstart](https://agentmesh.sh/quickstart)

## Start, Stop, And Uninstall

To start AgentMesh again for an initialized repository:

```bash
agentmesh start -y
```

This refreshes machine-local AgentMesh state and installs AgentMesh-owned hooks for detected runtimes. It keeps `agentmesh.lock`, `.ai/`, and runtime files such as `AGENTS.md` intact.

To stop AgentMesh for the current repository while keeping all repository state and AgentMesh installed on this computer:

```bash
agentmesh stop -y
```

This stops the watcher, removes AgentMesh-owned hooks, and clears machine-local cache state. `agentmesh.lock`, `.ai/`, and runtime files such as `AGENTS.md` are left intact so you can re-run `agentmesh init` later.

To uninstall AgentMesh from the current repository:

```bash
agentmesh uninstall -y
```

This deletes only AgentMesh-owned repository state: `agentmesh.lock`, `.ai/`, and `agentmesh.config.yaml`. AgentMesh never deletes runtime files such as `AGENTS.md` or `CLAUDE.md`.

To uninstall AgentMesh from the current repository and this computer:

```bash
agentmesh uninstall -y --full
```

This also removes the `agentmesh` command from this computer. Runtime files such as `AGENTS.md` and `CLAUDE.md` are still retained.

Preview planned stop or uninstall changes without writing:

```bash
agentmesh start --dry-run
agentmesh stop --dry-run
agentmesh uninstall --dry-run
```

More detail: [agentmesh.sh/docs/installation/curl](https://agentmesh.sh/docs/installation/curl).

## Development

```bash
make ci
```

The workspace is pinned by [rust-toolchain.toml](rust-toolchain.toml).

Architecture notes for contributors are in [ARCHITECTURE.md](ARCHITECTURE.md).
