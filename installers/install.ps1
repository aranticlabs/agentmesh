param(
    [ValidateSet("stable", "nightly")]
    [string]$Channel = $(if ($env:AGENTMESH_CHANNEL) { $env:AGENTMESH_CHANNEL } else { "stable" }),
    [string]$InstallDir = $env:AGENTMESH_INSTALL_DIR,
    [switch]$PrintPlatform,
    [switch]$PrintUrl,
    [switch]$PrintBanner,
    [switch]$Smoke,
    [switch]$UpgradeHelp,
    [switch]$Install
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$AgentMeshVersion = if ($env:AGENTMESH_VERSION) { $env:AGENTMESH_VERSION } else { "0.1.0" }
$BaseUrl = if ($env:AGENTMESH_BASE_URL) { $env:AGENTMESH_BASE_URL } else { "https://github.com/aranticlabs/agentmesh/releases/download" }
$CosignVersion = if ($env:AGENTMESH_COSIGN_VERSION) { $env:AGENTMESH_COSIGN_VERSION } else { "v2.6.3" }
$CosignIdentity = if ($env:AGENTMESH_COSIGN_CERTIFICATE_IDENTITY_REGEXP) { $env:AGENTMESH_COSIGN_CERTIFICATE_IDENTITY_REGEXP } else { "^https://github.com/aranticlabs/agentmesh/.github/workflows/release.yml@refs/tags/v.*" }
$CosignIssuer = if ($env:AGENTMESH_COSIGN_CERTIFICATE_OIDC_ISSUER) { $env:AGENTMESH_COSIGN_CERTIFICATE_OIDC_ISSUER } else { "https://token.actions.githubusercontent.com" }
$script:SpinnerState = $null
$script:SpinnerThread = $null

function Test-AgentMeshColor {
    return -not [Console]::IsErrorRedirected -and -not $env:NO_COLOR -and $env:AGENTMESH_NO_COLOR -ne "1"
}

function Test-AgentMeshSpinner {
    return -not [Console]::IsErrorRedirected -and $env:AGENTMESH_NO_SPINNER -ne "1" -and $PSVersionTable.PSEdition -ne "Desktop"
}

function Write-AgentMeshColoredError {
    param([string]$Text, [ConsoleColor]$Color)
    if (Test-AgentMeshColor) {
        $previous = [Console]::ForegroundColor
        [Console]::ForegroundColor = $Color
        [Console]::Error.Write($Text)
        [Console]::ForegroundColor = $previous
    } else {
        [Console]::Error.Write($Text)
    }
}

function Get-AgentMeshSuccessMark {
    return [string][char]0x2713
}

function Get-AgentMeshFailureMark {
    return [string][char]0x2717
}

function Join-AgentMeshChars {
    param([int[]]$CodePoints)
    $builder = New-Object System.Text.StringBuilder
    foreach ($codePoint in $CodePoints) {
        [void]$builder.Append([char]$codePoint)
    }
    return $builder.ToString()
}

function Start-AgentMeshSpinner {
    param([string]$Message)
    if (-not (Test-AgentMeshSpinner)) {
        Write-Host "$Message..."
        return
    }

    $script:SpinnerState = [hashtable]::Synchronized(@{
        Active = $true
        Message = $Message
        UseColor = (Test-AgentMeshColor)
    })
    $threadStart = [System.Threading.ParameterizedThreadStart]{
        param($State)
        $frames = @(
            [string][char]0x280B,
            [string][char]0x2819,
            [string][char]0x281A,
            [string][char]0x281E,
            [string][char]0x2816,
            [string][char]0x2826,
            [string][char]0x2834,
            [string][char]0x2832,
            [string][char]0x2833,
            [string][char]0x2813
        )
        $index = 0
        while ($State.Active) {
            [Console]::Error.Write("`r")
            if ($State.UseColor) {
                $previous = [Console]::ForegroundColor
                [Console]::ForegroundColor = [ConsoleColor]::Magenta
                [Console]::Error.Write($frames[$index % $frames.Count])
                [Console]::ForegroundColor = $previous
            } else {
                [Console]::Error.Write($frames[$index % $frames.Count])
            }
            [Console]::Error.Write(" $($State.Message)")
            [System.Threading.Thread]::Sleep(80)
            $index++
        }
    }
    $script:SpinnerThread = New-Object System.Threading.Thread -ArgumentList $threadStart
    $script:SpinnerThread.IsBackground = $true
    $script:SpinnerThread.Start($script:SpinnerState)
}

function Stop-AgentMeshSpinner {
    param([string]$Message, [string]$Status)
    $color = if ($Status -eq (Get-AgentMeshFailureMark)) { [ConsoleColor]::Red } else { [ConsoleColor]::Green }
    if ($script:SpinnerState -and $script:SpinnerThread) {
        $script:SpinnerState.Active = $false
        $script:SpinnerThread.Join()
        [Console]::Error.Write("`r")
        [Console]::Error.Write(" " * ([Math]::Max(80, $Message.Length + 8)))
        [Console]::Error.Write("`r")
        Write-AgentMeshColoredError -Text $Status -Color $color
        [Console]::Error.WriteLine(" $Message")
        $script:SpinnerState = $null
        $script:SpinnerThread = $null
    } else {
        Write-Host "$Status $Message"
    }
}

function Invoke-InstallStep {
    param([string]$Message, [scriptblock]$Action)
    Start-AgentMeshSpinner -Message $Message
    try {
        & $Action
        Stop-AgentMeshSpinner -Message $Message -Status (Get-AgentMeshSuccessMark)
    } catch {
        Stop-AgentMeshSpinner -Message $Message -Status (Get-AgentMeshFailureMark)
        throw
    }
}

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
        "stable" { return "v$AgentMeshVersion" }
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
    $output = & $cosign verify-blob `
        --signature $Signature `
        --bundle $Bundle `
        --certificate-identity-regexp $CosignIdentity `
        --certificate-oidc-issuer $CosignIssuer `
        $Manifest 2>&1
    if ($LASTEXITCODE -ne 0) {
        $output | Write-Error
        throw "SHA256SUMS signature verification failed"
    }
}

function Show-InstallSuccess {
    param([string]$BinaryPath, [string]$Tag)
    Write-Host ""
    Write-Host (Join-AgentMeshChars @(0x0020, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x0020, 0x0020, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x0020, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2588, 0x2557, 0x0020, 0x0020, 0x0020, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2588, 0x2557, 0x0020, 0x0020, 0x0020, 0x2588, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2557, 0x0020, 0x0020, 0x2588, 0x2588, 0x2557))
    Write-Host (Join-AgentMeshChars @(0x2588, 0x2588, 0x2554, 0x2550, 0x2550, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2554, 0x2550, 0x2550, 0x2550, 0x2550, 0x255D, 0x0020, 0x2588, 0x2588, 0x2554, 0x2550, 0x2550, 0x2550, 0x2550, 0x255D, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551, 0x255A, 0x2550, 0x2550, 0x2588, 0x2588, 0x2554, 0x2550, 0x2550, 0x255D, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x0020, 0x2588, 0x2588, 0x2588, 0x2588, 0x2551, 0x2588, 0x2588, 0x2554, 0x2550, 0x2550, 0x2550, 0x2550, 0x255D, 0x2588, 0x2588, 0x2554, 0x2550, 0x2550, 0x2550, 0x2550, 0x255D, 0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551))
    Write-Host (Join-AgentMeshChars @(0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2551, 0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x2588, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x0020, 0x0020, 0x2588, 0x2588, 0x2554, 0x2588, 0x2588, 0x2557, 0x0020, 0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x0020, 0x2588, 0x2588, 0x2554, 0x2588, 0x2588, 0x2588, 0x2588, 0x2554, 0x2588, 0x2588, 0x2551, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x0020, 0x0020, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2551))
    Write-Host (Join-AgentMeshChars @(0x2588, 0x2588, 0x2554, 0x2550, 0x2550, 0x2588, 0x2588, 0x2551, 0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551, 0x2588, 0x2588, 0x2554, 0x2550, 0x2550, 0x255D, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551, 0x255A, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551, 0x255A, 0x2588, 0x2588, 0x2554, 0x255D, 0x2588, 0x2588, 0x2551, 0x2588, 0x2588, 0x2554, 0x2550, 0x2550, 0x255D, 0x0020, 0x0020, 0x255A, 0x2550, 0x2550, 0x2550, 0x2550, 0x2588, 0x2588, 0x2551, 0x2588, 0x2588, 0x2554, 0x2550, 0x2550, 0x2588, 0x2588, 0x2551))
    Write-Host (Join-AgentMeshChars @(0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551, 0x255A, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2554, 0x255D, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2551, 0x0020, 0x255A, 0x2588, 0x2588, 0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551, 0x0020, 0x255A, 0x2550, 0x255D, 0x0020, 0x2588, 0x2588, 0x2551, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2557, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2588, 0x2551, 0x2588, 0x2588, 0x2551, 0x0020, 0x0020, 0x2588, 0x2588, 0x2551))
    Write-Host (Join-AgentMeshChars @(0x255A, 0x2550, 0x255D, 0x0020, 0x0020, 0x255A, 0x2550, 0x255D, 0x0020, 0x255A, 0x2550, 0x2550, 0x2550, 0x2550, 0x2550, 0x255D, 0x0020, 0x255A, 0x2550, 0x2550, 0x2550, 0x2550, 0x2550, 0x2550, 0x255D, 0x255A, 0x2550, 0x255D, 0x0020, 0x0020, 0x255A, 0x2550, 0x2550, 0x2550, 0x255D, 0x0020, 0x0020, 0x0020, 0x255A, 0x2550, 0x255D, 0x0020, 0x0020, 0x0020, 0x255A, 0x2550, 0x255D, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x255A, 0x2550, 0x255D, 0x255A, 0x2550, 0x2550, 0x2550, 0x2550, 0x2550, 0x2550, 0x255D, 0x255A, 0x2550, 0x2550, 0x2550, 0x2550, 0x2550, 0x2550, 0x255D, 0x255A, 0x2550, 0x255D, 0x0020, 0x0020, 0x255A, 0x2550, 0x255D))
    Write-Host (Join-AgentMeshChars @(0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0020, 0x0062, 0x0079, 0x0020, 0x0041, 0x0072, 0x0061, 0x006E, 0x0074, 0x0069, 0x0063, 0x0020, 0x0044, 0x0069, 0x0067, 0x0069, 0x0074, 0x0061, 0x006C))
    Write-Host ""
    Write-Host "AgentMesh is installed and ready."
    Write-Host ""
    Write-Host "Installed:"
    Write-Host "  Binary:  $BinaryPath"
    Write-Host "  Channel: $Channel ($Tag)"
    Write-Host ""
    Write-Host "Next steps in a repository:"
    Write-Host "  1. agentmesh scan"
    Write-Host "     See which runtimes and instruction files AgentMesh detects."
    Write-Host "  2. agentmesh init"
    Write-Host "     Set up project sync, lockfile state, and runtime hooks."
    Write-Host "  3. agentmesh status"
    Write-Host "     Confirm the mesh is healthy before committing changes."
    Write-Host ""
    Write-Host "Docs: https://agentmesh.sh/docs/"
    Write-Host ""
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

        Invoke-InstallStep -Message "Resolving AgentMesh release $tag" -Action {
            Save-Url -Url (Join-Url $releaseBase "SHA256SUMS") -Path $manifest
        }
        Invoke-InstallStep -Message "Downloading signature metadata" -Action {
            Save-Url -Url (Join-Url $releaseBase "SHA256SUMS.sig") -Path $signature
        }
        Invoke-InstallStep -Message "Downloading transparency bundle" -Action {
            Save-Url -Url (Join-Url $releaseBase "SHA256SUMS.bundle") -Path $bundle
        }
        Invoke-InstallStep -Message "Verifying signed checksum manifest" -Action {
            Test-Sha256SumsSignature -Manifest $manifest -Signature $signature -Bundle $bundle
        }
        Invoke-InstallStep -Message "Downloading AgentMesh for $platform" -Action {
            Save-Url -Url (Join-Url $releaseBase $artifact) -Path $archive
        }
        Invoke-InstallStep -Message "Verifying AgentMesh archive checksum" -Action {
            $expected = Get-ManifestHash -ManifestPath $manifest -ArtifactName $artifact
            $actual = Get-Sha256 $archive
            if ($actual -ne $expected) {
                throw "sha256 mismatch for $artifact"
            }
        }
        $extract = Join-Path $tmp "extract"
        New-Item -ItemType Directory -Path $extract -Force | Out-Null
        Invoke-InstallStep -Message "Extracting AgentMesh binary" -Action {
            tar -xzf $archive -C $extract
            if ($LASTEXITCODE -ne 0) {
                throw "failed to extract $artifact"
            }
        }
        $binary = Get-ChildItem -Path $extract -Filter "agentmesh.exe" -Recurse | Select-Object -First 1
        if (-not $binary) {
            throw "archive did not contain agentmesh.exe"
        }
        New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
        $targetBinary = Join-Path $targetDir "agentmesh.exe"
        Invoke-InstallStep -Message "Installing AgentMesh into $targetDir" -Action {
            Copy-Item -LiteralPath $binary.FullName -Destination $targetBinary -Force
        }
        Show-InstallSuccess -BinaryPath $targetBinary -Tag $tag
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
if ($PrintBanner) {
    Show-InstallSuccess -BinaryPath "<agentmesh-binary>" -Tag (Get-ReleaseTag)
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
