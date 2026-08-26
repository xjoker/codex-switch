# Command reference

The installed binary remains authoritative: use `codex-switch --help` and `codex-switch <command> --help` for the exact flags and examples supported by your version.

## Commands

| Command | Purpose |
|---|---|
| `login [--device] [alias]` | Add or reauthorize a profile through browser PKCE or device-code login. If the alias already exists, it is reauthorized; otherwise a new profile is created. |
| `import <path> [alias]` | Validate and import one `auth.json`, or recursively scan a directory for JSON files. The alias applies to single-file imports only; directories auto-assign aliases. An account that is already saved (same file, or same `account_id` and email) is skipped instead of duplicated, so its single-use refresh token is not spent. |
| `list [-f]` | Show profiles, usage, and availability; `-f` / `--force` bypasses the cache. |
| `use [alias] [--consume-card]` | Switch explicitly, or omit the alias to auto-select with the unified scoring algorithm. When the pool is exhausted, `--consume-card` consumes the earliest-expiring reset card to revive an account (auto-select only; ignored when an alias is given). |
| `launch [alias] [--consume-card] [--model <id>] -- [args]` | Start Codex with the best (or specified) ChatGPT profile's auth, or with a custom API provider when `alias` names one. For a provider, `--model` selects a saved model; otherwise it is forwarded to Codex. Everything after `--` is passed through to Codex. Auto-select (no alias) is ChatGPT-only. |
| `provider add <alias> --base-url <URL> --model <id>` | Save a custom API provider. `--model` is repeatable; the first is the default. `--reasoning` / `--no-web-search` attach to the most recent `--model`. The API key is read from a hidden prompt, or from stdin with `--api-key-stdin` — never from argv. |
| `provider list` | List saved providers (no keys). |
| `provider show <alias>` | Show one provider; the key is redacted. |
| `provider rename <old> <new>` | Rename a provider (directory + derived ids). |
| `provider remove <alias> [-y]` | Delete a provider and its stored key; `-y` / `--yes` skips the prompt. Non-interactive and `--json` runs require `--yes`. |
| `reset-card <alias> [-y]` | Consume the earliest-expiring reset card for a profile after confirmation; `-y` / `--yes` skips the prompt. |
| `warmup [alias]` | Send a minimal request to activate the quota-window countdown for one or all profiles. |
| `rename <old> <new>` | Rename a saved profile. |
| `delete <alias> [-y]` | Move an inactive profile into recoverable deleted storage; `-y` / `--yes` skips the prompt. |
| `daemon start [--foreground]` | Start the Beta daemon, detached by default; `--foreground` is for service managers. |
| `daemon stop` | Stop a running Beta daemon. |
| `daemon status` | Report daemon support, service, process, configuration, and pending-switch state. |
| `daemon install` | Install the native user service: LaunchAgent on macOS, systemd on Linux, Task Scheduler on Windows (elevated PowerShell required). |
| `daemon uninstall` | Remove the native user service. |
| `self-update [--check] [--dev\|--stable] [--version <VERSION>]` | Check or update a direct installation. Without flags it stays on the current channel; `--version` installs a specific newer stable version and conflicts with the channel flags. |
| `tui` | Open the interactive terminal dashboard. |
| `open` | Open the codex-switch data directory in the platform file manager. |

## Global options

| Option | Environment variable | Behavior |
|---|---|---|
| `--json` | — | Compact structured output (supported by `list`, `use`, `reset-card`, `rename`, `delete`, `login`, `import`, `self-update`, `daemon status`, `provider add`, `provider list`, `provider show`, `provider rename`, `provider remove`). |
| `--json-pretty` | — | Indented structured output. |
| `--proxy <URL>` | `CS_PROXY` | Override proxy configuration for this process; supports `http(s)://`, `socks4://`, `socks5://`, and `socks5h://` (remote DNS). |
| `--color <auto\|always\|never>` | `CS_COLOR` | Control terminal color. `NO_COLOR` disables color regardless of this option. |
| `--debug` | — | Emit diagnostic information (HTTP requests, API responses, cache status) to stderr; redact it before sharing. |
| `-V`, `--version` | — | Print the binary version. |

## Automation contract

- Structured data is written to stdout; progress and diagnostics are written to stderr.
- JSON and other non-interactive execution never consumes a reset card or deletes a profile without an explicit opt-in flag.
- `launch` treats everything after `--` as Codex CLI arguments. When `alias` names a custom provider, Codex is started with `-c` overrides and the key in the child environment; `$CODEX_HOME/auth.json` is not swapped.
- A manual `use` affects the next Codex process and accepts ChatGPT profile aliases only. Restart an already-running Codex process to load the new `auth.json`.
- Update checks are manual except for the one check performed when the TUI starts.

Examples:

```bash
codex-switch --json list
codex-switch --json use work
codex-switch launch work -- --model gpt-5.4
codex-switch provider add openrouter --base-url https://openrouter.ai/api/v1 --model openai/gpt-5.3-codex
codex-switch launch openrouter
codex-switch self-update --check
```

## Provider

`provider add` required flags are `--base-url` and at least one `--model`. `--model` is repeatable; the first is `default_model`. `--reasoning EFFORT` and `--no-web-search` attach to the most recent `--model`. Optional `--env-key` defaults to `CODEX_SWITCH_<ALIAS>_KEY`; `--wire-api` defaults to `responses` (the only protocol current Codex accepts). `--set KEY=VALUE` (repeatable) saves a provider-level `codex -c` override. All per-model and `--set` values are passed to Codex verbatim (only the `KEY=VALUE` shape is checked for `--set`). `--api-key-stdin` is required when there is no interactive terminal. `provider rename <old> <new>` moves the directory and re-derives `provider_id` / `env_key`. `launch <alias> --model <id>` selects a saved model on a provider.

The alias must not collide with a ChatGPT profile, another provider, or Codex's reserved ids `openai`, `ollama`, and `lmstudio`. Removal is immediate and is not archived under `deleted-profiles/`.

See [Custom API providers](Providers) for OpenRouter, DeepSeek-via-gateway, storage, and the no-argv key contract.

## TUI shortcuts

Two tabs: **Accounts** and **Providers**. `Tab` / `Shift+Tab` switches between them. `q` and `h` are global.

### Accounts tab

`Enter` opens the scrollable detail and action menu for the selected account; if accounts are marked, it opens the batch menu instead.

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Navigate |
| `Enter` | Open the account menu, or the batch menu when accounts are marked |
| `/` | Filter accounts |
| `r` | Refresh visible accounts |
| `a` | Add a new account |
| `t` | Toggle auto-refresh |
| `W` | Toggle auto-warmup for accounts whose 5h window has expired |
| `i` | Toggle the compact quota panel on the main view |
| `s` | Cycle sort order (name / quota / status) |
| `Space` | Mark or unmark an account |
| `u` (account menu) | Switch to the selected account |
| `c` (account menu) | Confirm and consume the earliest-expiring reset card |
| `w` (account menu) | Warm up the selected account |
| `l` (account menu) | Re-login the selected account |
| `n` (account menu) | Rename the selected account |
| `d` (account menu) | Delete the selected account (confirmation required) |
| `r` / `w` / `l` / `d` (batch menu) | Refresh, warm up, re-login, or delete the marked accounts |
| `h` | Show the complete shortcut list |
| `Esc` | Clear filter/marks or close the current popup |
| `q` | Quit |

### Providers tab

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Navigate |
| `a` | Add a provider (form dialog) |
| `e` / Enter | Edit the selected provider |
| `n` | Rename the selected provider |
| `d` | Remove the selected provider (confirmation required) |
| `Tab` | Switch to Accounts |
| `h` | Show help |
| `q` | Quit |

The Providers table never renders the stored key. Launching a provider is CLI-only (`codex-switch launch <alias>`).

Destructive or consumptive actions always require confirmation.

## Next steps

- See how these commands combine into workflows in the [Feature guide](Feature-Guide).
- Custom API endpoints, OpenRouter, and key handling: [Custom API providers](Providers).
- Adjust defaults, proxy, and daemon behavior in [Configuration](Configuration).
- Check update channels and flags in [Updating](Updating).
