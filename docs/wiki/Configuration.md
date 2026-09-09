# Configuration

`codex-switch` uses `~/.codex-switch` by default. Set `CODEX_SWITCH_HOME` to relocate its profiles, custom providers, cache, locks, and logs. This does not change Codex's own home; set `CODEX_HOME` for that.

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
| `$CODEX_SWITCH_HOME/providers/<alias>/models.json` | Generated Codex model catalog passed at launch (`/model` list plus metadata). |
| `$CODEX_SWITCH_HOME/provider-runs/<identity_id>/<run_id>/` | Persistent per-launch Codex homes keyed by stable provider identity. Concurrent runs never share provider sqlite/config state; history remains addressable after alias rename and is retained as a tombstone after removal. |
| `$CODEX_SWITCH_HOME/deleted-profiles/` | Recoverable deleted profiles. |
| `$CODEX_SWITCH_HOME/current` | Current alias marker. |
| `$CODEX_SWITCH_HOME/cache.json` | Per-profile usage cache. |
| `$CODEX_SWITCH_HOME/config.toml` | Optional settings. |
| `$CODEX_SWITCH_HOME/logs/` | Diagnostic logs: one file per day, 3 calendar days retained, with a 10 MiB approximate total target. |
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

[launch]
restore_delay_secs = 3             # seconds before restoring auth.json after launch
```

`launch.restore_delay_secs` is a compatibility delay, not a handshake; increase it only if the local Codex process reads authentication later than three seconds after launch.

Usage refresh and warmup are explicit one-time operations: `list --force` bypasses the usage cache, and `warmup [alias]` activates quota windows for one or all profiles. The TUI `t` key enables session-only automatic refresh; it does not write a scheduler configuration. If periodic work is needed, install a user-level OS task that invokes these commands; see [Optional OS scheduling](Feature-Guide#optional-os-scheduling).

The old `[daemon]` section is no longer read. Remove its keys when migrating; unknown TOML sections continue to be ignored by the configuration decoder. The unified selection settings under `[use]` remain active.

## Environment variables

| Variable | Effect |
|---|---|
| `CODEX_HOME` | Codex's own home; `auth.json` and Codex's `config.toml` live here (default `~/.codex`). Paths containing `..` are rejected. |
| `CODEX_SWITCH_HOME` | Relocates codex-switch state (default `~/.codex-switch`); an empty value is ignored. |
| `CODEX_SWITCH_METADATA_FALLBACK` | Catalog metadata fallback during explicit provider model sync: HTTP(S) URL, JSON file path, or `none`. Default is the public OpenRouter models list. Per-provider `--metadata-fallback` wins when set. |
| `CODEX_SWITCH_OPENROUTER_MODELS_URL` | Older alias for the same fallback URL when `CODEX_SWITCH_METADATA_FALLBACK` is unset. |
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

Commands write best-effort diagnostic events to `$CODEX_SWITCH_HOME/logs/`, one file per calendar day, keeping 3 days and an approximately 10 MiB total target; concurrent processes can temporarily exceed that target. The TUI keeps `INFO` and above in its Logs tab and in its file log, while ordinary CLI stderr remains `ERROR` by default. `--debug` wins over `RUST_LOG`.

## Platform integration

There is no project-managed service integration. A user may schedule `list --force`, `warmup`, or another explicit CLI operation with the platform scheduler; task installation, environment variables, output handling, and removal remain under the user's control. See the [OS scheduling examples](Feature-Guide#optional-os-scheduling).

## Next steps

- See what these settings control in the [Feature guide](Feature-Guide).
- Custom API provider storage and launch overlay: [Custom API providers](Providers).
- Look up the flags that override configuration in the [Command reference](Command-Reference).
- Diagnose configuration errors with [Troubleshooting](Troubleshooting).
