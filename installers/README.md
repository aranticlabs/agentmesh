# Installers

This directory holds packaging wrappers around the single AgentMesh binary.

- `npm/` contains the npm wrapper.
- `install.sh` contains the macOS/Linux curl installer entry point.
- `install.ps1` contains the Windows PowerShell installer entry point.

The wrappers are release installers. They resolve the current platform archive, verify it against
the published `SHA256SUMS` manifest, verify the manifest signature and Sigstore bundle with cosign,
and then install or delegate to the single binary.

Smoke checks are available without network access:

```bash
sh install.sh --smoke
sh npm/bin/agentmesh --smoke
```

The shell installer can also verify a local file hash, and all wrappers expose the upgrade/repin
reminder without network access:

```bash
sh install.sh --verify-sha256 <file> <expected-sha256>
sh install.sh --upgrade-help
sh npm/bin/agentmesh --upgrade-help
```
