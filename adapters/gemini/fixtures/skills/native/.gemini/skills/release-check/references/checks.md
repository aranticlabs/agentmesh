# Release Checks

- cargo fmt --all
- cargo clippy --workspace --all-targets --all-features -- -D warnings
- cargo test --workspace --all-features
