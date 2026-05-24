# AgentMesh

AgentMesh is a Rust CLI for synchronizing project-level AI runtime context across coding tools.

The public documentation site is planned for [agentmesh.sh](https://agentmesh.sh).

## Development

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace
```

The workspace is pinned by [rust-toolchain.toml](rust-toolchain.toml).
