# codex-switch

Multi-account profile manager for [OpenAI Codex CLI](https://github.com/openai/codex) with usage dashboard and interactive TUI.

[**中文文档 →**](README_CN.md)

---

![TUI Screenshot](docs/tui.png)

## Features

- **Profile Management** — Save, switch, rename, delete Codex accounts
- **Auto-Detection** — Automatically discovers and tracks the current `auth.json`
- **Usage Dashboard** — Live quota monitoring (5h and 7d windows) with status indicators and per-account refresh timestamps
- **Smart Auto-Switch** — `codex-switch use` without arguments selects the account with the most remaining quota
- **Stale-Only Refresh** — `use`, `list`, and TUI refresh only accounts whose cached usage has expired
- **Progress Display** — Long-running `use`, `list`, and directory `import` operations show a single-line cross-platform progress indicator
- **Interactive TUI** — Full terminal UI with live usage data, color-coded status, and keyboard shortcuts
- **OAuth Login** — Built-in PKCE browser login flow, no manual token copying
- **Token Auto-Refresh** — Automatically refreshes expired tokens using refresh_token
- **Validated Bulk Import** — Import a single `auth.json` or recursively scan a directory, validate files, and auto-assign unique aliases
- **Manual Self-Update** — `self-update --check` checks GitHub Releases on demand; `self-update` installs the latest release for direct installs
- **Proxy Support** — HTTP/HTTPS/SOCKS4/SOCKS5/SOCKS5H with authentication
- **Cross-Platform** — macOS, Linux, Windows
- **JSON Output** — `--json` flag for scripting and automation

## Installation

### One-Liner (Recommended)

**macOS / Linux:**

```bash
curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash
```

**Windows (PowerShell):**

```powershell
irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
```

### Homebrew (macOS / Linux)

```bash
brew install xjoker/tap/codex-switch
```

### Install a Specific Version

```bash
# macOS / Linux
CS_VERSION=0.0.3 curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash

# Windows
$env:CS_VERSION="0.0.3"; irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
```

### From GitHub Releases (Manual)

Download pre-built binaries from [Releases](https://github.com/xjoker/codex-switch/releases):

| Platform | Architecture | File |
|----------|-------------|------|
| macOS | Apple Silicon (M1/M2/M3) | `cs-darwin-arm64.tar.gz` |
| macOS | Intel | `cs-darwin-amd64.tar.gz` |
| Linux | x86_64 | `cs-linux-amd64.tar.gz` |
| Linux | ARM64 | `cs-linux-arm64.tar.gz` |
| Windows | x86_64 | `cs-windows-amd64.zip` |
| Windows | ARM64 | `cs-windows-arm64.zip` |

### From Source

Requires [Rust](https://rustup.rs/) 1.88+:

```bash
git clone https://github.com/xjoker/codex-switch.git
cd codex-switch
cargo build --release
# Binary: target/release/codex-switch (or target\release\codex-switch.exe on Windows)
sudo cp target/release/codex-switch /usr/local/bin/  # macOS/Linux
```

## Quick Start

```bash
# 1. Log in to your first Codex account
codex-switch login

# 2. Log in to another account
codex-switch login

# 3. See all accounts with live usage
codex-switch list

# 4. Switch to a specific account
codex-switch use alice

# 5. Auto-switch to the best available account
codex-switch use

# 6. Launch interactive TUI
codex-switch tui

# 7. Check for a new release manually
codex-switch self-update --check
```

## Commands

| Command | Description |
|---------|-------------|
| `codex-switch use [alias]` | Switch to a profile. Omit alias to auto-select the best account by quota |
| `codex-switch list [-f]` | List all profiles with account info, usage, and availability (`-f` force refresh) |
| `codex-switch login [--device] [alias]` | Log in via OAuth (`--device` for headless servers). If alias exists, re-authorizes |
| `codex-switch delete <alias>` | Delete a profile |
| `codex-switch import <path> [alias]` | Import one auth.json file, or recursively validate and import all JSON files under a directory |
| `codex-switch self-update [--check] [--version <ver>]` | Manually check GitHub Releases or update the current direct-install binary |
| `codex-switch tui` | Launch the interactive terminal UI |
| `codex-switch open` | Open the config directory in file manager |

### Global Options

| Option | Description |
|--------|-------------|
| `--json` | Output as compact JSON (for scripting/pipes) |
| `--json-pretty` | Output as pretty-printed JSON |
| `--proxy <URL>` | Set proxy (see [Proxy](#proxy-support) section) |
| `--color <auto\|always\|never>` | Color output mode (default: auto) |
| `--debug` | Enable debug logging (shows HTTP requests, API responses, cache status) |
| `-V, --version` | Print version |

## TUI Keyboard Shortcuts

| Key | Action |
|-----|--------|
| `j` / `k` or `Up` / `Down` | Navigate accounts |
| `Enter` | Switch to selected account |
| `r` | Refresh all usage data |
| `n` | Rename selected profile |
| `d` | Delete selected profile (with confirmation) |
| `q` / `Esc` | Quit |

## Updating

Update checks are manual only. `codex-switch` does not check for updates automatically during startup, `list`, `use`, or TUI launch.

```bash
# Check whether a newer release exists
codex-switch self-update --check

# Update a direct install to the latest release
codex-switch self-update

# Update to a specific newer version
codex-switch self-update --version 0.0.3
```

- Homebrew installs are not self-overwritten. Use `brew upgrade xjoker/tap/codex-switch`.
- Direct installs verify the release `.sha256` before replacing the current executable.
- Downgrades are not supported. `--version` only accepts the current version or a newer release.

## Proxy Support

Proxy resolution priority (highest to lowest):

1. `--proxy` CLI flag
2. `CS_PROXY` environment variable
3. Config file `~/.codex-switch/config.toml`
4. Standard environment variables (`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`)

### Supported Protocols

| Protocol | DNS Resolution | Auth |
|----------|---------------|------|
| `http://[user:pass@]host:port` | Local | Yes |
| `https://[user:pass@]host:port` | Local | Yes |
| `socks4://host:port` | Local | No |
| `socks5://[user:pass@]host:port` | Local | Yes |
| `socks5h://[user:pass@]host:port` | Remote (via proxy) | Yes |

### Config File

`~/.codex-switch/config.toml`:

```toml
[proxy]
url = "socks5h://user:pass@127.0.0.1:1080"
no_proxy = "localhost,127.0.0.1"

[cache]
ttl = 300  # Cache TTL in seconds (default: 300)

[network]
max_concurrent = 20  # Max concurrent usage requests (default: 20)
```

### Examples

```bash
# CLI flag
codex-switch --proxy socks5h://127.0.0.1:1080 list

# Environment variable
export CS_PROXY="http://user:pass@proxy.corp.com:8080"
codex-switch list

# Standard env var (reqwest reads this automatically)
export HTTPS_PROXY="http://proxy.corp.com:8080"
codex-switch list
```

## How It Works

### File Locations

| Path | Description |
|------|-------------|
| `~/.codex/auth.json` | Live Codex CLI auth file (or `$CODEX_HOME/auth.json`) |
| `~/.codex-switch/profiles/<alias>/auth.json` | Saved profile data |
| `~/.codex-switch/current` | Currently active profile name |
| `~/.codex-switch/config.toml` | Configuration file |

### Auto-Detection

When you run `codex-switch list` or `codex-switch tui`, the tool checks if the live `~/.codex/auth.json` belongs to an untracked account. If so, it automatically saves it as a new profile (using the email username as alias).

### Deduplication

On login or import, the tool matches accounts by `account_id` (primary) or `email` (fallback). If the same account already exists under a different alias, it updates the existing profile instead of creating a duplicate.

### Import Validation

`codex-switch import` validates every candidate file in stages:

1. File format — valid JSON
2. Structure — required `tokens` fields and a decodable `id_token`
3. Usage validation — token refresh and usage API check, unless explicitly skipped in tests
4. Save — deduplicate by identity and assign a unique alias if needed

If the input path is a directory, the command scans recursively for `.json` files and reports imported vs skipped files.

### Smart Auto-Switch (`codex-switch use`)

When called without an alias, `codex-switch use` first consumes fresh cached entries, then only refreshes stale accounts and scores them:

1. Weekly (7d) limit at 100% → heavily penalized (account unusable)
2. 5h window has remaining quota → high score (1000 + remaining %)
3. 5h window exhausted, weekly OK → medium score (based on reset time)

The highest-scoring account is selected and switched to.

### Cache Behavior

- Usage cache is stored per profile alias in `~/.codex-switch/cache.json`
- Each cached entry keeps its own refresh timestamp; JSON output exposes it as `usage.fetched_at`
- `list`, `use`, and the TUI only refresh stale accounts by default
- `list -f` and TUI `r` bypass cache and force a refresh for all accounts
- Directory import always validates every file and shows progress as it advances

### Token Auto-Refresh

When a usage query returns HTTP 401/403, the tool automatically attempts to refresh the token using the stored `refresh_token`. If successful, the new tokens are persisted back to the profile and the live auth.json.

### Safety Notes

- CLI and TUI both refuse to delete the active profile
- JSON mode keeps stdout machine-readable; human progress/messages go to stderr instead

## Platform Notes

### macOS

- Default Codex auth path: `~/.codex/auth.json`
- Browser opens via system `open` command
- File manager opens via `open`

### Linux

- Default Codex auth path: `~/.codex/auth.json`
- Browser opens via `xdg-open` (ensure a desktop browser is configured)
- File manager opens via `xdg-open`
- WSL: browser opening may require `wslu` package (`sudo apt install wslu`)

### Windows

- Default Codex auth path: `%USERPROFILE%\.codex\auth.json`
- Browser opens via `rundll32.exe url.dll,FileProtocolHandler` (fallback: `webbrowser` crate)
- File manager opens via `explorer.exe`
- Terminal: works with Windows Terminal, PowerShell, and cmd.exe
- TUI rendering uses Windows Console API via `crossterm`

## JSON Output

All commands support `--json` for machine-readable output:

```bash
# List profiles as JSON
codex-switch --json list

# Switch and get result
codex-switch --json use alice

# Check updates in JSON mode
codex-switch --json self-update --check
```

## Building

```bash
# Debug build
cargo build

# Release build (optimized, stripped)
cargo build --release

# Cross-compile for Linux (from macOS)
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl

# Cross-compile for Windows (from macOS/Linux)
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

## License

[MIT](LICENSE)
