# Configuration

`codex-switch` uses `~/.codex-switch` by default. Set `CODEX_SWITCH_HOME` to relocate its profiles, custom providers, cache, locks, logs, and daemon state. This does not change Codex's own home; set `CODEX_HOME` for that.

Configuration is optional: a missing `config.toml` means defaults. An existing but unreadable or invalid file fails fast with its path instead of being silently ignored.

## Authentication prerequisite

The live Codex credential store must be file-backed because switching replaces `$CODEX_HOME/auth.json` atomically. Add the following to `$CODEX_HOME/config.toml`:

```toml
cli_auth_credentials_store = "file"
```

Explicit `keyring`, `auto`, and `ephemeral` modes are rejected. A managed configuration with `forced_login_method = "api"` is also incompatible with ChatGPT login profiles.

### Why only the file store is supported

This is a deliberate limitation, not a temporary gap:

- Every reliability guarantee codex-switch makes — cross-process locking, atomic replacement, backup rotation — is built on file primitives. OS keyrings (macOS Keychain, Windows Credential Manager, Linux Secret Service) expose no locking or atomic-replace semantics, so a switch racing a running Codex process could silently select the wrong account instead of failing loudly.
- Codex's keyring entry layout is an undocumented internal format. It was already reworked once (June 2026, when Windows moved to an encrypted sidecar because of a Credential Manager size limit) and now differs between Windows and other platforms. Depending on it would break silently whenever Codex changes it.
- An `ephemeral` store persists nothing, so there is nothing to switch.

Accounts are added by logging in with `codex-switch login` or by importing an existing `auth.json`; codex-switch never reads credentials out of an OS keyring. If Codex was previously used with a keyring store, set `cli_auth_credentials_store = "file"` and log in again.

## Paths

| Path | Purpose |
|---|---|
| `$CODEX_HOME/auth.json` | Live authentication read by Codex. |
| `$CODEX_SWITCH_HOME/profiles/<alias>/auth.json` | Saved profile authentication. |
| `$CODEX_SWITCH_HOME/providers/<alias>/provider.toml` | Custom API provider definition and key (directory `0700`, file `0600`). |
| `$CODEX_SWITCH_HOME/deleted-profiles/` | Recoverable deleted profiles. |
| `$CODEX_SWITCH_HOME/current` | Current alias marker. |
| `$CODEX_SWITCH_HOME/cache.json` | Per-profile usage cache. |
| `$CODEX_SWITCH_HOME/config.toml` | Optional settings. |
| `$CODEX_SWITCH_HOME/daemon-state.json` | Last Beta daemon state snapshot. |
| `$CODEX_SWITCH_HOME/logs/` | Diagnostic logs: one file per day, 3 calendar days retained, 10 MiB total cap. |
| `$CODEX_SWITCH_HOME/*.lock` | Cross-process coordination files. |

Unset variables default to `~/.codex` and `~/.codex-switch` respectively (`%USERPROFILE%\.codex-switch` on Windows).

## Settings

All keys with their defaults:

```toml
[proxy]
url = "socks5h://user:pass@127.0.0.1:1080"  # no default; unset means no proxy from config
no_proxy = "localhost,127.0.0.1"

[cache]
ttl = 300                          # usage cache TTL in seconds

[network]
max_concurrent = 20                # concurrent usage requests; 0 is normalized to 1

[tui]
auto_refresh_interval_secs = 300   # minimum 30; lower values are raised to 30

[use]
safety_margin_7d = 20              # 7d headroom % below which scoring penalizes
team_priority = true               # prefer Team-plan accounts during selection

[daemon]
poll_interval_secs = 60            # usage poll; 0 is normalized to 60
switch_threshold = 80              # 5h usage % that triggers an auto-switch
cache_refresh_interval_secs = 300  # all-profile cache refresh; 0 is normalized to 300
auto_warmup = false                # master switch for background warmup
warmup_times = []                  # HH:MM slots; empty = warm during cache refresh when auto_warmup
timezone = ""                      # IANA name (Asia/Shanghai); empty = system local time
token_check_interval_secs = 300    # proactive token refresh; 0 is normalized to 300
notify = false                     # desktop notification on switch
log_level = "error"                # daemon log level; empty is normalized to "error"
defer_switch_while_codex_running = true  # hold a pending switch during interactive Codex sessions

[launch]
restore_delay_secs = 3             # seconds before restoring auth.json after launch
```

`launch.restore_delay_secs` is a compatibility delay, not a handshake; increase it only if the local Codex process reads authentication later than three seconds after launch.

### Timed warmup

`auto_warmup` is the master switch. TUI `W` is a separate session toggle and does not write this key.

- `auto_warmup = false`: the daemon never warms, even if `warmup_times` is set.
- `auto_warmup = true` and `warmup_times` empty: current behavior — cache refresh also warms inactive quota windows.
- `auto_warmup = true` and `warmup_times` non-empty: cache refresh only updates usage. Warmup runs at those `HH:MM` slots in `daemon.timezone`. Empty `timezone` uses the daemon process local timezone; set an IANA name such as `Asia/Shanghai` or `UTC` to pin the clock. A slot is due once today's time has passed in that zone and is newer than `last_warmup_slot` in `daemon-state.json`. Catch-up fires only the latest overdue slot today; yesterday is not replayed. Identity is stored as `YYYY-MM-DD HH:MM` in the schedule timezone (the slot, not the fire minute). A dedicated ~60s timer drives this, so poll backoff cannot skip a slot.

Invalid times are dropped with a warning. Duplicate times are merged. Two slots less than five hours apart warn because the later one is a no-op if the earlier warmup opened a 5h window; they are not rejected. An unknown timezone name warns and is kept in the file; due detection then uses system local time. Saving Settings from the TUI rewrites `config.toml` (comments and unknown keys are not preserved). Poll, token, and cache **intervals** still need a daemon restart; `warmup_times`, `timezone`, and `auto_warmup` are re-read about once a minute.

Edit the same keys from the TUI **Settings** tab (`s` saves). Unsaved form edits are kept if you leave the tab and come back; `Tab` does not change tabs while a field is being typed. `warmup_times` accepts one `HH:MM` or a comma/space-separated list; adding keeps focus on the add row. A pair less than five hours apart warns immediately.

The legacy `[use] mode` and `[use] min_remaining` keys are ignored and produce a startup warning; the unified scoring algorithm replaced the old selection modes.

## Environment variables

| Variable | Effect |
|---|---|
| `CODEX_HOME` | Codex's own home; `auth.json` and Codex's `config.toml` live here (default `~/.codex`). Paths containing `..` are rejected. |
| `CODEX_SWITCH_HOME` | Relocates codex-switch state (default `~/.codex-switch`); an empty value is ignored. |
| `CS_PROXY` | Proxy URL; same as `--proxy`. |
| `CS_COLOR` | Color mode; same as `--color`. |
| `NO_COLOR` | Disables color on CLI output regardless of other settings. The TUI still paints its designed palette. |
| `RUST_LOG` | Overrides the log filter; `--debug` has higher priority. |
| `CODEX_CA_CERTIFICATE`, `SSL_CERT_FILE` | Custom CA certificate for HTTPS, in Codex-compatible fallback order. |

## Proxy precedence

Proxy settings resolve in this order:

1. `--proxy`
2. `CS_PROXY`
3. `[proxy]` in `config.toml`
4. `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY`

Supported schemes:

| Scheme | DNS resolution | Authentication |
|---|---|---|
| `http://[user:pass@]host:port` | local | supported |
| `https://[user:pass@]host:port` | local | supported |
| `socks4://host:port` | local | not supported |
| `socks5://[user:pass@]host:port` | local | supported |
| `socks5h://[user:pass@]host:port` | remote (at the proxy) | supported |

Do not commit credentials in configuration files.

## Logging

Every command writes diagnostic logs to `$CODEX_SWITCH_HOME/logs/`, one file per calendar day, keeping 3 days and at most 10 MiB. Level resolution: `--debug` wins over `RUST_LOG`, which wins over `daemon.log_level`; the default is `error`. `daemon.log_level` applies only to `daemon` commands — it does not change logging for `list`, `use`, or other commands.

## Platform integration

- macOS uses a LaunchAgent for the Beta daemon.
- Linux uses a systemd user service; headless login should use `login --device`.
- Windows uses Task Scheduler and requires elevated PowerShell for daemon installation. Windows Terminal or PowerShell is recommended for the TUI.

> **`CODEX_SWITCH_HOME` and installed daemon services:** when `CODEX_SWITCH_HOME` is set in the shell that runs `daemon install`, its value is captured into the generated LaunchAgent plist, systemd unit, or Task Scheduler command so the installed service reads the same relocated store. The variable is read at install time; if you later change or unset it, re-run `daemon install` to update the service definition.

## Next steps

- See what these settings control in the [Feature guide](Feature-Guide).
- Custom API provider storage and launch overlay: [Custom API providers](Providers).
- Look up the flags that override configuration in the [Command reference](Command-Reference).
- Diagnose configuration errors with [Troubleshooting](Troubleshooting).
