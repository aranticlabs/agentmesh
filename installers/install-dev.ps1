$ErrorActionPreference = "Stop"
$env:AGENTMESH_CHANNEL = "dev"
if (-not $env:AGENTMESH_COSIGN_CERTIFICATE_IDENTITY_REGEXP) {
    $env:AGENTMESH_COSIGN_CERTIFICATE_IDENTITY_REGEXP = "^https://github.com/aranticlabs/agentmesh/.github/workflows/dev-release.yml@refs/heads/dev$"
}

$localInstaller = if ($PSScriptRoot) { Join-Path $PSScriptRoot "install.ps1" } else { $null }
if ($localInstaller -and (Test-Path -LiteralPath $localInstaller -PathType Leaf)) {
    & powershell -NoProfile -ExecutionPolicy Bypass -File $localInstaller @args
    exit $LASTEXITCODE
}

$installerUrl = if ($env:AGENTMESH_DEV_INSTALLER_URL) {
    $env:AGENTMESH_DEV_INSTALLER_URL
} else {
    "https://raw.githubusercontent.com/aranticlabs/agentmesh/dev/installers/install.ps1"
}
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) "agentmesh-install-dev-$([guid]::NewGuid()).ps1"
try {
    Invoke-WebRequest -Uri $installerUrl -OutFile $tmp
    & powershell -NoProfile -ExecutionPolicy Bypass -File $tmp @args
    exit $LASTEXITCODE
} finally {
    Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
}
