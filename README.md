# AgentMesh

AgentMesh synchronizes project-level AI runtime context across coding tools.

The v0.1 binary is a local-first Rust CLI with bundled Claude Code and Codex adapters. It
normalizes project instructions, skills, and subagents into a shared repository model, then renders
those entities back into each runtime's native file layout.

The public documentation site is planned for [agentmesh.sh](https://agentmesh.sh).

## Install

For local development, build the binary from source:

```bash
cargo build --workspace
./target/debug/agentmesh --help
```

Installer wrappers live under [installers](installers/). Release installs verify archives against
`SHA256SUMS` plus the published cosign signature before placing or delegating to the binary.

## Quickstart

Inspect a repository without writing:

```bash
agentmesh scan
agentmesh status
agentmesh doctor
```

Start filesystem coverage. By default this spawns a background watcher; use `--foreground` when
you want the notify loop attached to the current terminal:

```bash
agentmesh watch
agentmesh watch --foreground
```

Install runtime hooks explicitly when you are ready for native runtime integration:

```bash
agentmesh install --runtime claude
agentmesh install --runtime codex
```

Codex will require a one-time trust approval for the command hook before it runs.

After replacing or rebuilding the binary, repin repository hook integrity:

```bash
agentmesh upgrade
```

Run installer smoke checks without published artifacts:

```bash
sh installers/install.sh --smoke
sh installers/install.sh --upgrade-help
pwsh -NoProfile -ExecutionPolicy Bypass -File installers/install.ps1 -Smoke
pwsh -NoProfile -ExecutionPolicy Bypass -File installers/install.ps1 -UpgradeHelp
sh installers/npm/bin/agentmesh --smoke
sh installers/npm/bin/agentmesh --upgrade-help
```

## Development

```bash
make ci
```

The workspace is pinned by [rust-toolchain.toml](rust-toolchain.toml).

Architecture notes for contributors are in [ARCHITECTURE.md](ARCHITECTURE.md).
