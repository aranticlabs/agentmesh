# npm Installer

This package wrapper installs and delegates to the AgentMesh binary.

The package shape is intentionally small:

- `package.json` exposes an `agentmesh` bin.
- `scripts/install.js` runs the verified binary install flow during npm postinstall.
- `bin/agentmesh` downloads the platform release archive when a cached binary is missing.
- Downloaded archives are verified against `SHA256SUMS` and the cosign signature.
- `bin/agentmesh --smoke` exits 0 for package smoke checks.
- `bin/agentmesh --upgrade-help` prints the local binary-upgrade and integrity-repin flow.
