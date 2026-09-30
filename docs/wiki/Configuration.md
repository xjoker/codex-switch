# Configuration

`codex-switch` uses `~/.codex-switch` by default. Set `CODEX_SWITCH_HOME` to relocate its profiles, custom providers, cache, locks, and logs. This does not change Codex's own home; set `CODEX_HOME` for that.

Configuration is optional: a missing `config.toml` means defaults. An existing but unreadable or invalid file fails fast with its path instead of being silently ignored.

## Authentication prerequisite

The live Codex credential store must be file-backed because switching replaces `$CODEX_HOME/auth.json` atomically. Add the following to `$CODEX_HOME/config.toml`:

```toml
cli_auth_credentials_store = "file"
```

Explicit `keyring`, `auto`, and `ephemeral` modes are rejected. ChatGPT operations also check effective authentication requirements from the user configuration, legacy managed defaults, system `requirements.toml`, and forced macOS MDM preferences. Requirements override ordinary defaults; a user setting cannot relax an administrator's store, login-method or workspace restriction. Windows system requirements are read from `%ProgramData%\OpenAI\Codex\requirements.toml`; Unix systems use `/etc/codex/requirements.toml`.

ChatGPT file-login operations are refused when `OPENAI_FEDERATION_RULE_ID` or `OPENAI_IDENTITY_TOKEN_FILE` is present, including an empty value, because Codex selects workload identity federation ahead of stored OAuth credentials. codex-switch does not unset these variables or edit managed policy. API provider keys remain a separate launch path.

A managed configuration with `forced_login_method = "api"`, an allowlist excluding ChatGPT, or no permitted ChatGPT workspace is incompatible with ChatGPT login profiles. Non-default `chatgpt_base_url` routing is not supported; relevant ChatGPT requests are refused before network access rather than sent to the default backend. Unreadable, oversized or invalid policy is reported explicitly.

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
| `$CODEX_SWITCH_HOME/provider-runs/<identity_id>/<run_id>/` | Persistent launch metadata and model catalogs keyed by stable provider identity. Native runs share the normal Codex home and use separate writable profiles; legacy isolated homes remain available for their existing history. Runs that never produced a session are swept on the next launch. |
| `$CODEX_HOME/cs-*.config.toml` | Native per-run provider config overlays; shared MCP/skills/plugins remain in the normal home. No provider API key is stored here. Files for dead runs are reclaimed automatically, and `provider remove` deletes that provider's files. |
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
restart_app_server = true          # restart a running Codex app-server daemon after the live auth.json changes

[launch]
restore_delay_secs = 3             # seconds before restoring auth.json after launch
```

`use.restart_app_server` (also the `use.restart_app_server` toggle in the TUI Settings tab) controls what `use`, an auto-selecting `use`, the TUI `u` switch, and the `login` paths that activate credentials do when the live `auth.json` changed and a Codex app-server daemon (Codex 0.157+) is running. With `true`, codex-switch runs `codex app-server daemon restart` so new sessions use the switched account; sessions attached to the daemon reconnect, and a turn in progress is interrupted. With `false`, the daemon is left alone and a note shows the manual command. The live file is compared as canonical JSON before and after the change, so a rewrite that only changes formatting (for example by Codex's own token refresh) never restarts the daemon. Each daemon call, including collecting its output, is bounded to 15 seconds; a daemon that does not answer is reported as a failed restart and never fails the switch. `launch` does not use this setting: a ChatGPT launch avoids the daemon with `--no-daemon` instead (see [Feature guide](Feature-Guide#launch-codex-with-a-profile)).

`launch.restore_delay_secs` is a compatibility delay, not a handshake; increase it only if the local Codex process reads authentication later than three seconds after launch. A value of `0` is invalid (the original `auth.json` would be restored before Codex has read the staged one), so it is replaced by `3` with a startup warning.

`[use]` keys `mode` and `min_remaining` from older releases are ignored with a warning; the adaptive algorithm replaced all selection modes. The TUI Settings tab writes the whole file, so comments and unknown keys are not preserved.

Usage refresh and warmup are explicit one-time operations: `list --force` bypasses the usage cache, and `warmup [alias]` activates 5h quota windows for profiles that have one. Accounts with only a 7-day window are skipped. The TUI `t` key enables session-only automatic refresh; it does not write a scheduler configuration. If periodic work is needed, install a user-level OS task that invokes these commands; see [Optional OS scheduling](Feature-Guide#optional-os-scheduling).

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

Command failures are reported once: a human-readable error on stderr, or an error object on stdout with `--json`. The corresponding failure event stays in file logs and is excluded from CLI stderr even with `--debug` or `RUST_LOG`, so diagnostics do not duplicate the error message.

## Platform integration

There is no project-managed service integration. A user may schedule `list --force`, `warmup`, or another explicit CLI operation with the platform scheduler; task installation, environment variables, output handling, and removal remain under the user's control. See the [OS scheduling examples](Feature-Guide#optional-os-scheduling).

## Next steps

- See what these settings control in the [Feature guide](Feature-Guide).
- Custom API provider storage and launch overlay: [Custom API providers](Providers).
- Look up the flags that override configuration in the [Command reference](Command-Reference).
- Diagnose configuration errors with [Troubleshooting](Troubleshooting).
