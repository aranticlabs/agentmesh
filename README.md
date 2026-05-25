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

The npm package and upgrade/uninstall options are documented at
[agentmesh.sh/installation/curl](https://agentmesh.sh/installation/curl).

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

If `AGENTS.md` and `CLAUDE.md` differ, `init` asks which is canonical. For scripts or CI:

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

## Uninstall

**From a project**, run from the repository root:

```bash
agentmesh uninstall --yes
```

This stops the watcher, removes AgentMesh-owned hooks, and clears machine-local cache state. `agentmesh.lock`, `.ai/`, and runtime files such as `AGENTS.md` are left intact so you can re-run `agentmesh init` later.

To also remove repository-visible AgentMesh state:

```bash
agentmesh uninstall --yes --purge
```

This deletes `agentmesh.lock`, `.ai/`, and `agentmesh.config.yaml`. Emitted runtime files are not removed automatically; delete or edit those manually if you no longer want them.

Preview planned changes without writing:

```bash
agentmesh uninstall --dry-run
```

**Remove the binary** after uninstalling from each project:

| Install method      | Command                                            |
| ------------------- | -------------------------------------------------- |
| curl / `install.sh` | `rm ~/.local/bin/agentmesh` (or your install path) |
| npm                 | `npm uninstall -g @aranticlabs/agentmesh`          |
| Windows             | Remove `agentmesh.exe` from your install directory |

More detail: [agentmesh.sh/installation/curl](https://agentmesh.sh/installation/curl).

## Development

```bash
make ci
```

The workspace is pinned by [rust-toolchain.toml](rust-toolchain.toml).

Architecture notes for contributors are in [ARCHITECTURE.md](ARCHITECTURE.md).
