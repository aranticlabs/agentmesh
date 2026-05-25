param(
    [ValidateSet("stable", "nightly")]
    [string]$Channel = $(if ($env:AGENTMESH_CHANNEL) { $env:AGENTMESH_CHANNEL } else { "stable" }),
    [string]$InstallDir = $env:AGENTMESH_INSTALL_DIR,
    [switch]$PrintPlatform,
    [switch]$PrintUrl,
    [switch]$Smoke,
    [switch]$UpgradeHelp,
    [switch]$Install
)

$ErrorActionPreference = "Stop"

$AgentMeshVersion = if ($env:AGENTMESH_VERSION) { $env:AGENTMESH_VERSION } else { "0.1.0" }
$BaseUrl = if ($env:AGENTMESH_BASE_URL) { $env:AGENTMESH_BASE_URL } else { "https://github.com/aranticlabs/agentmesh/releases/download" }
$CosignVersion = if ($env:AGENTMESH_COSIGN_VERSION) { $env:AGENTMESH_COSIGN_VERSION } else { "v2.6.3" }
$CosignIdentity = if ($env:AGENTMESH_COSIGN_CERTIFICATE_IDENTITY_REGEXP) { $env:AGENTMESH_COSIGN_CERTIFICATE_IDENTITY_REGEXP } else { "^https://github.com/aranticlabs/agentmesh/.github/workflows/release.yml@refs/tags/(v|agentmesh-v).*" }
$CosignIssuer = if ($env:AGENTMESH_COSIGN_CERTIFICATE_OIDC_ISSUER) { $env:AGENTMESH_COSIGN_CERTIFICATE_OIDC_ISSUER } else { "https://token.actions.githubusercontent.com" }

function Get-AgentMeshPlatform {
    if ($env:OS -ne "Windows_NT") {
        throw "install.ps1 supports Windows only; use install.sh on macOS and Linux"
    }

    switch ($env:PROCESSOR_ARCHITECTURE) {
        "AMD64" { return "x86_64-pc-windows-msvc" }
        default { throw "unsupported Windows architecture: $env:PROCESSOR_ARCHITECTURE" }
    }
}

function Get-ReleaseTag {
    switch ($Channel) {
        "stable" { return "agentmesh-v$AgentMeshVersion" }
        "nightly" { return "nightly" }
    }
}

function Get-ArtifactName {
    param([string]$Platform)
    switch ($Channel) {
        "stable" { return "agentmesh-v$AgentMeshVersion-$Platform.tar.gz" }
        "nightly" { return "agentmesh-nightly-$Platform.tar.gz" }
    }
}

function Join-Url {
    param([string]$Left, [string]$Right)
    return "$($Left.TrimEnd('/'))/$Right"
}

function Get-ArtifactUrl {
    $tag = Get-ReleaseTag
    $artifact = Get-ArtifactName -Platform (Get-AgentMeshPlatform)
    return (Join-Url (Join-Url $BaseUrl $tag) $artifact)
}

function Get-Sha256 {
    param([string]$Path)
    return (Get-FileHash -Path $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Save-Url {
    param([string]$Url, [string]$Path)
    if ($Url.StartsWith("file://")) {
        Copy-Item -LiteralPath ([uri]$Url).LocalPath -Destination $Path -Force
        return
    }
    Invoke-WebRequest -Uri $Url -OutFile $Path
}

function Get-ManifestHash {
    param([string]$ManifestPath, [string]$ArtifactName)
    foreach ($line in Get-Content -LiteralPath $ManifestPath) {
        $parts = $line.Trim() -split "\s+"
        if ($parts.Length -ge 2) {
            $name = Split-Path -Leaf $parts[1].TrimStart("*")
            if ($name -eq $ArtifactName) {
                return $parts[0].ToLowerInvariant()
            }
        }
    }
    throw "artifact $ArtifactName not found in SHA256SUMS"
}

function Get-CosignArtifactName {
    return "cosign-windows-amd64.exe"
}

function Get-CosignExpectedSha256 {
    if ($env:AGENTMESH_COSIGN_SHA256) {
        return $env:AGENTMESH_COSIGN_SHA256.ToLowerInvariant()
    }
    if ($CosignVersion -ne "v2.6.3") {
        throw "AGENTMESH_COSIGN_SHA256 is required when overriding AGENTMESH_COSIGN_VERSION"
    }
    return "2264ea5867077b9e070161648e8c18544decac351f5f3a7edaea43c233ce2e36"
}

function Get-CosignCommand {
    if ($env:AGENTMESH_COSIGN_BIN) {
        if (-not (Test-Path -LiteralPath $env:AGENTMESH_COSIGN_BIN -PathType Leaf)) {
            throw "AGENTMESH_COSIGN_BIN does not point to a file: $env:AGENTMESH_COSIGN_BIN"
        }
        return $env:AGENTMESH_COSIGN_BIN
    }

    $found = Get-Command cosign -ErrorAction SilentlyContinue
    if ($found) {
        return $found.Source
    }

    $artifact = Get-CosignArtifactName
    $cacheRoot = if ($env:AGENTMESH_COSIGN_DIR) {
        $env:AGENTMESH_COSIGN_DIR
    } else {
        Join-Path $env:LOCALAPPDATA "AgentMesh\cosign\$CosignVersion"
    }
    New-Item -ItemType Directory -Path $cacheRoot -Force | Out-Null
    $cosignPath = Join-Path $cacheRoot $artifact
    $expected = Get-CosignExpectedSha256

    if ((Test-Path -LiteralPath $cosignPath -PathType Leaf) -and ((Get-Sha256 $cosignPath) -eq $expected)) {
        return $cosignPath
    }

    $cosignBase = if ($env:AGENTMESH_COSIGN_BASE_URL) { $env:AGENTMESH_COSIGN_BASE_URL } else { "https://github.com/sigstore/cosign/releases/download/$CosignVersion" }
    $tmp = "$cosignPath.tmp"
    Save-Url -Url (Join-Url $cosignBase $artifact) -Path $tmp
    $actual = Get-Sha256 $tmp
    if ($actual -ne $expected) {
        Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
        throw "cosign sha256 mismatch"
    }
    Move-Item -LiteralPath $tmp -Destination $cosignPath -Force
    return $cosignPath
}

function Test-Sha256SumsSignature {
    param([string]$Manifest, [string]$Signature, [string]$Bundle)
    $cosign = Get-CosignCommand
    & $cosign verify-blob `
        --signature $Signature `
        --bundle $Bundle `
        --certificate-identity-regexp $CosignIdentity `
        --certificate-oidc-issuer $CosignIssuer `
        $Manifest
    if ($LASTEXITCODE -ne 0) {
        throw "SHA256SUMS signature verification failed"
    }
}

function Install-AgentMesh {
    $platform = Get-AgentMeshPlatform
    $tag = Get-ReleaseTag
    $artifact = Get-ArtifactName -Platform $platform
    $targetDir = if ($InstallDir) { $InstallDir } else { Join-Path $env:LOCALAPPDATA "Microsoft\WindowsApps" }
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null
    try {
        $archive = Join-Path $tmp $artifact
        $manifest = Join-Path $tmp "SHA256SUMS"
        $signature = Join-Path $tmp "SHA256SUMS.sig"
        $bundle = Join-Path $tmp "SHA256SUMS.bundle"
        $releaseBase = Join-Url $BaseUrl $tag

        Save-Url -Url (Join-Url $releaseBase $artifact) -Path $archive
        Save-Url -Url (Join-Url $releaseBase "SHA256SUMS") -Path $manifest
        Save-Url -Url (Join-Url $releaseBase "SHA256SUMS.sig") -Path $signature
        Save-Url -Url (Join-Url $releaseBase "SHA256SUMS.bundle") -Path $bundle

        Test-Sha256SumsSignature -Manifest $manifest -Signature $signature -Bundle $bundle
        $expected = Get-ManifestHash -ManifestPath $manifest -ArtifactName $artifact
        $actual = Get-Sha256 $archive
        if ($actual -ne $expected) {
            throw "sha256 mismatch for $artifact"
        }

        $extract = Join-Path $tmp "extract"
        New-Item -ItemType Directory -Path $extract -Force | Out-Null
        tar -xzf $archive -C $extract
        $binary = Get-ChildItem -Path $extract -Filter "agentmesh.exe" -Recurse | Select-Object -First 1
        if (-not $binary) {
            throw "archive did not contain agentmesh.exe"
        }
        New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
        Copy-Item -LiteralPath $binary.FullName -Destination (Join-Path $targetDir "agentmesh.exe") -Force
        Write-Host "agentmesh installed to $(Join-Path $targetDir "agentmesh.exe")"
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($PrintPlatform) {
    Write-Output (Get-AgentMeshPlatform)
    exit 0
}
if ($PrintUrl) {
    Write-Output (Get-ArtifactUrl)
    exit 0
}
if ($Smoke) {
    $platform = Get-AgentMeshPlatform
    $artifact = Get-ArtifactName -Platform $platform
    Write-Output "agentmesh Windows installer smoke ok (platform=$platform artifact=$artifact)"
    exit 0
}
if ($UpgradeHelp) {
    Write-Output "After replacing the AgentMesh binary, run agentmesh upgrade in each managed repository to repin hook integrity."
    exit 0
}

Install-AgentMesh
