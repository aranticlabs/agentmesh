# AgentMesh

AgentMesh synchronizes project-level AI runtime context across coding tools.

The v0.2 binary is a local-first Rust CLI with bundled adapters for Claude Code, Codex,
GitHub Copilot, Cursor, and Gemini CLI. It normalizes project instructions, rules, prompts,
skills, subagents, commands, hooks, MCP bindings, and permission policies into a shared repository
model, then renders supported entities back into each runtime's native file layout.

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
present or planned:

- Claude Code: `.claude/` and/or `CLAUDE.md`
- Codex: `.codex/` and/or `AGENTS.md`
- GitHub Copilot: `.github/copilot-instructions.md`, `.github/instructions/`,
  `.github/prompts/`, `.github/skills/`, or `.github/agents/`
- Cursor: `.cursor/rules/`
- Gemini CLI: `GEMINI.md`, nested `GEMINI.md`, `.gemini/skills/`, or `.gemini/commands/`

Preview detection without writing:

```bash
agentmesh scan
```

Initialize AgentMesh from your project root. This detects runtimes, imports entities into the
canonical `.ai/` model, propagates to other runtimes, installs hooks for hook-capable runtimes,
starts watcher coverage, and writes `agentmesh.lock`:

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

Commit native runtime files that are part of your team workflow, such as `CLAUDE.md`,
`.claude/rules/`, `.codex/config.toml`, `.github/`, `.cursor/rules/`, `GEMINI.md`, `.gemini/`,
and shared `.agents/skills/`.

Do not commit machine-local hook files (`.claude/settings.local.json`, `.codex/hooks.json`). Each
teammate runs `agentmesh init` on their machine. Add `.codex/hooks.json` to `.gitignore`.

Codex requires a one-time trust approval before it runs the AgentMesh command hook. After setup,
open Codex in the repository and run any tool-backed action; when Codex asks whether to trust the
AgentMesh hook command, approve it once. Sync still works via the watcher daemon, Claude hooks, and
manual `agentmesh sync` until then.

Cursor, GitHub Copilot, and Gemini CLI are watcher/manual-sync runtimes in v0.2. AgentMesh imports
and emits their write-enabled project files, but does not install native runtime hooks for them.
`agentmesh doctor` reports read-only and deferred surfaces instead of silently writing unsupported
files.

| Situation                       | Command                              |
| ------------------------------- | ------------------------------------ |
| Added Claude/Codex after init   | `agentmesh install --runtime <name>` |
| Added hookless runtime files    | `agentmesh sync --await-drain`       |
| Upgraded the binary             | `agentmesh upgrade`                  |
| Commit-time drift check         | `agentmesh install --git-pre-commit` |
| CI pipeline                     | `agentmesh sync --check`             |

Full walkthrough: [agentmesh.sh/quickstart](https://agentmesh.sh/quickstart)

## Start, Stop, And Uninstall

To start AgentMesh again for an initialized repository:

```bash
agentmesh start -y
```

This refreshes machine-local AgentMesh state, installs AgentMesh-owned hooks for detected
hook-capable runtimes, and starts the watcher so direct edits to supported native files sync
promptly. It keeps `agentmesh.lock`, `.ai/`, and runtime files such as `AGENTS.md` intact.

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
