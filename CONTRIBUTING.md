# Contributing

AgentMesh uses conventional commits for change history and release automation.

Examples:

- `feat: add lockfile reader`
- `fix: preserve runtime overlay entries`
- `docs: clarify adapter protocol`
- `chore: update CI tooling`

Before opening a pull request, run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Keep changes focused, preserve user-authored files, and follow the Rust standards documented for this repository.
