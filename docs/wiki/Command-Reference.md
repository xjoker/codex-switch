# Command reference

The installed binary remains authoritative: use `codex-switch --help` and `codex-switch <command> --help` for the exact flags and examples supported by your version.

## Commands

| Command | Purpose |
|---|---|
| `login [--device] [alias]` | Add or reauthorize a profile through browser PKCE or device-code login. If the alias already exists, it is reauthorized; otherwise a new profile is created. |
| `import <path> [alias]` | Validate and import one `auth.json`, or recursively scan a directory for JSON files. The alias applies to single-file imports only; directories auto-assign aliases. An account that is already saved (same file, or same `account_id` and email) is skipped instead of duplicated, so its single-use refresh token is not spent. |
| `doctor [--desktop-codex <path>]` | Check the Codex executable resolved from `PATH` and, optionally, a desktop app's explicitly supplied bundled engine. Prints compatibility details; exits nonzero if the PATH CLI or supplied desktop engine is missing, unknown, or below the supported minimum. |
| `list [-f]` | Show profiles, usage, and availability; `-f` / `--force` bypasses the cache. |
| `use [alias] [--consume-card]` | Switch explicitly, or omit the alias to auto-select with the unified scoring algorithm. When the pool is exhausted, `--consume-card` consumes the earliest-expiring reset card to revive an account (auto-select only; ignored when an alias is given). |
| `launch [alias] [--consume-card] [--model <id>] [-- <codex-args>]` | Requires a PATH Codex CLI at or above the supported minimum `0.159.2`, checked before account selection, reset-card consumption, provider run creation, or credential staging. This is the project's support baseline, not a statement that older Codex releases cannot work independently. Start Codex with the best (or specified) ChatGPT profile's auth, or with a custom API provider when `alias` names one. A ChatGPT launch adds `--no-daemon` when `codex --help` lists it (see [Automation contract](#automation-contract)); for a ChatGPT profile, `--model` is forwarded to Codex as `--model`. Provider runs retain the default `CODEX_HOME` and use a separate native Codex profile for each run (`--profile cs-*`); `resume` resolves only that provider's sessions and passes an exact session ID. `--last` follows Codex cwd/visibility filters, and bare `resume` opens a provider-scoped picker. For a provider, `--model` before `--` selects a saved model; after `--` it is Codex's own `--model`. A known Codex subcommand (`exec`, `resume`, …) can start the argv without `--`. Tokens on both sides of `--` are kept. Auto-select (no alias) is ChatGPT-only. |
| `provider add <alias> --base-url <URL> (--model <id> \| --fetch-models)` | Save a custom API provider. HTTPS is required unless `--allow-insecure-http` is explicitly passed; the URL is validated before the API key is requested. `--model` is repeatable; the first is the default. `--fetch-models` imports chat slugs from `GET {base_url}/models` (embedding/reranker omitted; catalogs larger than 48 must use `--model` or TUI `f`). The API key is read from a hidden prompt, or from stdin with `--api-key-stdin` — never from argv. |
| `provider list` | List saved providers (no keys). |
| `provider show <alias>` | Show one provider; the key is redacted. |
| `provider fetch-models <alias> [--model <id>]` | Replace saved models with chat slugs from the provider's `GET /models`. Matching ids keep reasoning / `web_search`. Large catalogs require `--model`. |
| `provider probe <alias> [--model <id>]` | `POST {base_url}/responses` with only `model` (no `input`) to see if Codex can use the slug. Does not generate tokens. Default: every saved model. Results are kept 7 days (scoped to the model, credential and effective connection settings); `launch` re-checks a saved unsupported result live before refusing. |
| `provider rename <old> <new>` | Rename a provider (directory + derived ids). |
| `provider remove <alias> [-y]` | Delete a provider and its stored key; `-y` / `--yes` skips the prompt. Non-interactive and `--json` runs require `--yes`. |
| `reset-card <alias> [-y]` | Consume the earliest-expiring reset card for a profile after confirmation; `-y` / `--yes` skips the prompt. |
| `warmup [alias]` | Send a minimal request to activate the 5h quota-window countdown for one or all profiles. Accounts with only a 7-day window are skipped. |
| `rename <old> <new>` | Rename a saved profile. |
| `delete <alias> [-y]` | Move an inactive profile into recoverable deleted storage; `-y` / `--yes` skips the prompt. |
| `self-update [--check] [--dev\|--stable] [--version <VERSION>]` | Check or update a direct installation. Without flags it stays on the current channel; `--version` installs a specific newer stable version and conflicts with the channel flags. |
| `tui` | Open the interactive terminal dashboard. |
| `open` | Open the codex-switch data directory in the platform file manager. |

## Global options

| Option | Environment variable | Behavior |
|---|---|---|
| `--json` | — | Compact structured output (supported by `list`, `use`, `launch`, `warmup`, `reset-card`, `rename`, `delete`, `login`, `import`, `self-update`, `doctor`, `provider add`, `provider list`, `provider show`, `provider rename`, `provider remove`, `provider fetch-models`, `provider probe`). `doctor --json` reports `ok`, `minimum_version`, `aligned_version`, required `runtime_note`, `path_cli`, `desktop_codex`, and (when both versions parse) `versions_match` plus `version_relation`; each executable report has `executable`, `version`, `status`, and optional `note`. `launch --json` prints one envelope after Codex exits; each captured Codex stream is limited to 1 MiB and has a `*_truncated` flag. |
| `--json-pretty` | — | Indented structured output. |
| `--proxy <URL>` | `CS_PROXY` | Override proxy configuration for this process; supports `http(s)://`, `socks4://`, `socks5://`, and `socks5h://` (remote DNS). |
| `--color <auto\|always\|never>` | `CS_COLOR` | Control CLI terminal color. `NO_COLOR` disables CLI color regardless of this option. The TUI still paints its designed palette. |
| `--debug` | — | Emit diagnostic information (HTTP status, retry, cache status) to stderr. Review it before sharing. |
| `-V`, `--version` | — | Print the binary version. |

## Automation contract

- Structured data is written to stdout; progress and diagnostics are written to stderr.
- JSON and other non-interactive execution never consumes a reset card or deletes a profile without an explicit opt-in flag.
- Usage output labels `credits_balance` as credits, not dollars. The JSON field remains the raw numeric balance for compatibility; do not infer a universal USD conversion because credit pricing depends on model, speed, plan, and agreement. See [Codex pricing](https://learn.chatgpt.com/docs/pricing).
- `launch` treats a known Codex subcommand (`exec`, `resume`, …) or a non-launch flag as the start of Codex argv, even without `--`. Tokens on both sides of `--` are kept, so `launch work exec -- --json` still runs `exec`. A prompt that looks like an alias still needs `--`. When `alias` names a custom provider, Codex is started with `-c` overrides (including the saved model catalog, after `exec` / `resume` / … so Codex 0.149 applies them; user flags that preceded the subcommand move with them), `--profile` selecting the run's native `cs-*.config.toml` in the normal `$CODEX_HOME`, and the key in the child environment; `auth.json` is not swapped and the user's `config.toml` is not rewritten. For a ChatGPT profile, `--no-daemon` is prepended (before any subcommand) when `codex --help` lists it, so the session reads the staged `auth.json` rather than the shared app-server daemon. The help probe is bounded to 10 seconds; if it fails, times out, or returns no usable help, launch refuses before staging any credentials because the routing is unknown. An argv that already contains `--no-daemon`, `--remote`, or the daemon-only `agents` command is passed through unchanged. `--json launch` captures Codex stdout/stderr into the JSON envelope instead of mixing them onto stdout.
- A manual `use` affects the next Codex process and accepts ChatGPT profile aliases only. It updates `auth.json` and resets existing `model_provider` selections in the user `config.toml` and its default inline profile to `openai`, preserving models, provider definitions, MCP settings, and comments. When the live `auth.json` changes (compared as canonical JSON, so formatting-only rewrites do not count) and the Codex app-server daemon (Codex 0.157+) is running, `use` (explicit or auto-selecting), the TUI `u` switch, and a `login` that activates credentials restart it with `codex app-server daemon restart` and report the outcome as a diagnostic; each daemon call is bounded to 15 seconds, and a failed or timed-out restart is a warning, not a failed switch. Set `[use] restart_app_server = false` (or toggle it in TUI Settings) to leave the daemon alone; the outcome then names the manual command. Restart an already-running `codex exec` or `codex --no-daemon` process yourself; `use` without an alias selects the best eligible profile once. Launch flags, project configuration, separately selected Codex profiles, and `openai_base_url` / `OPENAI_BASE_URL` overrides remain unchanged.
- Update checks are manual except for the one check performed when the TUI starts.
- `list --force` and `warmup` are one-time operations. The project does not install or manage an OS scheduler; see the [Feature guide](Feature-Guide#optional-os-scheduling) for user-managed examples.

Examples:

```bash
codex-switch --json list
codex-switch doctor
codex-switch doctor --desktop-codex <path-to-desktop-codex-engine>
codex-switch --json doctor --desktop-codex <path-to-desktop-codex-engine>
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

The supported minimum and current alignment baseline are both Codex 0.159.2. `doctor` checks the `codex` executable resolved from `PATH`; it does not search for desktop installations. Pass `--desktop-codex <path>` to check a specific bundled engine as a separate executable. Omitting that option reports `not_checked` and does not fail. Status values are `not_checked`, `not_found`, `unknown`, `below_minimum`, `aligned`, and `above_baseline_unverified`. If both versions meet the minimum but differ by core version or prerelease, `versions_match` is `false` and the command succeeds with a note; build metadata alone does not make versions different. `version_relation` is `same` when their version precedence matches, `desktop_engine_newer` when the selected desktop engine is newer than the PATH CLI, or `desktop_engine_older` when it is older. A version newer than the alignment baseline is accepted but not fully verified. The version probe is bounded to 4 seconds and runs with a temporary `CODEX_HOME` so it does not initialize the user's Codex home. A PATH CLI or explicitly supplied engine that cannot be found, returns no usable version, times out, exits unsuccessfully, or is below minimum makes the report fail; JSON still prints one report before returning nonzero. `runtime_note` explains that matching versions do not guarantee matching app capabilities or daemon behavior. `doctor` checks executable versions only; it does not validate authentication, managed/system policy, desktop UI behavior, or daemon compatibility.

If `launch` or the TUI reports that the PATH CLI is below the 0.159.2 minimum, upgrade that CLI using the method you originally used to install it. For an npm-managed CLI, run `npm install -g @openai/codex@latest` in the same Node environment; with fnm, select the same fnm Node version/environment used for the installation. Then restart the terminal and TUI, and verify `where.exe codex`, `codex --version`, and `codex-switch doctor`. If `doctor --desktop-codex <path>` reports that the bundled desktop engine is below minimum, update the Codex desktop app through its own updater or installer and check the new bundled engine path. Updating the npm CLI does not update the desktop engine.

`launch` applies the same minimum-version check to the resolved PATH executable for ChatGPT and custom-provider launches. It runs before account selection, consuming a reset card, creating a provider run, or staging credentials. The existing bounded `codex --help` probe remains in place to determine `--no-daemon` routing for ChatGPT launches.

## Provider

`provider add` required flags are `--base-url` and either `--fetch-models` or at least one `--model`. `--model` is repeatable; the first is `default_model`. `--fetch-models` GETs `{base_url}/models` and saves chat slugs plus launch catalog metadata (embedding/reranker omitted; more than 48 chat models must be picked with `--model`, or with TUI `f`). `--reasoning EFFORT` and `--no-web-search` attach to the most recent `--model`. Optional `--env-key` defaults to `CODEX_SWITCH_<ALIAS>_KEY`; `--wire-api` defaults to `responses` (the only protocol current Codex accepts). `--set KEY=VALUE` (repeatable) saves a provider-level `codex -c` override. `--metadata-fallback URL|PATH|none` picks the catalog metadata fallback used after the gateway `/models` call (default: the public OpenRouter list). `--allow-insecure-http` opts that provider into plain HTTP. All per-model and `--set` values are passed to Codex verbatim (only the `KEY=VALUE` shape is checked for `--set`). `--api-key-stdin` is required when there is no interactive terminal. `provider fetch-models <alias>` replaces the saved list from the gateway and persists its launch catalog; matching ids keep their settings. On a large catalog pass `--model` (repeatable). `provider probe <alias>` POSTs `{base_url}/responses` with only `model` (no `input`) so a supporting handler 400s at validation without generating tokens; `--model` probes one saved slug and conclusive verdicts are saved for 7 days (fingerprint-scoped: any endpoint, key, header, query or catalog change drops them). `provider rename <old> <new>` moves the directory and re-derives `provider_id` / `env_key`. `launch <alias> --model <id>` (before `--`) selects a saved model on a provider. `launch <alias> -- --model <id>` forwards Codex's own `--model` and drops the competing per-model `-c` pairs (`model`, `model_reasoning_effort`, `web_search`). Provider `launch` never refuses on a saved "unsupported" result alone: it first sends one live probe. A confirming answer refuses the launch; a "supported" answer proceeds and refreshes the record; an inconclusive answer or a failed request proceeds with a warning on stderr and drops the stale record. Saved "supported" or missing results add no request.

The alias must not collide with a ChatGPT profile, another provider, or Codex's reserved ids `openai`, `ollama`, and `lmstudio`. Removal is immediate and is not archived under `deleted-profiles/`.

See [Custom API providers](Providers) for OpenRouter, DeepSeek-via-gateway, storage, and the no-argv key contract.

## TUI shortcuts

Four tabs: **Accounts**, **Providers**, **Settings**, and **Logs**. `Tab` / `Shift+Tab` cycles them. `q` and `h` are main-view shortcuts; forms, text edits, menus, and confirmations use their own `Esc` and confirmation rules.

Mouse input is available alongside the keyboard: click a tab to switch pages, click an Accounts or Providers row to select it, and double-click the selected row to open its account menu or provider launch menu. On Settings, click a field to edit or toggle it and use the wheel to move among fields; clicking another field while editing commits the current value first. Provider add/edit forms, launch pickers, and confirmation prompts accept clicks on their own controls (fields, checkboxes, model rows, `y`/`n`) and do not click through to the page behind them. Use the wheel to scroll Logs, Help, menus, and modal lists. Clicking outside a dismissible popup closes it.

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
| `i` | Toggle the compact quota panel on the main view |
| `s` | Cycle sort order (name / quota / status) |
| `Space` | Mark or unmark an account |
| `u` (Accounts page when no accounts are marked, or account menu) | Switch to the selected account; the status line shows `Switching to …` while it is in progress |
| `o` | Open the selected account's launch picker (also `o` in the account menu): use Codex's default, or choose a cached model, one-shot reasoning, and extra Codex arguments |
| `c` (account menu) | Confirm and consume the earliest-expiring reset card |
| `r` (account menu) | Refresh usage and the account's authenticated model catalog |
| `w` (account menu) | Warm up the selected account |
| `l` (account menu) | Re-login the selected account |
| `n` (account menu) | Rename the selected account |
| `d` (account menu) | Delete the selected account (confirmation required) |
| `r` / `w` / `l` / `d` (batch menu) | Refresh, warm up, re-login, or delete the marked accounts |
| `h` | Show the complete shortcut list (main view) |
| `Esc` | Clear filter/marks or close the current popup |
| `q` | Quit (main view) |

The ChatGPT model list comes from the selected profile's authenticated `/models` response, using the Codex CLI version detected on `PATH`; it is account- and route-specific and is cached in the TUI for five minutes. Use `Enter` then `r` in the account menu to refresh that account's usage and model catalog. The app does not add models that the service did not return. Model request errors report the `client_version` used.

The detected PATH Codex version supplies the `client_version` query parameter and User-Agent value for shared HTTP requests; the desktop application's bundled engine does not determine that HTTP client version. If no usable PATH version can be detected, HTTP requests use the 0.159.2 alignment baseline as a transport fallback; `doctor` still reports the actual executable as missing or unknown, and `launch` enforces the real PATH minimum. A persistent TUI status-bar warning identifies a PATH CLI below the 0.159.2 project support minimum. A newer desktop engine does not change which CLI the terminal resolves.

Usage refresh is gated by a parseable access-token expiry: it proactively refreshes when that token is within five minutes of expiry. If the access-token expiry is unavailable, the quota request is tried first and an HTTP 401/403 can enter the existing recovery flow. An expired ID token by itself does not mean the access token is expired and does not block an otherwise valid quota request. If refresh reports `refresh_token_invalidated` for a profile, stop retrying refresh for that profile; use `codex-switch login <alias>` to reauthenticate it, or `codex-switch use <alias>` to switch to another saved account.

Usage and refresh diagnostics record request phase start and completion, including elapsed time, outcome, and HTTP status when available. They do not log credential values or response bodies.

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
| `h` | Show help (main view) |
| `q` | Quit (main view) |

The Providers table never renders the stored key. `Enter` or `o` picks a saved model (and optionally changes reasoning or extra Codex argv for this session) then launches, or run `codex-switch launch <alias>` from the shell. `e` opens the edit form (including env key, wire API, and extra `-c`). `l` is re-login on the Accounts tab, not launch.

### Settings tab

Edits `$CODEX_SWITCH_HOME/config.toml`. Saving rewrites the file (comments and unknown keys are not kept). The TUI process applies changes immediately. `t` on the Accounts page controls session-only automatic usage refresh; quota warmup remains a one-time account-menu action (`w`) or CLI `warmup`.

Fields are labelled with their `config.toml` keys: `proxy.url`, `proxy.no_proxy`, `cache.ttl`, `network.max_concurrent`, `tui.auto_refresh_interval_secs`, `use.safety_margin_7d`, `use.team_priority`, `use.restart_app_server`, and `launch.restore_delay_secs`. The focused field shows a short explanation under the list (units, minimums, and for `use.restart_app_server` what the restart does).

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Move among fields |
| `Enter` / `Space` | Edit the focused value, or toggle a boolean. Clicking a field does the same. |
| `←` / `→` | Toggle the boolean field |
| `s` | Save `config.toml` |
| `Esc` | Cancel the current field edit (does not discard other unsaved fields) |
| `Tab` | Next tab (Logs). Ignored while a field is being edited. Unsaved edits are kept. |
| `h` | Show help (main view) |
| `q` | Quit (main view) |

Destructive or consumptive actions always require confirmation.

### Logs tab

Session diagnostics stay inside the TUI instead of writing through the active terminal screen. It shows `INFO` and above, including completed or failed account operations; `DEBUG` requires `--debug` or `RUST_LOG`. Use `j` / `k` or `PgUp` / `PgDn` to scroll and `End` to return to the latest line. `Tab` continues to Accounts.

## Next steps

- See how these commands combine into workflows in the [Feature guide](Feature-Guide).
- Custom API endpoints, OpenRouter, and key handling: [Custom API providers](Providers).
- Adjust defaults, proxy, cache, selection, and launch behavior in [Configuration](Configuration).
- Check update channels and flags in [Updating](Updating).

Provider launches inherit native MCP credentials, skills, agents, plugins, hooks, and project configuration from the normal Codex environment. Provider keys are injected into the child environment, never saved in the generated profile. Settings saved inside Codex belong to that run's profile; default `config.toml` is not swapped or restored. Forwarding another `--profile`/`-p` is rejected. Legacy isolated-home sessions remain resumable in their original environment.

For provider `--json` launches, child output is captured in persistent run files, read and removed on normal completion. A launcher crash leaves those files available and does not close the child's output destination. Intentional shutdown still terminates the child; closing a terminal or killing an entire process tree is distinct from a launcher-only crash.
