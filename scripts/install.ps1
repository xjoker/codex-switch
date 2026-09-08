# codex-switch installer / uninstaller for Windows
# Usage:
#   irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
#   $env:CS_DEV="1"; irm https://github.com/xjoker/codex-switch/releases/download/dev/install.ps1 | iex
#   $env:CS_VERSION="20260712.1.0"; irm .../install.ps1 | iex # install specific version
#   $env:CS_UNINSTALL="1"; irm .../install.ps1 | iex         # uninstall codex-switch

$ErrorActionPreference = "Stop"
$Repo = "xjoker/codex-switch"
$ProvenanceAsset = "codex-switch-build-provenance.json"
$ReleaseWorkflow = "xjoker/codex-switch/.github/workflows/release.yml"
$BinaryName = "codex-switch.exe"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\codex-switch"
$DataDir = Join-Path $env:USERPROFILE ".codex-switch"

# Optionally verify the downloaded archive's build provenance. When GitHub CLI
# attestation support is available, this checks the repository and release
# workflow recorded for an artifact built on a GitHub-hosted runner. It does
# not prevent replay of an older valid artifact; the checksum still validates
# the downloaded bytes against the checksum file from the selected release.
# Offline `--bundle` mode needs neither `gh auth login` nor any GitHub API call.
# Without a GitHub CLI that supports attestation the archive is still checksum
# verified; set CS_REQUIRE_PROVENANCE=1 to make a missing verifier a hard error.
function Test-BuildProvenance {
    param(
        [string]$ArchivePath,
        [string]$DownloadUrl,
        [string]$TmpDir,
        [string]$AssetName
    )
    $require = $env:CS_REQUIRE_PROVENANCE -eq "1"

    $hasAttestation = $false
    if (Get-Command gh -ErrorAction SilentlyContinue) {
        & gh attestation --help *> $null
        $hasAttestation = ($LASTEXITCODE -eq 0)
    }
    if (-not $hasAttestation) {
        if ($require) {
            Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
            throw "CS_REQUIRE_PROVENANCE=1 but a GitHub CLI with attestation support was not found. Install https://cli.github.com/ and retry."
        }
        Write-Warning "GitHub CLI with attestation support not found; skipping build-provenance verification (the SHA-256 checksum was still verified). Install https://cli.github.com/ and re-run, or set CS_REQUIRE_PROVENANCE=1 to require it."
        return
    }

    $BundleUrl = ($DownloadUrl -replace '/[^/]+$', "/$ProvenanceAsset")
    $BundlePath = Join-Path $TmpDir $ProvenanceAsset
    try {
        Invoke-WebRequest -Uri $BundleUrl -OutFile $BundlePath -UseBasicParsing
    } catch {
        if ($require) {
            Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
            throw "CS_REQUIRE_PROVENANCE=1 but the build-provenance bundle could not be downloaded from $BundleUrl."
        }
        Write-Warning "Could not download the build-provenance bundle ($BundleUrl); skipping provenance verification (the SHA-256 checksum was still verified)."
        return
    }

    & gh attestation verify $ArchivePath --bundle $BundlePath --repo $Repo --signer-workflow $ReleaseWorkflow --deny-self-hosted-runners *> $null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "[info]  Build provenance verified: $AssetName" -ForegroundColor Blue
    } else {
        Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
        throw "Build-provenance verification failed for $AssetName; refusing to install. The artifact is not attested as built by $ReleaseWorkflow."
    }
}

# ── Uninstall ────────────────────────────────────────────
if ($env:CS_UNINSTALL -eq "1") {
    Write-Host "[info]  Uninstalling codex-switch..." -ForegroundColor Blue

    $BinPath = Join-Path $InstallDir $BinaryName
    $ServiceUninstallFailed = $false
    # SCHED_E_TASK_NOT_RUNNING (0x8004130B) is reported as a signed Int32.
    $TaskNotRunningHResult = [int64]-2147216629
    $TaskQueryOutput = & schtasks.exe /Query /FO CSV /NH /HRESULT 2>&1
    $QueryHResult = [int64]$LASTEXITCODE
    if ($QueryHResult -ne 0) {
        Write-Warning "Failed to inspect Windows scheduled tasks (HRESULT $QueryHResult)."
        $ServiceUninstallFailed = $true
    } elseif ($null -ne ($TaskQueryOutput |
            ConvertFrom-Csv -Header TaskName, NextRunTime, Status |
            Where-Object { $_.TaskName -eq '\codex-switch-daemon' })) {
        & schtasks.exe /End /TN "\codex-switch-daemon" /HRESULT 2>$null | Out-Null
        $EndHResult = [int64]$LASTEXITCODE
        if ($EndHResult -ne 0 -and $EndHResult -ne $TaskNotRunningHResult) {
            Write-Warning "Failed to stop Windows scheduled task \codex-switch-daemon."
            $ServiceUninstallFailed = $true
        } else {
            & schtasks.exe /Delete /TN "\codex-switch-daemon" /F
            if ($LASTEXITCODE -ne 0) {
                Write-Warning "Failed to delete Windows scheduled task \codex-switch-daemon."
                $ServiceUninstallFailed = $true
            } else {
                Write-Host "[info]  Removed daemon scheduled task." -ForegroundColor Blue
            }
        }
    }

    if ($ServiceUninstallFailed) {
        throw "Daemon service cleanup failed; binary and data were kept. Resolve the service error and retry uninstall."
    }

    # Remove binary
    if (Test-Path $BinPath) {
        Remove-Item -Force $BinPath
        Write-Host "[info]  Removed $BinPath" -ForegroundColor Blue
    }

    # Remove install directory if empty
    if ((Test-Path $InstallDir) -and @(Get-ChildItem $InstallDir).Count -eq 0) {
        Remove-Item -Force $InstallDir
    }

    # Remove from PATH. Compared entry by entry rather than with -like: the
    # pattern operators treat [ and ] in a path (a username can contain them) as
    # wildcards, and a substring match would also fire on an unrelated directory
    # that merely starts with this one. Empty entries are dropped on the way out
    # because Windows resolves them to the current working directory.
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathEntries = @($UserPath -split ";" | Where-Object { $_.Trim() -ne "" })
    if ($PathEntries -contains $InstallDir) {
        $NewPath = ($PathEntries | Where-Object { $_ -ne $InstallDir }) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        Write-Host "[info]  Removed $InstallDir from user PATH" -ForegroundColor Blue
    }

    # Ask about data directory
    if (Test-Path $DataDir) {
        $answer = Read-Host "[info]  Remove data directory ${DataDir}? [y/N]"
        if ($answer -match "^[yY]") {
            Remove-Item -Recurse -Force $DataDir
            Write-Host "[info]  Removed $DataDir" -ForegroundColor Blue
        } else {
            Write-Host "[info]  Kept $DataDir" -ForegroundColor Blue
        }
    }

    Write-Host "[info]  codex-switch has been uninstalled." -ForegroundColor Blue
    return
}

# ── Install ──────────────────────────────────────────────

# Detect architecture
$Arch = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") { "arm64" } else { "amd64" }
$AssetName = "cs-windows-${Arch}.zip"

# Determine version / channel
$UseDev = $env:CS_DEV -eq "1"
if ($UseDev) {
    $Version = "dev"
    $DownloadUrl = "https://github.com/$Repo/releases/download/dev/$AssetName"
} else {
    $Version = if ($env:CS_VERSION) { $env:CS_VERSION } else { "latest" }
    if ($Version -eq "latest") {
        $DownloadUrl = "https://github.com/$Repo/releases/latest/download/$AssetName"
    } else {
        $DownloadUrl = "https://github.com/$Repo/releases/download/v$Version/$AssetName"
    }
}

Write-Host "[info]  Detected: windows/$Arch" -ForegroundColor Blue
Write-Host "[info]  Downloading: $DownloadUrl" -ForegroundColor Blue

# Download
$TmpDir = Join-Path $env:TEMP "cs-install-$(Get-Random)"
New-Item -ItemType Directory -Path $TmpDir -Force | Out-Null
$ZipPath = Join-Path $TmpDir $AssetName
$ChecksumUrl = "$DownloadUrl.sha256"
$ChecksumPath = "$ZipPath.sha256"

try {
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath -UseBasicParsing
    Invoke-WebRequest -Uri $ChecksumUrl -OutFile $ChecksumPath -UseBasicParsing
} catch {
    Write-Host "[error] Archive or checksum download failed: $_" -ForegroundColor Red
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    throw "Archive or checksum download failed: $_"
}

# Verify checksum before extracting any downloaded content
$ChecksumText = (Get-Content -LiteralPath $ChecksumPath -Raw).Trim()
$ChecksumPattern = '^(?<hash>[0-9A-Fa-f]{64})\s+\*?(?<file>\S+)$'
if ($ChecksumText -notmatch $ChecksumPattern -or (Split-Path -Leaf $Matches.file) -ne $AssetName) {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    throw "Invalid or empty checksum file for $AssetName."
}

$ExpectedSha256 = $Matches.hash.ToUpperInvariant()
$ActualSha256 = (Get-FileHash -LiteralPath $ZipPath -Algorithm SHA256).Hash.ToUpperInvariant()
if ($ActualSha256 -ne $ExpectedSha256) {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
    throw "Checksum mismatch for $AssetName; refusing to extract it."
}
Write-Host "[info]  Checksum verified: $AssetName" -ForegroundColor Blue

# Verify build provenance (Sigstore attestation) before extracting.
Test-BuildProvenance -ArchivePath $ZipPath -DownloadUrl $DownloadUrl -TmpDir $TmpDir -AssetName $AssetName

# Extract
Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force

# Install
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
Move-Item -Path (Join-Path $TmpDir $BinaryName) -Destination (Join-Path $InstallDir $BinaryName) -Force

# Add to PATH if not already present.
#
# Rebuilt from entries instead of concatenating "$UserPath;$InstallDir": when the
# user has no User-scoped Path, or it ends with a separator, that concatenation
# produces an empty PATH element. Windows resolves an empty element to the
# current working directory when it searches for an executable, so the installer
# would leave every directory the user later cd's into on the search path for
# every command they run — a persistent change outliving the tool itself.
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$PathEntries = @($UserPath -split ";" | Where-Object { $_.Trim() -ne "" })
if ($PathEntries -notcontains $InstallDir) {
    $NewPath = ($PathEntries + $InstallDir) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
    Write-Host "[info]  Added $InstallDir to user PATH (restart terminal to take effect)" -ForegroundColor Blue
}

# Cleanup
Remove-Item -Recurse -Force $TmpDir

# Verify
$InstalledBin = Join-Path $InstallDir $BinaryName
$VersionOutput = & $InstalledBin --version 2>&1
Write-Host "[info]  Installed: $VersionOutput" -ForegroundColor Blue
Write-Host "[info]  Run 'codex-switch --help' to get started" -ForegroundColor Blue
