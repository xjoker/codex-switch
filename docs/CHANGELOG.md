# Changelog

## Unreleased

## v0.0.4 — 2026-03-26

### Fixed

- **Improved error diagnostics** — All HTTP requests (token exchange, device code, usage API, token refresh) now display the full error source chain instead of a generic "error sending request" message, making proxy/TLS/network issues immediately diagnosable
- **HTTP client timeouts** — Added `connect_timeout(30s)` and `timeout(60s)` to the shared HTTP client, preventing indefinite hangs when a proxy or upstream is unresponsive
- **File I/O error context** — `write_auth`, `backup_auth`, profile directory operations (`list`, `delete`, `rename`, `import`), and `open` command now include file/directory paths in error messages
- **OAuth callback error context** — Local callback server (`accept`/`read`) errors now indicate they occurred during the OAuth login flow rather than showing raw socket errors
- **Usage API error clarity** — HTTP error responses from the usage API now clearly identify the failing endpoint instead of showing bare status codes
- **TUI error context** — Terminal draw/event errors now include operation context for easier troubleshooting

## v0.0.3 — 2026-03-26

### Added

- **Manual self-update** — New `codex-switch self-update` command for direct installs, plus `self-update --check` for on-demand release checks
- **Checksum-verified updates** — Direct-install self-update validates the release `.sha256` before replacing the current executable
- **Install-source awareness** — Homebrew installs are detected and redirected to `brew upgrade xjoker/tap/codex-switch`
- **Recursive directory import** — `import <path>` now accepts directories, scans recursively for `.json` files, validates them, and reports imported vs skipped files
- **Import stage reporting** — Directory import surfaces failures as `file_format`, `structure`, `usage_validation`, or `save`
- **Progress reporting** — `use`, `list`, and bulk `import` show a single-line progress indicator for large batches
- **Per-account cache timestamps** — Cached usage now preserves the original refresh time and exposes it as `usage.fetched_at` in JSON output
- **Device Code Flow** — `login --device` for headless servers without a browser (RFC 8628, polls `deviceauth` endpoint)
- **`--debug` flag** — Enable debug logging for HTTP requests, API responses, and cache status
- **`--json-pretty` flag** — Pretty-printed JSON output in addition to existing `--json` compact mode

### Changed

- **Manual-only update checks** — No automatic update checks on startup, `use`, `list`, or TUI launch
- **Cache scheduling** — `use`, `list`, and TUI now refresh only stale accounts by default; force-refresh paths still bypass cache
- **JSON hygiene** — Human messages and progress output are routed away from stdout so `--json` stays machine-readable
- **Network concurrency** — Configurable max concurrent usage requests (`[network] max_concurrent`, default 20) now applies across CLI and TUI refresh paths
- **`list` replaces `status`** — `codex-switch list` now fetches live usage (previously only `status` did); `status` command removed

### Fixed

- **Active profile deletion** — CLI now rejects deleting the currently active profile, matching TUI behavior
- **Zero-concurrency config** — Invalid `network.max_concurrent = 0` is normalized instead of hanging refresh paths
- **TUI refresh behavior** — TUI preloads cache first, refreshes only stale entries, and no longer emits stray auto-track text into the UI
- **Device flow polling** — OAuth device code flow now handles `slow_down` correctly and exits cleanly on Ctrl+C
- **Import validation** — `auth.json` imports now require valid token structure and perform a real account usability check unless explicitly skipped in tests

## v0.0.2 — 2026-03-25

### Changed

- **Homebrew release automation** — Homebrew formula updates now run inside the main release workflow instead of a separate post-release workflow
- **Multi-platform formula generation** — Release automation now generates checksums and Homebrew formula entries for all supported macOS and Linux targets
- **Dependency refresh** — Upgraded core crates including `dirs` 6.x, `toml` 1.x, and `rand` 0.9

### Fixed

- **Release packaging** — Replaced the single-URL Homebrew bump action with a custom multi-asset formula generator so `brew upgrade` tracks the correct archive for each platform
- **Login RNG compatibility** — OAuth PKCE state/verifier generation now uses the current `rand` 0.9 API

## v0.0.1 — Initial Release

### Features

- **Profile Management** — `codex-switch use`, `list`, `delete`, `import`, `login` commands
- **Interactive TUI** — ratatui-based terminal UI with account list, usage gauges, keyboard navigation (`j/k`, `Enter`, `r`, `n`, `d`)
- **Usage Dashboard** — Real-time 5h/7d quota monitoring via ChatGPT wham API with color-coded status (OK/Limited/Error)
- **Smart Auto-Switch** — `codex-switch use` without alias auto-selects the best account (7d limit checked first, then 5h remaining %)
- **OAuth PKCE Login** — Browser-based login flow with local callback server on port 1455
- **Token Auto-Refresh** — Automatic refresh_token flow on HTTP 401/403, persists new tokens
- **Auto-Detection** — `list`, `tui` auto-discover and save untracked `~/.codex/auth.json`
- **Deduplication** — Login/import matches by account_id > email, updates existing profiles instead of creating duplicates
- **TUI Rename** — Rename profiles in-place with `n` key
- **TUI Delete** — Delete profiles with `d` key (confirmation required, cannot delete active profile)
- **Proxy Support** — HTTP/HTTPS/SOCKS4/SOCKS5/SOCKS5H with authentication; CLI `--proxy`, `CS_PROXY` env, config file, standard env vars
- **Config File** — `~/.codex-switch/config.toml` for persistent proxy settings
- **Color Output** — `--color auto|always|never`, respects `NO_COLOR` env and terminal capability detection
- **JSON Output** — `--json` flag for all commands
- **Team/Org Display** — Shows workspace/organization name in plan label (e.g., "team · Personal")
- **Local Timezone** — Reset times displayed in local timezone (e.g., "2h30m (14:30)")
- **Retry with Backoff** — Network requests retry up to 3 times with 1-2s delay
- **Backup Management** — Auto-backup auth.json on switch, keeps only last 3 backups
- **Cross-Platform** — macOS (amd64/arm64), Linux (amd64/arm64), Windows (amd64/arm64)
- **One-Liner Install** — `curl | bash` for macOS/Linux, `irm | iex` for Windows
- **Homebrew** — `brew install xjoker/tap/codex-switch`

### Build Targets

| Platform | Architecture | Asset |
|----------|-------------|-------|
| macOS | Apple Silicon | `cs-darwin-arm64.tar.gz` |
| macOS | Intel | `cs-darwin-amd64.tar.gz` |
| Linux | x86_64 | `cs-linux-amd64.tar.gz` |
| Linux | ARM64 | `cs-linux-arm64.tar.gz` |
| Windows | x86_64 | `cs-windows-amd64.zip` |
| Windows | ARM64 | `cs-windows-arm64.zip` |
