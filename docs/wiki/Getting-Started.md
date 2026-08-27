# Getting started

This page takes you from nothing to a working multi-account setup: install codex-switch, add accounts, and pick the best one before a Codex session.

## Requirements

- [OpenAI Codex CLI](https://github.com/openai/codex) installed, plus at least one ChatGPT account that can log in to Codex.
- Codex must use its **file credential store**, because codex-switch works by atomically replacing `$CODEX_HOME/auth.json`. If needed, add this to `$CODEX_HOME/config.toml` (normally `~/.codex/config.toml`):

```toml
cli_auth_credentials_store = "file"
```

Explicit `keyring`, `auto`, and `ephemeral` stores are rejected — permanently by design, because OS keyrings cannot provide the locking and atomic-replace guarantees switching depends on (see [why only the file store is supported](Configuration#why-only-the-file-store-is-supported)). A managed Codex configuration with `forced_login_method = "api"` is also incompatible, because codex-switch manages ChatGPT login profiles. In both cases codex-switch stops with an actionable error instead of modifying authentication state; after switching to the file store, log in again.

## Install

**macOS / Linux:**

```bash
curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash
```

This installs to the user-owned `$HOME/.local/bin` and configures PATH for zsh, bash, and fish; other shells receive a manual PATH instruction. An older direct install under `/usr/local/bin` is migrated once: the new user binary is installed first, then the installer removes the old copy with one elevated operation when required. Administrators can explicitly keep a system-wide install with `--system`; system installs may require `sudo` for later updates.

The installer verifies the download's SHA-256 checksum and, when a [GitHub CLI](https://cli.github.com/) with attestation support is present, its Sigstore build provenance — the same attestation `self-update` enforces, proving the archive was built by this repository's release workflow on a GitHub-hosted runner rather than merely matching a checksum published beside it. Provenance uses offline bundle verification, so it needs no `gh auth login`. If the GitHub CLI is unavailable the checksum is still enforced and provenance is skipped with a warning; set `CS_REQUIRE_PROVENANCE=1` to make missing verification a hard failure.

> If the installer says `Installing to /usr/local/bin (requires sudo)` without an explicit `--system`, stop it: that is the retired script from the repository's old `master` branch. Use the Release URL above.

**Windows PowerShell:**

```powershell
irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
```

Windows installs under `%LOCALAPPDATA%\Programs\codex-switch` and updates the user PATH.

**Homebrew (macOS / Linux):**

```bash
brew install xjoker/tap/codex-switch
```

Homebrew distributes stable releases only and keeps ownership of its binary; update it with `brew upgrade xjoker/tap/codex-switch`, not with `self-update`.

> **Note:** this project is not distributed on crates.io. Do not `cargo install codex-switch` — that package name belongs to an unrelated project of the same name.

Verify the installation:

```bash
codex-switch --version
```

## Add your first account

```bash
codex-switch login work
```

`login` opens a browser PKCE flow; the alias (`work`) is optional and can be renamed later. On a headless machine, use the device-code flow instead:

```bash
codex-switch login --device server
```

If you already have `auth.json` backups, import a file or scan a whole directory. Imports are parsed, identity-checked, validated against the usage service, and saved under collision-free aliases. An import never overwrites an existing profile: a Team workspace ID proves access to that workspace, not ownership of another user's saved credentials.

```bash
codex-switch import ~/auth-backups
```

codex-switch also notices logins performed outside of it: when the live `auth.json` contains an account it does not track (for example after a plain `codex login`), an interactive run offers to save it as a profile.

## Inspect quota and pick an account

```bash
codex-switch list        # accounts, quota, availability
codex-switch tui         # interactive dashboard
codex-switch use         # switch to the best eligible account
codex-switch launch      # select, start Codex, restore auth afterwards
```

`use` without an alias ranks all accounts with the adaptive scoring algorithm; `use <alias>` switches explicitly. Codex reads authentication at startup, so restart Codex after a manual switch — or use `launch`, which handles staging and restoration for you.

## Where your data lives

Saved profiles, cache, configuration, and daemon state default to `~/.codex-switch` (`%USERPROFILE%\.codex-switch` on Windows). The live Codex file stays at `$CODEX_HOME/auth.json`. See [Configuration](Configuration) for every path and setting.

Never share profile files, `auth.json`, tokens, provider API keys, proxy credentials, or unredacted `--debug` output.

## Add a custom API provider (optional)

If you use OpenRouter or another Responses-compatible gateway instead of ChatGPT OAuth:

```bash
codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-5.3-codex
codex-switch launch openrouter
```

`codex-switch tui` also has a **Providers** tab for add/edit/rename/remove and for launching Codex with a saved model. `use` and auto-select stay ChatGPT-only.

## Next steps

- Learn account, quota, launch, and daemon workflows in the [Feature guide](Feature-Guide).
- Launch Codex against OpenRouter or another custom API: [Custom API providers](Providers).
- Look up exact commands and TUI shortcuts in the [Command reference](Command-Reference).
- Keep the binary current with [Updating](Updating).
