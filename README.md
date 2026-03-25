# codex-switch

Multi-account profile manager for [OpenAI Codex CLI](https://github.com/openai/codex) with usage dashboard and interactive TUI.

[**中文文档 →**](README_CN.md)

---

![TUI Screenshot](docs/tui.png)

## Features

- **Profile Management** — Save, switch, rename, delete Codex accounts
- **Auto-Detection** — Automatically discovers and tracks the current `auth.json`
- **Usage Dashboard** — Real-time quota monitoring (5h and 7d windows) with status indicators
- **Smart Auto-Switch** — `codex-switch use` without arguments selects the account with the most remaining quota
- **Interactive TUI** — Full terminal UI with live usage data, color-coded status, and keyboard shortcuts
- **OAuth Login** — Built-in PKCE browser login flow, no manual token copying
- **Token Auto-Refresh** — Automatically refreshes expired tokens using refresh_token
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
CS_VERSION=0.0.1 curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash

# Windows
$env:CS_VERSION="0.0.1"; irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
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

Requires [Rust](https://rustup.rs/) 1.85+:

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
codex-switch status

# 4. Switch to a specific account
codex-switch use alice

# 5. Auto-switch to the best available account
codex-switch use

# 6. Launch interactive TUI
codex-switch tui
```

## Commands

| Command | Description |
|---------|-------------|
| `codex-switch use [alias]` | Switch to a profile. Omit alias to auto-select the best account by quota |
| `codex-switch list` | List all saved profiles (auto-discovers current auth.json) |
| `codex-switch status` | Show all profiles with account info, usage, and availability |
| `codex-switch login [alias]` | Log in via OAuth. If alias exists, re-authorizes that profile |
| `codex-switch delete <alias>` | Delete a profile |
| `codex-switch import <file> <alias>` | Import an auth.json file as a profile |
| `codex-switch tui` | Launch the interactive terminal UI |
| `codex-switch open` | Open the config directory in file manager |

### Global Options

| Option | Description |
|--------|-------------|
| `--json` | Output as JSON (for scripting/pipes) |
| `--proxy <URL>` | Set proxy (see [Proxy](#proxy-support) section) |
| `--color <auto\|always\|never>` | Color output mode (default: auto) |
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
```

### Examples

```bash
# CLI flag
codex-switch --proxy socks5h://127.0.0.1:1080 status

# Environment variable
export CS_PROXY="http://user:pass@proxy.corp.com:8080"
codex-switch status

# Standard env var (reqwest reads this automatically)
export HTTPS_PROXY="http://proxy.corp.com:8080"
codex-switch status
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

When you run `codex-switch list`, `codex-switch status`, or `codex-switch tui`, the tool checks if the live `~/.codex/auth.json` belongs to an untracked account. If so, it automatically saves it as a new profile (using the email username as alias).

### Deduplication

On login or import, the tool matches accounts by `account_id` (primary) or `email` (fallback). If the same account already exists under a different alias, it updates the existing profile instead of creating a duplicate.

### Smart Auto-Switch (`codex-switch use`)

When called without an alias, `codex-switch use` queries all accounts concurrently and scores them:

1. Weekly (7d) limit at 100% → heavily penalized (account unusable)
2. 5h window has remaining quota → high score (1000 + remaining %)
3. 5h window exhausted, weekly OK → medium score (based on reset time)

The highest-scoring account is selected and switched to.

### Token Auto-Refresh

When a usage query returns HTTP 401/403, the tool automatically attempts to refresh the token using the stored `refresh_token`. If successful, the new tokens are persisted back to the profile and the live auth.json.

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

# Status with usage data
codex-switch --json status

# Switch and get result
codex-switch --json use alice
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
