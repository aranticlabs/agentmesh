# Installers

This directory holds packaging wrappers around the single AgentMesh binary.

| Path          | Purpose                                        |
| ------------- | ---------------------------------------------- |
| `install.sh`  | macOS and Linux installer                      |
| `install.ps1` | Windows installer                              |
| `npm/`        | npm wrapper package (`@aranticlabs/agentmesh`) |

Release installers resolve the current platform archive, verify it against the published
`SHA256SUMS` manifest, verify the manifest signature and Sigstore bundle with cosign, and install
the single binary.

Public docs: [agentmesh.sh/installation/curl](https://agentmesh.sh/installation/curl)

## Install

### macOS / Linux

Published one-liner:

```bash
curl -fsSL https://agentmesh.sh/install.sh | sh
```

From a clone of this repository:

```bash
sh installers/install.sh
```

### Windows

Published one-liner (PowerShell):

```powershell
irm https://agentmesh.sh/install.ps1 | iex
```

From a clone of this repository:

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File installers/install.ps1
```

### npm

The npm wrapper is not published yet. When available:

```bash
npm install -g @aranticlabs/agentmesh
```

See `npm/README.md` for package layout.

### Build from source

For contributors and local development only:

```bash
cargo build --release -p agentmesh-cli
```

## Upgrade

Re-run the installer for your platform, then repin hooks in initialized repositories:

```bash
curl -fsSL https://agentmesh.sh/install.sh | sh   # macOS / Linux
agentmesh upgrade
```

```powershell
irm https://agentmesh.sh/install.ps1 | iex        # Windows
agentmesh upgrade
```

When the npm package is available:

```bash
npm update -g @aranticlabs/agentmesh
agentmesh upgrade
```

## Uninstall

From each initialized repository:

```bash
agentmesh uninstall --yes
```

Then remove the binary:

| Install method             | Command                                            |
| -------------------------- | -------------------------------------------------- |
| curl / `install.sh`        | `rm ~/.local/bin/agentmesh` (or your install path) |
| PowerShell / `install.ps1` | Remove `agentmesh.exe` from your install directory |
| npm                        | `npm uninstall -g @aranticlabs/agentmesh`          |

## Developer checks

Smoke checks run without network access:

```bash
sh installers/install.sh --smoke
sh installers/install.sh --upgrade-help
sh installers/npm/bin/agentmesh --smoke
sh installers/npm/bin/agentmesh --upgrade-help
pwsh -NoProfile -ExecutionPolicy Bypass -File installers/install.ps1 -Smoke
pwsh -NoProfile -ExecutionPolicy Bypass -File installers/install.ps1 -UpgradeHelp
```

The shell installer can verify local artifacts:

```bash
sh installers/install.sh --verify-sha256 <file> <expected-sha256>
```

Run the full installer smoke suite from the repository root:

```bash
make ci-installers
```
