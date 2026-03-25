# codex-switch installer for Windows
# Usage: irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex

$ErrorActionPreference = "Stop"
$Repo = "xjoker/codex-switch"
$BinaryName = "codex-switch.exe"

# Detect architecture
$Arch = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") { "arm64" } else { "amd64" }
$AssetName = "cs-windows-${Arch}.zip"

# Determine version
$Version = if ($env:CS_VERSION) { $env:CS_VERSION } else { "latest" }
if ($Version -eq "latest") {
    $DownloadUrl = "https://github.com/$Repo/releases/latest/download/$AssetName"
} else {
    $DownloadUrl = "https://github.com/$Repo/releases/download/v$Version/$AssetName"
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

# Install to user-local bin directory
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\codex-switch"
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
