# Pre-publish upgrade gate (Windows): a published legacy release self-updates
# to this workflow's unpublished build artifacts, which a local GitHub API mock
# serves. See legacy-upgrade.sh for the scenarios.
#
# usage: legacy-upgrade.ps1 -Legacy <0.0.19|20260804.1.0>
# env: EXPECTED_VERSION, IS_DEV, GITHUB_SHA, RUNNER_TEMP; ARTIFACTS_DIR
# (default: artifacts). `gh attestation verify` checks the bundle offline.
param([Parameter(Mandatory = $true)][string]$Legacy)

$ErrorActionPreference = 'Stop'
foreach ($name in 'EXPECTED_VERSION', 'IS_DEV', 'GITHUB_SHA', 'RUNNER_TEMP') {
  if (-not [Environment]::GetEnvironmentVariable($name)) { throw "$name is required" }
}
$artifactsDir = if ($env:ARTIFACTS_DIR) { $env:ARTIFACTS_DIR } else { 'artifacts' }
$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$port = 8765
$taskName = '\codex-switch-daemon'
$schtasks = Join-Path $env:SystemRoot 'System32\schtasks.exe'
$asset = 'cs-windows-amd64.zip'

# Reviewed fixtures: the published archives must never change underneath us.
$pinnedHashes = @{
  '0.0.19'       = '9ff8a3f8794517771ef6959632417fdf1dfbc85b7fdf4c344998223985582e63'
  '20260804.1.0' = '2702116e852f8bf3563a0dc892ab6bab2792679c090054d184d65f43b9224e4b'
}
if (-not $pinnedHashes.ContainsKey($Legacy)) { throw "No reviewed fixture for v$Legacy $asset" }

$work = Join-Path $env:RUNNER_TEMP ("legacy-upgrade-$Legacy-" + [guid]::NewGuid())
$apiRoot = Join-Path $work 'api'
$artifactRoot = Join-Path $apiRoot 'artifacts'
New-Item -ItemType Directory -Path $artifactRoot -Force | Out-Null
Copy-Item -Path (Join-Path $artifactsDir '*') -Destination $artifactRoot -Force

if ($env:IS_DEV -eq 'true') {
  $apiPath = 'repos/xjoker/codex-switch/releases/tags/dev'
  $releaseTag = 'dev'
  $releaseName = "dev ($env:EXPECTED_VERSION)"
  $channel = '--dev'
} else {
  $apiPath = 'repos/xjoker/codex-switch/releases/latest'
  $releaseTag = "v$env:EXPECTED_VERSION"
  $releaseName = $releaseTag
  $channel = '--stable'
}
& python (Join-Path $repoRoot 'scripts/prepare-release-metadata.py') `
  --artifacts-dir $artifactRoot `
  --output (Join-Path $apiRoot $apiPath) `
  --base-url "http://127.0.0.1:$port/artifacts" `
  --tag $releaseTag `
  --name $releaseName `
  --extra-asset codex-switch-build-provenance.json `
  --tag-ref-output (Join-Path $apiRoot "repos/xjoker/codex-switch/git/ref/tags/$releaseTag") `
  --commit-sha $env:GITHUB_SHA
if ($LASTEXITCODE -ne 0) { throw 'local release metadata generation failed' }

$server = Start-Process -FilePath python -ArgumentList @(
  '-m', 'http.server', "$port", '--bind', '127.0.0.1', '--directory', $apiRoot
) -WindowStyle Hidden -PassThru

function Wait-Until([string]$Description, [scriptblock]$Condition) {
  foreach ($attempt in 1..60) {
    if (& $Condition) { return }
    Start-Sleep -Milliseconds 500
  }
  throw "Timed out waiting for: $Description"
}

function Test-TaskRegistered {
  & $schtasks /Query /TN $taskName *> $null
  return $LASTEXITCODE -eq 0
}

try {
  Wait-Until 'local release metadata server' {
    try {
      Invoke-WebRequest -Uri "http://127.0.0.1:$port/$apiPath" -UseBasicParsing | Out-Null
      $true
    } catch { $false }
  }

  $base = "https://github.com/xjoker/codex-switch/releases/download/v$Legacy"
  $archive = Join-Path $work $asset
  Invoke-WebRequest -Uri "$base/$asset" -OutFile $archive
  Invoke-WebRequest -Uri "$base/$asset.sha256" -OutFile "$archive.sha256"
  $expectedHash = ((Get-Content -LiteralPath "$archive.sha256" -Raw).Trim() -split '\s+')[0]
  $actualHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
  if ($actualHash -ne $expectedHash) { throw "v$Legacy checksum mismatch" }
  if ($actualHash -ne $pinnedHashes[$Legacy]) { throw "v$Legacy archive changed from the reviewed fixture" }
  Expand-Archive -Path $archive -DestinationPath $work -Force
  $bin = Join-Path $work 'codex-switch.exe'
  if (@(& $bin --version)[0] -ne "codex-switch $Legacy") { throw 'unexpected legacy version' }

  # Windows resolves the profile folder without HOME, so isolate both homes.
  $env:CODEX_SWITCH_HOME = Join-Path $work 'app'
  $env:CODEX_HOME = Join-Path $work 'codex'
  $env:CS_GITHUB_API_URL = "http://127.0.0.1:$port"
  $pidFile = Join-Path $env:CODEX_SWITCH_HOME 'daemon.pid'

  function Assert-Upgraded {
    if (@(& $bin --version)[0] -ne "codex-switch $env:EXPECTED_VERSION") {
      throw 'legacy upgrade installed an unexpected version'
    }
  }

  if ($Legacy -eq '0.0.19') {
    & $bin self-update $channel
    if ($LASTEXITCODE -ne 0) { throw 'v0.0.19 self-update failed' }
    Assert-Upgraded
    return
  }

  # On Windows, v20260804.1.0 runs its daemon from the scheduled task as
  # `daemon start --foreground`, and that daemon holds its pidfile locked, so
  # the old CLI cannot see it: detached `daemon start` times out and the old
  # updater neither stops nor restarts it. Reproduce that state: the binary
  # must be replaced while the old daemon process is still running.
  $daemon = Start-Process -FilePath $bin -ArgumentList 'daemon', 'start', '--foreground' `
    -WindowStyle Hidden -PassThru
  try {
    Wait-Until 'old foreground daemon to write its pidfile' { Test-Path -LiteralPath $pidFile }
    if ($daemon.HasExited) { throw "old daemon exited early with $($daemon.ExitCode)" }
    & $bin self-update $channel
    if ($LASTEXITCODE -ne 0) { throw "v$Legacy self-update failed beside a running daemon" }
    Assert-Upgraded
  } finally {
    # In real use the old daemon keeps running until logoff.
    Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
  }

  # At the next logon the old task launches the new binary, which must
  # remove the task instead of failing on every logon.
  & $schtasks /Create /TN $taskName /TR "`"$bin`" daemon start --foreground" /SC ONCE /ST 23:59 /F | Out-Null
  if ($LASTEXITCODE -ne 0) { throw 'creating the legacy scheduled task fixture failed' }
  & $bin daemon start --foreground
  if ($LASTEXITCODE -ne 0) { throw 'daemon shim failed' }
  if (Test-TaskRegistered) { throw 'legacy scheduled task was not removed' }
} finally {
  Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
  if (Test-TaskRegistered) { & $schtasks /Delete /TN $taskName /F | Out-Null }
  # The runner ends pwsh steps with `exit $LASTEXITCODE`; the expected
  # "not registered" query above must not fail a passing gate. Exceptions
  # still propagate and fail the step.
  $global:LASTEXITCODE = 0
}
