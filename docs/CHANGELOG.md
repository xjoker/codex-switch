# Changelog

## v0.0.11 — 2026-04-07

### Added

- **`warmup` command** — `codex-switch warmup [alias]` sends a minimal Codex request (`ping`) to activate the 5h/7d quota window countdown for a fresh account. Omit alias to warm up all saved profiles concurrently. Already-active accounts (reset time still in the future) are automatically skipped. Supports `--json` output with per-account results and a top-level `ok` field
- **TUI warmup** — Press `w` to warm up the selected account or `W` to warm up all accounts. Usage is automatically refreshed after warmup completes
- **Pace Marker** — Usage bars (CLI and TUI) now display a `|` pace marker showing expected consumption based on elapsed window time, making it easy to see if you're ahead or behind budget
- **Dev update channel** — `self-update --dev` installs the latest dev build; `self-update --stable` switches back. Without flags, auto-detects the current channel. Dev builds use timestamped semver (`0.0.11-dev.YYYYMMDDHHmmss`) for proper update detection
- **Install scripts enhanced** — `install.sh --dev` / `$env:CS_DEV="1"` for dev channel install; `install.sh --uninstall` / `$env:CS_UNINSTALL="1"` for clean removal. Homebrew-installed versions are detected and blocked from direct-install to prevent PATH conflicts
- **Startup auth change detection** — On launch, codex-switch compares the live `~/.codex/auth.json` against all saved profiles. If a new account is detected (e.g., the user ran `codex login`), it prompts to save as a new profile. If tokens were refreshed for an existing account, it prompts to update the corresponding profile
- **Non-interactive safety** — When stdin is not a TTY (pipes, cron, CI), startup detection informs without silently mutating state. EOF on stdin is treated as rejection

### Changed

- **TUI mark-aware operations unified** — `r` (refresh) and `w` (warmup) now operate on marked accounts when marks exist, or all accounts when none are marked. Separate `b` (batch refresh) and `W` (warmup all) keys removed
- **TUI usage gauges redesigned** — Bars now use block characters (`█` used, `░` remaining) with a `|` pace marker and an `XX% used / YY% left` suffix for clearer at-a-glance quota visibility. The pace marker label (`↑ pace`) is right-aligned alongside `resets in …` on row 2, and is suppressed automatically when there is not enough terminal width to avoid overlap
- **JSON usage output extended** — `JsonWindow` now includes `remaining_percent`, `pace_percent`, and `over_pace` fields (all `skip_serializing_if = None`; existing fields are unchanged)
- **Linux static linking** — Linux release binaries now use musl (static linking) instead of glibc, eliminating `GLIBC_2.xx not found` errors on older distributions. ARM64 Linux builds use `cross` for proper musl cross-compilation
- **Cargo package name** — The crate `name` field in `Cargo.toml` changed from `cs` to `codex-switch`. The binary name was already `codex-switch` and is unaffected; only `cargo install`/source build workflows that referenced the old package name need updating

### Fixed

- **Gauge row-2 text overlap** — When the quota window is nearly expired and the pace marker sits near the right edge, `resets in …` is now right-aligned and `↑ pace` is only shown when there is space, preventing the two strings from running together
- **Zero-width bar crash** — On extremely narrow terminals where `bar_width` computes to 0, the gauge now skips bar rendering entirely and shows only the reset time, instead of placing a pace marker at an out-of-bounds position
- **Warmup `--json` double output** — On partial failure, `warmup_cmd` previously emitted a results object then let `main()` emit a second error object. Now a single `{"ok": false, "results": […]}` object is printed and the process exits with code 1
- **Warmup UTF-8 truncation panic** — HTTP error bodies were truncated by byte index, which panicked on multi-byte UTF-8 characters. Now truncated by Unicode scalar count via `chars().take(160)`
- **TUI warmup rate limiting** — `warmup_all` (`W`) now respects `network.max_concurrent` via the shared `usage_limiter` semaphore, consistent with usage fetch behaviour
- **TUI warmup dedup and stale refresh** — Rapid `w`/`W` presses no longer trigger multiple forced refreshes for the same account. On warmup success the account always gets a fresh usage fetch so the newly opened quota window appears immediately
- **`auto_track_current` current pointer sync** — When `auth.json` matches an existing saved profile but the `current` file points elsewhere, the pointer is now updated automatically instead of being left stale
- **Empty `account_id` treated as present** — `/tokens/account_id: ""` was not filtered, preventing the email-only identity fallback. Empty strings are now treated as `None`, consistent with JWT claims filtering
- **`list` respects startup detection** — When startup auth detection already handled the live `auth.json`, `list` no longer runs `auto_track_current()` a second time, preventing silent saves after an explicit user rejection
- **Dev update 404 handling** — `check_for_dev_update` now uses proper HTTP status code detection instead of string matching; network errors are propagated while 404 (no dev release) returns `None`
- **Dev update redundant reinstall** — `self_update_dev` now compares versions before downloading, avoiding re-download when already on the latest dev build
- **Dev version extraction safety** — `extract_release_version` only attempts dev name parsing when `tag_name == "dev"`, preventing false matches on stable releases
- **Cache lock poisoning silent bypass** — `CACHE_LOCK.lock().ok()` silently dropped poisoned locks; now propagates error via `map_err`
- **Token refresh ignored OAuth error field** — `do_refresh_token` logged `error` field as warning but continued parsing tokens; now bails immediately
- **`make_unique_alias` infinite loop** — Long aliases near `MAX_ALIAS_LEN` caused suffix truncation to produce the same candidate forever; now capped at 1000 retries
- **Home directory fallback to `.`** — `codex_auth_path()` and `app_home()` fell back to current directory when `dirs::home_dir()` returned `None`; now returns an error
- **Warmup with expired tokens** — Warmup used stored `access_token` without refresh, failing with 401 for expired tokens; now pre-refreshes expiring tokens and retries on 401/403
- **TUI round-robin state drift** — TUI switches did not update `cache.last_used`, causing round-robin to diverge from CLI behavior
- **TUI profile load errors silenced** — `list_profiles()` errors were swallowed via `unwrap_or_default()`; now logs a warning
- **Version comparison silent failure** — `compare_versions` returned `None` on unparseable semver without logging; `self-update` would report "already up to date" instead of flagging the issue

## v0.0.10 — 2026-04-02

### Changed

- **Dependency upgrades** — Bumped `rand` 0.9→0.10, `sha2` 0.10→0.11, `toml` 1.1.0→1.1.1, `zip` 2.4→8.4
- **Windows terminal recommendation** — README now recommends Windows Terminal over Git Bash (mintty) for TUI, due to known crossterm compatibility issues

### Fixed

- **Clippy warnings** — Replaced manual `Default` impl with `#[derive(Default)]` on `ConfigSelectMode`; used `RangeInclusive::contains` in test assertion

## v0.0.9 — 2026-03-29

### Added

- **Selection modes for `use` command** — Three modes control auto-select behavior via `--mode`/`-m` flag or `[use].mode` config:
  - `max-remaining` (default): pick the account with the most remaining 5h quota
  - `drain-first`: prefer accounts whose 5h reset is imminent — spend "free" quota first, save slow-to-reset quota as reserve. Accounts below `min_remaining` threshold (default 5%) are demoted
  - `round-robin`: rotate through eligible accounts evenly by least-recently-used order; team accounts are preferred in a higher tier
- **7d-aware scoring** — All scoring modes now consider the 7d (weekly) window as a safety modifier:
  - Two-phase selection: eligibility gate filters exhausted or 7d-critical accounts, then scoring ranks the rest
  - 7d health adjustment (max-remaining & drain-first): additive penalty (-300 to 0) when 7d remaining falls below `safety_margin_7d` (default 20%), with up to 80% relief when 7d resets within 48h
  - Accounts with critically low 7d remaining and distant reset are marked ineligible (unless all accounts are in this state)
- **Round-robin last-used tracking** — `cache.json` now tracks when each profile was last selected by `use`, enabling fair rotation
- **New config options** — `[use]` section in `config.toml`:
  - `mode` — default selection mode (default: `max-remaining`)
  - `min_remaining` — drain-first 5h demotion threshold in % (default: 5)
  - `safety_margin_7d` — 7d safety margin in % (default: 20)
- **JSON output includes mode** — `codex-switch --json use` now includes the `mode` field in the response

### Changed

- **Explicit `use <alias>` updates round-robin history** — Manual account switches are now tracked for round-robin rotation, preventing re-selection of a just-used account
- **Cache rename preserves last-used data** — `rename` now independently migrates both usage cache and last-used timestamps

## v0.0.8 — 2026-03-28

### Fixed

- **Profile dedup incorrectly merges different users in the same Team** — `find_profile_by_identity()` previously matched on `account_id` alone, but OpenAI's `chatgpt_account_id` is a workspace-level identifier shared across all members. Users with different emails but the same Team workspace were incorrectly merged into one profile. Now requires both `account_id` AND `email` to match for dedup. ([#8](https://github.com/xjoker/codex-switch/issues/8))

## v0.0.7 — 2026-03-27

### Added

- **`rename` CLI command** — `codex-switch rename <old> <new>`, previously only available in TUI
- **Team account priority** — `codex-switch use` auto-selection gives Team plan accounts a +20 scoring bonus, preferring them when quotas are similar
- **Alias validation** — All user-facing alias inputs are validated against a safe character set, preventing path traversal attacks
- **Common usage scenarios in README** — Shell integration, CI automation, and optional cron-based token refresh
- **Troubleshooting section in README** — `--debug` usage guide and issue reporting instructions

### Changed

- **ASCII-only terminal output** — All user-visible CLI/TUI strings now use pure ASCII characters for Windows GBK codepage compatibility
- **`--json` scope clarified** — Help text and README now accurately state that `tui` and `open` do not support `--json`
- **README documentation sync** — Chinese README aligned with English version (added HTTPS_PROXY example, Building section, consistent chapter ordering)
- **Version examples updated** — README and CLI help examples updated to `0.0.7`

### Fixed

- **TUI refresh race condition** — Token writeback to live `auth.json` now re-checks the active profile before writing, preventing stale tokens from overwriting a freshly switched account
- **Auth file permissions** — `auth.json` files are now created with mode `0600` and directories with `0700` on Unix, preventing other users from reading tokens
- **Proxy credential leak** — `--debug` logging now sanitizes `user:pass` in proxy URLs to `***:***`
- **Silent error swallowing** — Config load failures, cache write failures, and token refresh persistence failures now emit `tracing::warn!` instead of being silently ignored
- **Clippy warnings** — Resolved 4 `if_same_then_else` warnings in color threshold logic
- **Dead code cleanup** — Removed 2 unnecessary `#[allow(dead_code)]` annotations
- **Rename cache cleanup** — Renaming a profile now migrates its cache entry to the new alias

## v0.0.6 — 2026-03-26

### Fixed

- **OAuth login broken since v0.0.4** — redirect_uri changed from `http://localhost:1455/...` to `http://127.0.0.1:1455/...`, which doesn't match OpenAI's registered URI, causing `unknown_error` on login
- **Removed random port fallback for PKCE login** — OAuth redirect_uri must exactly match the registered `localhost:1455`; random port fallback is only valid for device code flow

## v0.0.5 — 2026-03-26

### Added

- **OAuth port fallback** — When port 1455 is occupied, login automatically falls back to a random available port instead of failing
- **Token pre-refresh** — Access tokens expiring within 60 seconds are proactively refreshed before usage API requests, reducing 401 retry latency
- **Process detection on switch** — `use` command detects running Codex processes and warns before switching; use `--force` to override
- **Credits balance display** — Usage output now shows `credits_balance` and `unlimited_credits` in CLI, TUI, and `--json` output (backward-compatible with APIs that don't return credits data)
- **Unit test coverage** — 21 new unit tests covering JWT parsing/expiration, usage data parsing/scoring/availability, auth JSON structure, and cache deserialization compatibility

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
