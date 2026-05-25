.PHONY: help fmt fmt-check check clippy test build bench-check deny audit fuzz-check \
        website-static website-render installer-smoke ci-rust ci-supply-chain ci-installers ci clean

GREEN := \033[0;32m
BLUE := \033[0;34m
NC := \033[0m

help:
	@echo "$(BLUE)AgentMesh targets$(NC)"
	@echo "  make ci              Run the full local CI suite"
	@echo "  make ci-rust         Format, typecheck, lint, test, build, and bench-compile"
	@echo "  make ci-supply-chain Run dependency policy and advisory checks"
	@echo "  make ci-installers   Run website and installer smoke checks"
	@echo "  make build           Build the release binary"

fmt:
	@cargo fmt --all

fmt-check:
	@cargo fmt --all -- --check

check:
	@cargo check --workspace --all-features

clippy:
	@cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	@cargo test --workspace --all-features -- --test-threads=1

build:
	@cargo build --locked --release -p agentmesh-cli

bench-check:
	@cargo bench --workspace --all-features --no-run

deny:
	@cargo deny check

audit:
	@cargo audit

fuzz-check:
	@cargo check --manifest-path fuzz/Cargo.toml --bins

website-static:
	@python3 website/check-static.py

website-render:
	@tmp="$$(mktemp -d)"; \
	trap 'rm -rf "$$tmp"' EXIT; \
	npm install --prefix "$$tmp/playwright" --no-save playwright@1.56.1; \
	"$$tmp/playwright/node_modules/.bin/playwright" install --with-deps chromium; \
	NODE_PATH="$$tmp/playwright/node_modules" node website/check-render.cjs

installer-smoke:
	@sh installers/install.sh --smoke
	@sh installers/install.sh --upgrade-help
	@node --check installers/npm/scripts/install.js
	@AGENTMESH_NPM_POSTINSTALL_SMOKE=1 node installers/npm/scripts/install.js
	@sh installers/npm/bin/agentmesh --smoke
	@sh installers/npm/bin/agentmesh --upgrade-help

ci-rust: fmt-check check clippy test build bench-check fuzz-check
	@echo "$(GREEN)[SUCCESS]$(NC) Rust CI checks passed"

ci-supply-chain: deny audit
	@echo "$(GREEN)[SUCCESS]$(NC) Supply-chain checks passed"

ci-installers: website-static installer-smoke
	@echo "$(GREEN)[SUCCESS]$(NC) Installer checks passed"

ci: ci-rust ci-supply-chain ci-installers
	@echo "$(GREEN)[SUCCESS]$(NC) Full CI suite passed"

clean:
	@rm -rf target dist
