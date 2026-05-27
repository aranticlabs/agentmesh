.PHONY: help fmt fmt-check check clippy test build bench-check deny audit fuzz-check \
        installer-smoke ci-rust ci-supply-chain ci-installers ci release retag clean

GREEN := \033[0;32m
BLUE := \033[0;34m
YELLOW := \033[0;33m
RED := \033[0;31m
NC := \033[0m

help:
	@echo "$(BLUE)AgentMesh targets$(NC)"
	@echo "  make ci              Run the full local CI suite"
	@echo "  make ci-rust         Format, typecheck, lint, test, build, and bench-compile"
	@echo "  make ci-supply-chain Run dependency policy and advisory checks"
	@echo "  make ci-installers   Run installer smoke checks"
	@echo "  make build           Build the release binary"
	@echo "  make release v=X.Y.Z Bump Cargo versions, tag, and push a GitHub release"
	@echo "  make retag v=X.Y.Z   Bump Cargo versions and overwrite an existing GitHub release tag"

fmt:
	@cargo fmt --all

fmt-check:
	@cargo fmt --all -- --check

check:
	@cargo check --workspace --all-features

clippy:
	@cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	@cargo test --workspace --all-features

build:
	@cargo build --locked --release -p agentmesh

bench-check:
	@cargo bench --workspace --all-features --no-run

deny:
	@cargo deny check

audit:
	@cargo audit

fuzz-check:
	@cargo check --manifest-path fuzz/Cargo.toml --bins

installer-smoke:
	@sh installers/install.sh --smoke
	@sh installers/install.sh --upgrade-help

ci-rust: fmt-check check clippy test build bench-check fuzz-check
	@echo "$(GREEN)[SUCCESS]$(NC) Rust CI checks passed"

ci-supply-chain: deny audit
	@echo "$(GREEN)[SUCCESS]$(NC) Supply-chain checks passed"

ci-installers: installer-smoke
	@echo "$(GREEN)[SUCCESS]$(NC) Installer checks passed"

ci: ci-rust ci-supply-chain ci-installers
	@echo "$(GREEN)[SUCCESS]$(NC) Full CI suite passed"

release:
	@if [ -z "$(v)" ]; then \
		echo "$(RED)[ERROR]$(NC) Version required. Usage: make release v=X.Y.Z"; \
		exit 1; \
	fi
	@if ! printf '%s\n' "$(v)" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$$'; then \
		echo "$(RED)[ERROR]$(NC) Version must look like X.Y.Z"; \
		exit 1; \
	fi
	@set -e; \
	branch="$$(git rev-parse --abbrev-ref HEAD)"; \
	tag="v$(v)"; \
	if [ "$$branch" != "main" ]; then \
		echo "$(RED)[ERROR]$(NC) Releases must be tagged from main (currently on $$branch)"; \
		exit 1; \
	fi; \
	if ! git diff --quiet || ! git diff --cached --quiet || [ -n "$$(git status --porcelain)" ]; then \
		echo "$(RED)[ERROR]$(NC) Commit or stash changes before tagging a release."; \
		exit 1; \
	fi; \
	if git rev-parse "$$tag" >/dev/null 2>&1; then \
		echo "$(RED)[ERROR]$(NC) Local tag $$tag already exists."; \
		exit 1; \
	fi; \
	if git ls-remote --exit-code --tags origin "refs/tags/$$tag" >/dev/null 2>&1; then \
		echo "$(RED)[ERROR]$(NC) Remote tag $$tag already exists."; \
		exit 1; \
	fi; \
	echo "$(BLUE)================================================$(NC)"; \
	echo "$(BLUE)  AgentMesh Release $(v)$(NC)"; \
	echo "$(BLUE)================================================$(NC)"; \
	echo ""; \
	echo "  Branch: $$branch"; \
	echo "  Tag:    $$tag"; \
	echo "  Action: bump Cargo versions, commit, push main, then push tag"; \
	echo ""; \
	printf "$(YELLOW)Proceed? [y/N]$(NC) "; \
	read -r confirm; \
	if [ "$$confirm" != "y" ] && [ "$$confirm" != "Y" ]; then \
		echo "$(RED)[ABORT]$(NC) Release cancelled."; \
		exit 1; \
	fi; \
	echo "$(BLUE)[INFO]$(NC) Bumping Cargo workspace version to $(v)..."; \
	scripts/bump-release-version.sh "$(v)"; \
	echo "$(BLUE)[INFO]$(NC) Validating Cargo metadata..."; \
	cargo metadata --locked --format-version 1 >/dev/null; \
	git add Cargo.toml Cargo.lock .release-please-manifest.json; \
	if ! git diff --cached --quiet; then \
		git commit -m "chore: release $$tag"; \
	else \
		echo "$(BLUE)[INFO]$(NC) Cargo workspace version already matches $(v)."; \
	fi; \
	echo "$(BLUE)[INFO]$(NC) Pushing $$branch..."; \
	git push origin "$$branch"; \
	echo "$(BLUE)[INFO]$(NC) Creating and pushing $$tag..."; \
	git tag "$$tag"; \
	git push origin "$$tag"; \
	echo "$(GREEN)[SUCCESS]$(NC) Release tag $$tag pushed. GitHub Actions will build and publish the release assets."

retag:
	@if [ -z "$(v)" ]; then \
		echo "$(RED)[ERROR]$(NC) Version required. Usage: make retag v=X.Y.Z"; \
		exit 1; \
	fi
	@if ! printf '%s\n' "$(v)" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$$'; then \
		echo "$(RED)[ERROR]$(NC) Version must look like X.Y.Z"; \
		exit 1; \
	fi
	@set -e; \
	branch="$$(git rev-parse --abbrev-ref HEAD)"; \
	tag="v$(v)"; \
	if [ "$$branch" != "main" ]; then \
		echo "$(RED)[ERROR]$(NC) Releases must be retagged from main (currently on $$branch)"; \
		exit 1; \
	fi; \
	if ! git diff --quiet || ! git diff --cached --quiet || [ -n "$$(git status --porcelain)" ]; then \
		echo "$(RED)[ERROR]$(NC) Commit or stash changes before retagging a release."; \
		exit 1; \
	fi; \
	if ! git ls-remote --exit-code --tags origin "refs/tags/$$tag" >/dev/null 2>&1; then \
		echo "$(RED)[ERROR]$(NC) Remote tag $$tag does not exist; use make release v=$(v) for a new release."; \
		exit 1; \
	fi; \
	echo "$(BLUE)================================================$(NC)"; \
	echo "$(BLUE)  AgentMesh Retag $(v)$(NC)"; \
	echo "$(BLUE)================================================$(NC)"; \
	echo ""; \
	echo "  Branch: $$branch"; \
	echo "  Tag:    $$tag"; \
	echo "  Action: bump Cargo versions, commit, push main, then overwrite existing tag"; \
	echo ""; \
	printf "$(YELLOW)Proceed? [y/N]$(NC) "; \
	read -r confirm; \
	if [ "$$confirm" != "y" ] && [ "$$confirm" != "Y" ]; then \
		echo "$(RED)[ABORT]$(NC) Retag cancelled."; \
		exit 1; \
	fi; \
	echo "$(BLUE)[INFO]$(NC) Bumping Cargo workspace version to $(v)..."; \
	scripts/bump-release-version.sh "$(v)"; \
	echo "$(BLUE)[INFO]$(NC) Validating Cargo metadata..."; \
	cargo metadata --locked --format-version 1 >/dev/null; \
	git add Cargo.toml Cargo.lock .release-please-manifest.json; \
	if ! git diff --cached --quiet; then \
		git commit -m "chore: release $$tag"; \
	else \
		echo "$(BLUE)[INFO]$(NC) Cargo workspace version already matches $(v)."; \
	fi; \
	echo "$(BLUE)[INFO]$(NC) Pushing $$branch to origin..."; \
	git push origin "$$branch"; \
	echo "$(BLUE)[INFO]$(NC) Retagging $$tag..."; \
	git tag -d "$$tag" 2>/dev/null || true; \
	git push origin ":refs/tags/$$tag" 2>/dev/null || true; \
	git tag "$$tag"; \
	git push origin "$$tag"; \
	echo "$(GREEN)[SUCCESS]$(NC) Tag $$tag re-pushed to origin. GitHub Actions will rebuild the release assets."

clean:
	@rm -rf target fuzz/target dist
