# codex-switch

**A multi-account manager for [OpenAI Codex CLI](https://github.com/openai/codex).** Save local Codex logins, monitor quota, and select the best account before the next session.

[中文说明](README_CN.md) · [**Documentation (Wiki)**](https://github.com/xjoker/codex-switch/wiki) · [Releases](https://github.com/xjoker/codex-switch/releases)

> `codex-switch` manages local authentication files. Never publish profiles, `auth.json`, tokens, proxy credentials, or unredacted debug output.

## Quick start

Codex must use its file credential store. If needed, add this to `$CODEX_HOME/config.toml` (normally `~/.codex/config.toml`); a managed configuration with `forced_login_method = "api"` is incompatible:

```toml
cli_auth_credentials_store = "file"
```

Install the stable release — macOS / Linux:

```bash
curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash
```

Windows PowerShell:

```powershell
irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
```

Homebrew users: `brew install xjoker/tap/codex-switch`.

> **Note:** this project is not distributed on crates.io — do not `cargo install codex-switch`; that package name belongs to an unrelated project.

Then add an account and open the dashboard:

```bash
codex-switch login        # use --device on a headless machine
codex-switch tui          # interactive dashboard
codex-switch use          # switch to the best eligible account
codex-switch launch       # start Codex with the best account
```

![TUI](docs/tui.png)

## What it does

- Saves, imports, renames, switches, and recoverably deletes Codex profiles.
- Saves custom API providers (OpenRouter and other Responses-compatible endpoints) with multiple models per endpoint, and launches Codex with them without writing to `~/.codex`:

  ```bash
  codex-switch provider add openrouter \
    --base-url https://openrouter.ai/api/v1 \
    --model openai/gpt-5.3-codex \
    --model deepseek/deepseek-r1-0528 --reasoning medium
  codex-switch launch openrouter
  codex-switch launch openrouter --model deepseek/deepseek-r1-0528
  ```
- Displays the main and model-specific quota pools in CLI and TUI views.
- Selects an eligible account with adaptive, pace-aware scoring, and launches Codex with it.
- Supports reset cards, one-time quota warmup (`warmup`), forced usage refresh (`list -f`), JSON output, and proxies. On the TUI Accounts page, `u` (when no accounts are marked) switches the selected account and shows progress, `t` enables session-only auto-refresh, and the account menu's `w` performs a one-time warmup; it does not run a resident service.
- Refreshes expiring tokens and updates direct installs: `self-update`, `self-update --stable`, `self-update --version <VERSION>`, or the rolling dev channel via `self-update --dev` — new dev installs use [install.sh](https://github.com/xjoker/codex-switch/releases/download/dev/install.sh) / [install.ps1](https://github.com/xjoker/codex-switch/releases/download/dev/install.ps1) from the `dev` release.
- Direct `self-update` verifies both SHA-256 and GitHub build provenance with `gh attestation verify`; install a current [GitHub CLI](https://cli.github.com/) before using it.
- Runs on macOS, Linux, and Windows.

> **Upgrading from a `0.0.x` install?** This release line intentionally breaks two conventions: versions are now calendar-based (`YYYYMMDD.N.0`, so updates sort and read by date), and Unix installs moved from `/usr/local/bin` to the user-owned `$HOME/.local/bin` so `self-update` never needs `sudo`. A normal `self-update` or one installer rerun migrates you; profiles and configuration are preserved. All breaking changes and reasons: [Updating](https://github.com/xjoker/codex-switch/wiki/Updating).

> **Upgrading from a release with the old daemon?** Before replacing that binary, run its `daemon stop`, `daemon status`, and `daemon uninstall` commands, then remove any old LaunchAgent, systemd unit, or Windows Task Scheduler task. Remove obsolete `[daemon]` settings such as `cache_refresh_interval_secs` and `auto_warmup`. The new release has no daemon compatibility command and does not clean OS tasks automatically; see [Updating](https://github.com/xjoker/codex-switch/wiki/Updating#migrate-from-the-removed-daemon).

## Documentation

The **[GitHub Wiki](https://github.com/xjoker/codex-switch/wiki)** is the complete documentation — getting started, feature guide, custom API providers, command reference, configuration, updating and channels, troubleshooting, FAQ, and the contributor guides (architecture, onboarding). Its sources live in [`docs/wiki/`](docs/wiki) and are reviewed with the code.

Maintainer documents: [release process](docs/RELEASE.md) · [changelog](docs/CHANGELOG.md) · [contributing](CONTRIBUTING.md).

## Development

Requires Rust 1.88 or newer:

```bash
cargo build
cargo test --all
cargo clippy --all-targets --all-features -- -D warnings
```

See the [developer onboarding](https://github.com/xjoker/codex-switch/wiki/Developer-Onboarding) and [architecture](https://github.com/xjoker/codex-switch/wiki/Architecture-Overview) Wiki pages before changing authentication, storage, selection, or update behavior.

## License

[MIT](LICENSE)
