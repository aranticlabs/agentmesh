# pipx Installer

This Python package wrapper installs and delegates to the AgentMesh binary.

The wrapper exposes an `agentmesh` console script through `agentmesh_wrapper.entry`. When the cached
binary is missing, it downloads the platform release archive, verifies it against `SHA256SUMS`,
verifies the cosign signature bundle, and then delegates invocations to the installed binary.
`agentmesh --smoke` exits 0 for package smoke checks, and `agentmesh --upgrade-help` prints the
local binary-upgrade and integrity-repin flow.
