# Installers

This directory holds packaging wrappers around the single AgentMesh binary.

- `npm/` contains the npm wrapper.
- `pip/` contains the pipx wrapper.
- `homebrew-formula/` contains the Homebrew formula source.
- `install.sh` contains the curl installer entry point.

The wrappers are release installers. They resolve the current platform archive, verify it against
the published `SHA256SUMS` manifest, verify the manifest signature with cosign, and then install or
delegate to the single binary.

Smoke checks are available without network access:

```bash
sh install.sh --smoke
sh npm/bin/agentmesh --smoke
python -c 'import sys; from pathlib import Path; sys.path.insert(0, str(Path("pip"))); import agentmesh_wrapper; raise SystemExit(agentmesh_wrapper.main())' --smoke
```

The shell installer can also verify a local file hash, and all wrappers expose the upgrade/repin
reminder without network access:

```bash
sh install.sh --verify-sha256 <file> <expected-sha256>
sh install.sh --upgrade-help
sh npm/bin/agentmesh --upgrade-help
python -c 'import sys; from pathlib import Path; sys.path.insert(0, str(Path("pip"))); import agentmesh_wrapper; raise SystemExit(agentmesh_wrapper.main())' --upgrade-help
```
