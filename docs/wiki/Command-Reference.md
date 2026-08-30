# Command reference

The installed binary remains authoritative: use `codex-switch --help` and `codex-switch <command> --help` for the exact flags and examples supported by your version.

## Commands

| Command | Purpose |
|---|---|
| `login [--device] [alias]` | Add or reauthorize a profile through browser PKCE or device-code login. If the alias already exists, it is reauthorized; otherwise a new profile is created. |
| `import <path> [alias]` | Validate and import one `auth.json`, or recursively scan a directory for JSON files. The alias applies to single-file imports only; directories auto-assign aliases. An account that is already saved (same file, or same `account_id` and email) is skipped instead of duplicated, so its single-use refresh token is not spent. |
| `list [-f]` | Show profiles, usage, and availability; `-f` / `--force` bypasses the cache. |
| `use [alias] [--consume-card]` | Switch explicitly, or omit the alias to auto-select with the unified scoring algorithm. When the pool is exhausted, `--consume-card` consumes the earliest-expiring reset card to revive an account (auto-select only; ignored when an alias is given). |
| `launch [alias] [--consume-card] [--model <id>] [-- <codex-args>]` | Start Codex with the best (or specified) ChatGPT profile's auth, or with a custom API provider when `alias` names one. For a provider, `--model` before `--` selects a saved model; after `--` it is Codex's own `--model`. A known Codex subcommand (`exec`, `resume`, …) can start the argv without `--`. Tokens on both sides of `--` are kept. Auto-select (no alias) is ChatGPT-only. |
| `provider add <alias> --base-url <URL> (--model <id> \| --fetch-models)` | Save a custom API provider. `--model` is repeatable; the first is the default. `--fetch-models` imports chat slugs from `GET {base_url}/models` (embedding/reranker omitted; catalogs larger than 48 must use `--model` or TUI `f`). `--reasoning` / `--no-web-search` attach to the most recent `--model`. The API key is read from a hidden prompt, or from stdin with `--api-key-stdin` — never from argv. |
| `provider list` | List saved providers (no keys). |
| `provider show <alias>` | Show one provider; the key is redacted. |
| `provider fetch-models <alias> [--model <id>]` | Replace saved models with chat slugs from the provider's `GET /models`. Matching ids keep reasoning / `web_search`. Large catalogs require `--model`. |
| `provider probe <alias> [--model <id>]` | `POST {base_url}/responses` with only `model` (no `input`) to see if Codex can use the slug. Does not generate tokens. Default: every saved model. |
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
| `--json` | — | Compact structured output (supported by `list`, `use`, `launch`, `reset-card`, `rename`, `delete`, `login`, `import`, `self-update`, `daemon status`, `provider add`, `provider list`, `provider show`, `provider rename`, `provider remove`, `provider fetch-models`, `provider probe`). `launch --json` prints one envelope after Codex exits; Codex stdout/stderr are fields of that envelope. |
| `--json-pretty` | — | Indented structured output. |
| `--proxy <URL>` | `CS_PROXY` | Override proxy configuration for this process; supports `http(s)://`, `socks4://`, `socks5://`, and `socks5h://` (remote DNS). |
| `--color <auto\|always\|never>` | `CS_COLOR` | Control CLI terminal color. `NO_COLOR` disables CLI color regardless of this option. The TUI still paints its designed palette. |
| `--debug` | — | Emit diagnostic information (HTTP requests, API responses, cache status) to stderr; redact it before sharing. |
| `-V`, `--version` | — | Print the binary version. |

## Automation contract

- Structured data is written to stdout; progress and diagnostics are written to stderr.
- JSON and other non-interactive execution never consumes a reset card or deletes a profile without an explicit opt-in flag.
- `launch` treats a known Codex subcommand (`exec`, `resume`, …) or a non-launch flag as the start of Codex argv, even without `--`. Tokens on both sides of `--` are kept, so `launch work exec -- --json` still runs `exec`. A prompt that looks like an alias still needs `--`. When `alias` names a custom provider, Codex is started with `-c` overrides (including a generated model catalog, after `exec` / `resume` / … so Codex 0.149 applies them; user flags that preceded the subcommand move with them) and the key in the child environment. The child uses a per-launch Codex home (prompts/skills/`AGENTS.md` linked to the user home); `auth.json` is not swapped. `--json launch` captures Codex stdout/stderr into the JSON envelope instead of mixing them onto stdout.
- A manual `use` affects the next Codex process and accepts ChatGPT profile aliases only. Restart an already-running Codex process to load the new `auth.json`.
- Update checks are manual except for the one check performed when the TUI starts.

Examples:

```bash
codex-switch --json list
codex-switch --json use work
codex-switch launch work -- exec --json "review this"
codex-switch launch work exec -- --json "review this"
codex-switch launch exec --json "do the thing"
codex-switch launch work -- --model gpt-5.4
codex-switch provider add openrouter --base-url https://openrouter.ai/api/v1 --model openai/gpt-5.3-codex
codex-switch provider add zai --base-url https://api.example/v1 --fetch-models
codex-switch provider fetch-models zai
codex-switch launch openrouter -- -s workspace-write -a never
codex-switch provider probe AI-KR
codex-switch provider probe AI-KR --model deepseek-v4-flash
codex-switch self-update --check
```

## Provider

`provider add` required flags are `--base-url` and either `--fetch-models` or at least one `--model`. `--model` is repeatable; the first is `default_model`. `--fetch-models` GETs `{base_url}/models` and saves chat slugs (embedding/reranker omitted; more than 48 chat models must be picked with `--model`, or with TUI `f`). `--reasoning EFFORT` and `--no-web-search` attach to the most recent `--model`. Optional `--env-key` defaults to `CODEX_SWITCH_<ALIAS>_KEY`; `--wire-api` defaults to `responses` (the only protocol current Codex accepts). `--set KEY=VALUE` (repeatable) saves a provider-level `codex -c` override. All per-model and `--set` values are passed to Codex verbatim (only the `KEY=VALUE` shape is checked for `--set`). `--api-key-stdin` is required when there is no interactive terminal. `provider fetch-models <alias>` replaces the saved list from the gateway; matching ids keep their settings. On a large catalog pass `--model` (repeatable). `provider probe <alias>` POSTs `{base_url}/responses` with only `model` (no `input`) so a supporting handler 400s at validation without generating tokens; `--model` probes one saved slug. `provider rename <old> <new>` moves the directory and re-derives `provider_id` / `env_key`. `launch <alias> --model <id>` (before `--`) selects a saved model on a provider. `launch <alias> -- --model <id>` forwards Codex's own `--model` and drops the competing per-model `-c` pairs (`model`, `model_reasoning_effort`, `web_search`). Provider `launch` refuses a slug whose probe is unsupported.

The alias must not collide with a ChatGPT profile, another provider, or Codex's reserved ids `openai`, `ollama`, and `lmstudio`. Removal is immediate and is not archived under `deleted-profiles/`.

See [Custom API providers](Providers) for OpenRouter, DeepSeek-via-gateway, storage, and the no-argv key contract.

## TUI shortcuts

Four tabs: **Accounts**, **Providers**, **Settings**, and **Logs**. `Tab` / `Shift+Tab` cycles them. `q` and `h` are global.

### Accounts tab

`Enter` opens the scrollable detail and action menu for the selected account; if accounts are marked, it opens the batch menu instead.

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Navigate |
| `Tab` | Next tab (Providers) |
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
| `o` | Launch Codex with the selected account (also `o` in the account menu) |
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
| `Enter` / `o` | Launch Codex: pick a saved model, reasoning, and optional extra argv |
| `e` | Edit the selected provider |
| `n` | Rename the selected provider |
| `d` | Remove the selected provider (confirmation required) |
| `Tab` | Next tab (Settings) |
| `h` | Show help |
| `q` | Quit |

The Providers table never renders the stored key. `Enter` or `o` picks a saved model (and optionally changes reasoning or extra Codex argv for this session) then launches, or run `codex-switch launch <alias>` from the shell. `e` opens the edit form (including env key, wire API, and extra `-c`). `l` is re-login on the Accounts tab, not launch.

### Settings tab

Edits `$CODEX_SWITCH_HOME/config.toml`. Saving rewrites the file (comments and unknown keys are not kept). Daemon poll/token/cache intervals need a restart; `auto_warmup`, `warmup_times`, and `timezone` are re-read about once a minute.

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Move field (`j`/`k` inside `warmup_times` move among slots) |
| `Enter` / `Space` | Edit the focused value, or toggle a boolean |
| `←` / `→` | Cycle `log_level` or booleans |
| `+` / `a` | Add warmup slots (at most 10). One `HH:MM`, or paste `08:00, 13:10, 18:20`. After add, focus stays on `+ add time`. |
| `d` / `-` | Remove the selected warmup slot |
| `s` | Save `config.toml` |
| `Esc` | Cancel the current field edit (does not discard other unsaved fields) |
| `Tab` | Next tab (Logs). Ignored while a field is being edited. Unsaved edits are kept. |
| `h` | Show help |
| `q` | Quit |

Destructive or consumptive actions always require confirmation.

### Logs tab

Session diagnostics stay inside the TUI instead of writing through the active terminal screen. Use `j` / `k` or `PgUp` / `PgDn` to scroll and `End` to return to the latest line. `Tab` continues to Accounts.

## Next steps

- See how these commands combine into workflows in the [Feature guide](Feature-Guide).
- Custom API endpoints, OpenRouter, and key handling: [Custom API providers](Providers).
- Adjust defaults, proxy, and daemon behavior in [Configuration](Configuration).
- Check update channels and flags in [Updating](Updating).
