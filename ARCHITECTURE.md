# Architecture

AgentMesh is a single Rust workspace that builds one `agentmesh` CLI binary with first-party
runtime adapters linked in.

```text
agentmesh
  -> agentmesh-core
  -> agentmesh-watcher
  -> agentmesh-adapter-sdk-rust
  -> adapters/claude
  -> adapters/codex

agentmesh-core <-> agentmesh-protocol
adapters/*     <-> agentmesh-protocol
```

## Crates

`crates/agentmesh` owns argument parsing, user-facing output, hidden adapter dispatch, and thin
command orchestration.

`crates/agentmesh-core` owns persisted state, lockfile types, identity, merge, queue, and machine
cache data structures. Domain rules should live here rather than in CLI handlers.

`crates/agentmesh-protocol` owns JSON-RPC protocol structs, framing, and adapter error codes.

`crates/agentmesh-adapter-sdk-rust` owns the Rust adapter trait, stdio server loop, atomic write
helpers, content hashing, and Markdown frontmatter helpers shared by bundled adapters.

`crates/agentmesh-watcher` owns daemon lifecycle and filesystem watching. It can spawn a background
watcher process or run a foreground `notify` loop, records idle/drain status in machine-local
state, debounces ordinary filesystem changes, throttles VCS metadata churn, and suppresses
self-emitted native writes by comparing current file hashes with lockfile-emitted native hashes.
The foreground watcher drives the same core sync path as hooks and CLI sync. Operating-system
service registration writes platform launch definitions without silently installing global hooks.

`adapters/claude` imports and emits Claude-native files:

- `CLAUDE.md`
- `.claude/skills/<name>/SKILL.md`
- `.claude/agents/<name>.md`
- `.claude/settings.local.json` for hook installation

`adapters/codex` imports and emits Codex-native files:

- `AGENTS.md`
- `.codex/skills/<name>/SKILL.md`
- `.codex/agents/<name>.toml`
- `.codex/hooks.json` for hook installation

## Runtime Boundaries

Adapters declare their readable and writable workspace-relative paths during initialization. Runtime
file writes stay within those declared surfaces, and hook installation uses machine-local overlay
files with absolute binary paths.

The CLI may format summaries and recovery hints, but it should not make merge, identity, lockfile,
or sync policy decisions. Those decisions belong in core APIs.

## Common Changes

Add a CLI command by extending `crates/agentmesh/src/main.rs`, delegating behavior to core where
possible, and adding a focused CLI test.

Add a protocol field in `crates/agentmesh-protocol`, then update the SDK dispatch and adapter
implementations that consume it.

Add a native runtime surface in the relevant adapter crate. Keep adapter code limited to native file
parsing, format translation, and hook overlay edits.

Add installer behavior under `installers/`. Wrapper smoke paths should remain network-free and
should not depend on repository-root Node or Python tooling. Installer integrity helpers should stay
usable before release artifacts exist; repository hooks are repinned by running `agentmesh upgrade`
after the binary path or hash changes.
