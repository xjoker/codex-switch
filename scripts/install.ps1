# codex-switch installer / uninstaller for Windows
# Usage:
#   irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
#   $env:CS_DEV="1"; irm .../install.ps1 | iex              # install latest dev build
#   $env:CS_VERSION="0.0.11"; irm .../install.ps1 | iex      # install specific version
#   $env:CS_UNINSTALL="1"; irm .../install.ps1 | iex         # uninstall codex-switch

$ErrorActionPreference = "Stop"
$Repo = "xjoker/codex-switch"
$BinaryName = "codex-switch.exe"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\codex-switch"
$DataDir = Join-Path $env:USERPROFILE ".codex-switch"

# ── Uninstall ────────────────────────────────────────────
if ($env:CS_UNINSTALL -eq "1") {
    Write-Host "[info]  Uninstalling codex-switch..." -ForegroundColor Blue

    # Remove binary
    $BinPath = Join-Path $InstallDir $BinaryName
    if (Test-Path $BinPath) {
        Remove-Item -Force $BinPath
        Write-Host "[info]  Removed $BinPath" -ForegroundColor Blue
    }

    # Remove install directory if empty
    if ((Test-Path $InstallDir) -and @(Get-ChildItem $InstallDir).Count -eq 0) {
        Remove-Item -Force $InstallDir
    }

    # Remove from PATH
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -like "*$InstallDir*") {
        $NewPath = ($UserPath -split ";" | Where-Object { $_ -ne $InstallDir }) -join ";"
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
    exit 0
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

try {
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $ZipPath -UseBasicParsing
} catch {
    Write-Host "[error] Download failed: $_" -ForegroundColor Red
    exit 1
}

# Extract
Expand-Archive -Path $ZipPath -DestinationPath $TmpDir -Force

# Install
New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
Move-Item -Path (Join-Path $TmpDir $BinaryName) -Destination (Join-Path $InstallDir $BinaryName) -Force

# Add to PATH if not already present
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($UserPath -notlike "*$InstallDir*") {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$InstallDir", "User")
    Write-Host "[info]  Added $InstallDir to user PATH (restart terminal to take effect)" -ForegroundColor Blue
}

# Cleanup
Remove-Item -Recurse -Force $TmpDir

# Verify
$InstalledBin = Join-Path $InstallDir $BinaryName
$VersionOutput = & $InstalledBin --version 2>&1
Write-Host "[info]  Installed: $VersionOutput" -ForegroundColor Blue
Write-Host "[info]  Run 'codex-switch --help' to get started" -ForegroundColor Blue
