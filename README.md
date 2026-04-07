# codex-switch

Multi-account profile manager for [OpenAI Codex CLI](https://github.com/openai/codex) with usage dashboard and interactive TUI.

[**中文文档 →**](README_CN.md)

---

![TUI Screenshot](docs/tui.png)

## Features

- **Profile Management** — Save, switch, rename, delete Codex accounts
- **Auto-Detection** — Automatically discovers and tracks the current `auth.json`
- **Usage Dashboard** — Live quota monitoring (5h and 7d windows) with status indicators and per-account refresh timestamps
- **Smart Auto-Switch** — `codex-switch use` without arguments selects the best account using one of three [selection modes](#selection-modes) (Team accounts are prioritized over other plan types)
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
CS_VERSION=0.0.11 curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash

# Windows
$env:CS_VERSION="0.0.11"; irm https://github.com/xjoker/codex-switch/releases/latest/download/install.ps1 | iex
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

# 1b. On headless servers (no browser), use device code flow:
codex-switch login --device

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
| `codex-switch use [alias] [-m mode]` | Switch to a profile. Omit alias to auto-select using the configured [selection mode](#selection-modes). `-m` overrides the default mode |
| `codex-switch list [-f]` | List all profiles with account info, usage, and availability (`-f` force refresh) |
| `codex-switch login [--device] [alias]` | Log in via OAuth (`--device` for headless servers). If alias exists, re-authorizes |
| `codex-switch rename <old> <new>` | Rename a profile |
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
| `/` | Search / filter accounts |
| `r` | Refresh all usage data |
| `s` | Cycle sort mode (name / quota / status) |
| `Space` | Mark / unmark account for batch operations |
| `b` | Batch refresh marked accounts |
| `c` | Clear all marks |
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
codex-switch self-update --version 0.0.11
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

[use]
mode = "max-remaining"      # Selection mode: max-remaining | drain-first | round-robin
min_remaining = 5           # drain-first: demote accounts below this 5h remaining% (default: 5)
safety_margin_7d = 20       # 7d safety margin: penalty kicks in below this remaining% (default: 20)
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

## Common Usage Scenarios

### Auto-switch before each Codex session

```bash
# Add to your shell profile (.zshrc / .bashrc):
codex-switch use && codex
```

### Scheduled token refresh via cron (optional)

Keep cached usage data and tokens fresh in the background so `codex-switch use` is instant:

```bash
# Edit crontab
crontab -e

# Refresh all account usage every 5 minutes
*/5 * * * * /usr/local/bin/codex-switch list --json > /dev/null 2>&1
```

This runs `codex-switch list` periodically, which refreshes stale tokens and caches usage data. It does **not** switch accounts automatically.

### Use in CI / automation

```bash
# Switch to best account and launch Codex in one line
codex-switch use --json && codex --quiet ...
```

## Troubleshooting

If you encounter errors, run with `--debug` to see detailed HTTP requests, API responses, and cache status:

```bash
codex-switch --debug list
codex-switch --debug use
```

If the issue persists, please [open an issue](https://github.com/xjoker/codex-switch/issues) with the debug output attached (remember to redact any sensitive information like tokens or emails).

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

When called without an alias, `codex-switch use` scores every account and switches to the highest-scoring one. It first consumes fresh cached entries and only refreshes stale accounts.

The algorithm uses a **two-phase** approach:
1. **Eligibility check** — accounts that are exhausted or have critically low 7d quota with distant resets are filtered out. If ALL accounts are ineligible, the best-scoring one is used as a fallback.
2. **Scoring** — eligible accounts are ranked by 5h-based mode score + 7d health adjustment.

**Team account bonus** — Team plan accounts receive a +20 scoring bonus in `max-remaining` and `drain-first` modes. In `round-robin` mode, team accounts are placed in a higher tier so they are always preferred over non-team accounts.

> **Note:** After switching accounts, you must **restart Codex** for it to pick up the new `auth.json`. The Codex CLI reads `auth.json` at startup and does not watch for file changes.

### How Scoring Works

**5h determines "who to use", 7d determines "how safe it is to use them".**

```
final_score = 5h_mode_score + 7d_adjustment   (max-remaining & drain-first)
final_score = team_tier + least_recently_used   (round-robin)
```

For `max-remaining` and `drain-first`, the 5h window is the primary factor — it decides which account to use right now. The 7d window acts as a safety modifier: when weekly quota gets low, it gradually penalizes the account, but cannot completely override a strong 5h advantage.

For `round-robin`, the 7d adjustment is not used. Instead, the eligibility gate filters out 7d-critical accounts, and remaining eligible accounts are rotated by least-recently-used order.

#### 7d Health Adjustment (max-remaining & drain-first)

Applied additively after 5h scoring (range: -300 to 0):

- **7d remaining >= safety margin (default 20%)** → no penalty (safe zone)
- **7d remaining < safety margin** → penalty increases as remaining% drops
- **7d resets within 48h** → penalty is reduced (up to 80% relief), since the weekly quota will recover soon
- **7d resets far away** → full penalty, since exhausting 7d means days of lockout
- **No 7d data** → mild penalty (-50) for unknown state

The max penalty of 300 is enough to tip close 5h races but cannot override a large 5h advantage — keeping 5h as the primary decision factor.

#### Eligibility Gate

Accounts are marked **ineligible** when:
- Either window is fully exhausted (≥100%), OR
- 7d remaining < critical threshold (25% of safety margin, min 1%) AND 7d resets more than 48h away

Ineligible accounts are excluded from selection unless ALL accounts are ineligible, in which case the best-scoring one is used as a last resort.

### Selection Modes

Three modes control how accounts are ranked. The 7d adjustment is applied to `max-remaining` and `drain-first`; `round-robin` relies solely on the eligibility gate for 7d protection.

| Mode | CLI flag | Description |
|------|----------|-------------|
| `max-remaining` | `-m max-remaining` | **Default.** Pick the account with the most remaining 5h quota |
| `drain-first` | `-m drain-first` | Prefer accounts whose 5h reset is imminent — spend "free" quota first |
| `round-robin` | `-m round-robin` | Rotate through eligible accounts evenly |

#### `max-remaining` (default)

**Strategy: maximize immediate headroom.**

Selects the account with the most remaining quota in the 5h window. Simple and predictable — always gives you the account that can handle the longest session right now.

5h scoring:
1. 7d at 100% → heavily penalized (account unusable, score 0–100)
2. 5h has remaining quota → score = `1000 + remaining%` (range 1000–1100)
3. 5h exhausted, 7d OK → medium score based on time until 5h reset (range 0–500)
4. No usage data → neutral score (50)

**Best for:** single-account setups, or when you always want the "fullest" account.

#### `drain-first`

**Strategy: spend quota that will be "free" soon, preserve quota that won't.**

The key insight: if an account's 5h window resets in 30 minutes, any quota you spend now is effectively free — it comes back soon regardless. Meanwhile, an account with 4 hours until reset should be saved as a reserve.

5h scoring:
1. 7d at 100% → heavily penalized (score 0–100)
2. 5h remaining below `min_remaining` threshold (default 5%) → demoted (score 500–600). Too little quota to sustain a session, but still ranks above fully exhausted accounts
3. 5h has remaining quota above threshold → score = `1000 + reset_urgency_bonus` (range 1000–1300). Closer to reset = higher bonus
4. 5h exhausted → medium score based on time until reset (range 0–500)

**Example with 5 accounts (all 7d healthy):**

| Account | 5h Used | 5h Resets in | 5h Score | 7d Adj | Final | Why |
|---------|---------|-------------|----------|--------|-------|-----|
| C | 60% | 20 min | ~1280 | 0 | ~1280 | Decent quota + resets very soon → spend "free" quota |
| D | 40% | 45 min | ~1255 | 0 | ~1255 | Good quota + resets soon → second choice |
| A | 10% | 4h | ~1060 | 0 | ~1060 | Lots of quota but distant reset → save as reserve |
| B | 97% | 10 min | ~597 | 0 | ~597 | Below threshold → demoted despite imminent reset |
| E | 100% | 2h | ~380 | 0 | ~380 | Exhausted → wait for reset |

**Example showing 7d impact:**

| Account | 5h Used | 5h Resets | 7d Used | 7d Resets | 5h Score | 7d Adj | Final |
|---------|---------|-----------|---------|-----------|----------|--------|-------|
| A | 0% | 30 min | 10% | 5d | 1300 | 0 | **1300** |
| B | 50% | 2h | 30% | 4d | 1180 | 0 | **1180** |
| C | 0% | 30 min | 90% | 12h | 1300 | -60 | **1240** |
| D | 0% | 30 min | 95% | 6d | 1300 | -225 | **1075** |

Account D has full 5h quota but 7d is critical (5% remaining, 6 days to reset) — the -225 penalty drops it below B despite its 5h advantage.

**Best for:** 3+ accounts where you want seamless rotation and maximum total throughput.

#### `round-robin`

**Strategy: spread wear evenly across all accounts.**

Ignores quota levels (beyond eligibility). Picks the eligible account that was least recently selected by `codex-switch use`. Team accounts are placed in a higher tier and always preferred over non-team accounts.

Scoring:
1. Ineligible accounts (exhausted or 7d-critical with distant reset) → excluded
2. Team accounts → higher tier, least recently used wins within tier
3. Non-team accounts → lower tier, least recently used wins within tier

**Best for:** large account pools where even distribution matters more than optimal quota utilization.

### Configuring the Selection Mode

**CLI flag (per-invocation override):**

```bash
codex-switch use -m drain-first
codex-switch use --mode round-robin
```

**Config file (persistent default):**

```toml
# ~/.codex-switch/config.toml
[use]
mode = "max-remaining"      # max-remaining | drain-first | round-robin
min_remaining = 5           # drain-first: demote accounts below this 5h remaining% (default: 5)
safety_margin_7d = 20       # 7d safety margin: penalty kicks in below this remaining% (default: 20)
```

CLI `-m` flag always overrides the config file setting.

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
- **Headless servers (no browser):** Use `codex-switch login --device` for device code flow — displays a URL and code to enter on any device with a browser

### Windows

- Default Codex auth path: `%USERPROFILE%\.codex\auth.json`
- Browser opens via `rundll32.exe url.dll,FileProtocolHandler` (fallback: `webbrowser` crate)
- File manager opens via `explorer.exe`
- Terminal: works with Windows Terminal, PowerShell, and cmd.exe
- TUI rendering uses Windows Console API via `crossterm`
- **Recommended terminal: [Windows Terminal](https://aka.ms/terminal).** Git Bash (mintty) has known compatibility issues with TUI rendering — use Windows Terminal or PowerShell instead

## JSON Output

Most commands support `--json` for machine-readable output (except `tui` and `open`):

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

## Changelog

See [docs/CHANGELOG.md](docs/CHANGELOG.md) for a detailed list of changes in each release.

## License

[MIT](LICENSE)
