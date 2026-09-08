# Updating

Direct installations update themselves from verified GitHub Release archives. Homebrew installations are updated by Homebrew only.

Update checks are manual except for the one check performed when the TUI starts:

```bash
codex-switch self-update --check    # check without installing
codex-switch self-update            # update within the current channel
codex-switch self-update --version <VERSION>   # install a specific newer stable version
```

Downloaded archives are checked against the `.sha256` file in the same GitHub Release and the release's `codex-switch-build-provenance.json` Sigstore bundle before the binary is replaced. The updater resolves the release tag through the GitHub Git API, then runs `gh attestation verify` with the repository, release workflow, exact tag ref, and that tag's full commit digest pinned; attestations produced by self-hosted runners are rejected. It resolves the tag again after verification and aborts if it moved during the update. SHA-256 detects corruption; the artifact attestation proves the archive came from the exact source revision of this repository's GitHub Actions release workflow. Direct self-update therefore requires a current [GitHub CLI](https://cli.github.com/) with attestation support and fails closed when `gh`, the provenance bundle, or the exact tag commit cannot be verified. The current binary does not run or restart a background service during update.

The standalone installers have a separate contract: they always check the archive checksum and optionally verify repository/workflow provenance when a capable GitHub CLI is available (`CS_REQUIRE_PROVENANCE=1` makes that optional check mandatory). They do not replace the self-updater's exact-tag and full-digest recheck.

## Channels and version scheme

Releases use calendar versions in the form `YYYYMMDD.N.0`. There are two channels:

- **stable** — tagged releases; the default for all installers.
- **dev** — a rolling prerelease published under the `dev` tag; versions end in `-dev`.

```bash
codex-switch self-update --dev      # move a direct install to the dev channel
codex-switch self-update --stable   # move a dev build back to the stable channel
```

Without a channel flag, `self-update` stays on the channel of the current binary. See [Testing development releases](Development-Releases) for the full dev-channel workflow including verification and rollback.

## Breaking changes since the 0.0.x series

The current release line intentionally breaks with several `0.0.x` conventions. Upgrading is still a normal `self-update` (or one installer rerun); configuration and profiles remain in place. What changed and why:

- **Version scheme: `0.0.x` → calendar `YYYYMMDD.N.0`.** The old SemVer counter said nothing about release age on a fast rolling cadence. Calendar versions encode the version-allocation date directly; `N` starts at 1 each day and increments for additional candidates allocated that day. A final dev build can be promoted unchanged on a later day after acceptance, so the stable tag can appear after the date encoded in its version. The values stay SemVer-compatible and sort higher than any `0.0.x`, which is exactly what keeps old installs directly upgradable.
- **Unix install location: `/usr/local/bin` → `$HOME/.local/bin`.** The old system-wide location required `sudo` for every update, which breaks unattended `self-update` and left root-owned binaries behind. The default is now a user-owned install with shell PATH configured by the installer; `self-update` never needs elevation. Administrators who really want a system-wide install must opt in with `--system`, which records a marker so later updates know the choice was intentional. Rerunning the installer migrates a legacy `/usr/local/bin` copy once (see below). Windows keeps `%LOCALAPPDATA%\Programs\codex-switch`; Homebrew keeps ownership of its own binary.
- **Codex file credential store is now required.** Explicit `keyring`, `auto`, and `ephemeral` stores are rejected instead of tolerated, because reliable switching depends on the live `auth.json` file. Set `cli_auth_credentials_store = "file"` in `$CODEX_HOME/config.toml`.
- **Invalid configuration fails fast.** An existing but unreadable, malformed, or dangling-symlink `config.toml` now stops with the real error instead of silently running on defaults. Only a genuinely missing file uses defaults.
- **Stricter non-interactive behavior.** `delete` defaults to `y/N` and requires `--yes` in JSON or non-TTY runs; `--json use` fails with an actionable error instead of hiding a prompt when the live auth file is untracked; reset cards are never consumed without an explicit flag. Automation that relied on silent prompts needs the explicit flags.

## Migrate from the removed daemon

The daemon commands were removed. Before replacing an older binary, use that older binary to stop and uninstall its service:

```bash
codex-switch daemon stop
codex-switch daemon status
codex-switch daemon uninstall
```

Run these commands before installing the new release; the new binary intentionally has no daemon compatibility command. Confirm in the operating system's scheduler that the old LaunchAgent, systemd user unit, or Task Scheduler task is gone. An upgrade does not automatically stop or remove machine tasks and services. If you already upgraded, use the old executable or restore it temporarily to perform the stop/uninstall, then remove any remaining task yourself. Do not leave an old scheduled task invoking the old daemon after the upgrade.

## Homebrew installations

Homebrew distributes stable releases only and must retain ownership of its binary:

```bash
brew upgrade xjoker/tap/codex-switch
```

To test the rolling dev build, remove the Homebrew package first, then use the direct dev installer:

```bash
brew uninstall codex-switch
curl -fsSL https://github.com/xjoker/codex-switch/releases/download/dev/install.sh | bash -s -- --dev
```

`self-update --stable` keeps a direct installation but returns it to the stable channel. To return to Homebrew ownership, run the direct uninstaller, keep the data directory when prompted, then reinstall with `brew install xjoker/tap/codex-switch`.

## Legacy direct installs

- Stable versions `0.0.3` and newer update directly with `self-update`; the release workflow continuously verifies the upgrade path from `v0.0.19` on macOS, Linux, and Windows.
- Versions `0.0.1` and `0.0.2` should rerun the installer because their updater predates the supported migration path.
- Older macOS/Linux direct installs in `/usr/local/bin` should rerun the current installer once:

```bash
curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash
```

The script verifies the download, validates one-time `sudo` access, installs the user-owned binary under `$HOME/.local/bin`, and removes the legacy copy so PATH cannot keep selecting it. Profiles and configuration under `~/.codex-switch` are preserved. If an old updater first upgrades in place with `sudo`, the new updater detects the unmarked legacy location and prints this installer command before any later network check. Explicit `--system` installs carry a marker and continue to use system-level updates.

Before downloading anything, `self-update` verifies that it can create a replacement in the current executable's directory, so a permission problem surfaces before the network is touched.

## Uninstall

**macOS / Linux:**

```bash
curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash -s -- --uninstall
```

**Windows PowerShell:**

```powershell
$previous = @{
  CS_DEV = $env:CS_DEV
  CS_VERSION = $env:CS_VERSION
  CS_UNINSTALL = $env:CS_UNINSTALL
}
try {
  $env:CS_UNINSTALL = "1"
  Remove-Item Env:CS_DEV, Env:CS_VERSION -ErrorAction SilentlyContinue
  irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
}
finally {
  foreach ($name in $previous.Keys) {
    if ($null -eq $previous[$name]) {
      Remove-Item "Env:$name" -ErrorAction SilentlyContinue
    } else {
      Set-Item "Env:$name" $previous[$name]
    }
  }
}
```

The uninstaller asks whether to remove the data directory; answer `N` to keep profiles and configuration.

## Next steps

- Opt into prerelease testing with [Testing development releases](Development-Releases).
- Update failures and permission errors are covered in [Troubleshooting](Troubleshooting).
- Return to the Wiki [Home](Home).
