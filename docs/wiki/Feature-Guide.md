# Feature guide

`codex-switch` manages multiple file-backed Codex CLI logins, observes their quota state, and selects an account for the next Codex process.

> **Authentication prerequisite:** Codex must use the file credential store. Set `cli_auth_credentials_store = "file"` in `$CODEX_HOME/config.toml`. Explicit `keyring`, `auto`, and `ephemeral` stores are rejected because they can bypass the `auth.json` file that codex-switch switches.

## Manage accounts

Add accounts with browser or device-code login:

```bash
codex-switch login work
codex-switch login --device server
```

Existing `auth.json` files can be imported individually or from a directory. Imports are validated in stages — JSON format, required token structure with a decodable `id_token`, then a live usage-service check — before being saved under collision-free aliases:

```bash
codex-switch import ~/auth-backups
```

Interactive login deduplicates local profiles by `account_id` first and falls back to email when safe. Import is deliberately create-only and never updates an existing profile: Usage API validation proves that the bearer can access a workspace, but a Team workspace ID can be shared by several users and cannot authorize overwriting another saved credential. For the same reason, import will not write a *second* profile for an account you already have: when the incoming file is byte-identical to a saved profile, or carries the same `account_id` **and** email, the import is skipped before validation so its single-use refresh token is never spent. Use `login <alias>` to refresh an existing profile.

Profile deletion is recoverable. An inactive profile is moved under `deleted-profiles/` after confirmation; the active profile cannot be deleted. See [recovery instructions](Troubleshooting#recover-a-deleted-profile).

## External login detection

Interactive commands compare the live `$CODEX_HOME/auth.json` against saved profiles before doing their own work:

- A new account (for example after a plain `codex login`) triggers an offer to save it as a profile.
- A refreshed token for a known account triggers an offer to update that profile.
- Non-interactive runs (pipes, cron, CI) report the change but never modify state silently.

## Observe quota and account state

Use the CLI for scripts and quick inspection, or the TUI for an interactive dashboard:

```bash
codex-switch list
codex-switch --json list
codex-switch tui
```

The usage model includes the main 5-hour and 7-day windows, additional model-specific pools, reset cards, spend limits, account restrictions, and model capabilities returned by the authenticated service. Cached entries are scoped by profile alias and retain their own fetch time.

Normal reads refresh only stale entries. Use `list -f` or the TUI refresh action when a fresh network read is required.

The TUI has three tabs: **Accounts** (ChatGPT OAuth, quota, scoring), **Providers** (custom API endpoints), and **Settings** (`config.toml`). `Tab` / `Shift+Tab` cycles them. `o` launches Codex on Accounts and Providers. Account-only keys (`W`, mark, filter) stay on Accounts. Settings uses `j`/`k` for fields and `s` to save; `s` on Accounts still cycles sort. Unsaved Settings edits survive leaving the tab; while a field is being edited, `Tab` stays on Settings.

The TUI account detail page is a single scrollable column with identity and organization labels, token expiry times in the local timezone, every quota pool with a pace marker, available reset cards, and the models the account may use. Model names and reasoning-effort capabilities are discovered from the authenticated service at runtime, not hardcoded. The full shortcut list is in the [command reference](Command-Reference#tui-shortcuts) and under `h` inside the TUI.

## Select an account

Select an explicit profile:

```bash
codex-switch use work
```

Or let the adaptive selector rank all profiles:

```bash
codex-switch use
```

Selection has two phases:

1. **Eligibility** excludes candidates with exhausted 5h or 7d windows, critically low weekly headroom with a distant reset, or an unsafe Free-plan balance.
2. **Scoring** ranks the eligible candidates by tier preference (Team accounts get priority by default), pace-aware 5h headroom, weekly sustainability, quota that is close to resetting, and recent use.

If every account is ineligible, the best fallback is reported instead of pretending an account is healthy.

Switching replaces the live `$CODEX_HOME/auth.json` atomically while holding a process lock. Restart Codex after a manual switch because Codex reads the file at startup.

## Launch Codex with a profile

`launch` selects or stages a profile, starts Codex, then restores the previous live authentication after the configured compatibility delay:

```bash
codex-switch launch work -- --model gpt-5.4
codex-switch launch work -- exec --json "review this"
codex-switch launch -- exec --json "do the thing"
codex-switch launch -- -s workspace-write -a never
```

Arguments after `--` are Codex's, not codex-switch's. A known Codex subcommand (`exec`, `resume`, …) can start the argv without `--` (`codex-switch launch exec --json "…"`). Tokens on both sides of `--` are kept. The separator is still required when the Codex argv starts with a prompt that looks like an alias, or a flag that also exists on codex-switch (`--json`, `--color`, `--model`) immediately after the alias. Current Codex has no `--full-auto`; use `-a never`, `--sandbox`, or `--dangerously-bypass-approvals-and-sandbox`. `--json launch` prints one JSON object after Codex exits and captures Codex stdout/stderr into that object.

The launch lock serializes overlapping launch sessions. The restore delay is configurable (`launch.restore_delay_secs`) because Codex does not expose an authentication-read handshake.

## Launch Codex with a custom API provider

A provider profile is a third-party API endpoint plus a bearer key, stored under `$CODEX_SWITCH_HOME/providers/` rather than as a ChatGPT `auth.json`. Typical case: OpenRouter.

```bash
codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-5.3-codex
codex-switch launch openrouter
```

`launch <provider>` does not swap `$CODEX_HOME/auth.json` and does not write the user's `$CODEX_HOME`. It starts Codex with `-c` overrides that define and select the provider (and `launch --model` to pick a saved model), injects the key into the child environment only, points the child's `CODEX_HOME` at `$CODEX_SWITCH_HOME/providers/<alias>/codex-home`, and writes a model catalog so Codex `/model` lists the saved provider slugs. MCP servers, `AGENTS.md`, prompts, and skills are copied from the user home into that isolated tree at launch. Fill those slugs with `--model`, or import chat ids from the gateway (`--fetch-models` / `provider fetch-models` / TUI `f`). Catalogs larger than 48 chat models must be picked (`--model` or the TUI picker); they are not imported wholesale. Auto-select (`launch` with no alias) and `use` remain ChatGPT-only. Before spawn, provider `launch` probes `POST /responses` with only `model` (no `input`) and refuses a slug that 404s. `codex-switch provider probe <alias>` runs that check without starting Codex.

A provider holds several models; reasoning effort and `web_search` are per model. In the TUI Providers tab, `Enter` / `o` opens a picker for a saved model, a one-shot reasoning override, and optional extra Codex argv; `e` edits the provider (including env key, wire API, and extra `-c`). The API key is read from a hidden prompt (or `--api-key-stdin`), never from argv. Full workflow, DeepSeek-via-OpenRouter, model-specific settings, TUI add/edit/rename, and the security contract are in [Custom API providers](Providers).

## Recover exhausted accounts

When the whole candidate pool is exhausted, an interactive `use` or `launch` can offer to consume the earliest-expiring reset card. Automation must opt in explicitly:

```bash
codex-switch use --consume-card
codex-switch reset-card work --yes
```

JSON or non-interactive execution never consumes a card without the explicit flag.

## Warm quota windows

Fresh accounts show no reset timer until their first real request. `warmup` sends minimal requests to activate inactive main and model-specific quota windows discovered from the official model response:

```bash
codex-switch warmup
codex-switch warmup work
```

Model names are discovered at runtime rather than maintained as a hardcoded compatibility list. Already-active or unavailable pools are skipped. Inside the TUI, `W` toggles automatic warmup for accounts whose 5-hour window has expired; that session toggle is separate from `daemon.auto_warmup`. When `auto_warmup` is on and `warmup_times` is empty, the daemon warms during cache refresh. When slots are set, warmup runs only at those `HH:MM` times in `daemon.timezone` (empty = system local; see [Configuration](Configuration#timed-warmup)).

## Run the background daemon

The Beta daemon monitors the current profile, refreshes cached usage and expiring tokens, and prepares a better account when the configured threshold is reached.

```bash
codex-switch daemon install
codex-switch daemon status
```

Service integration is platform-native: LaunchAgent on macOS, a systemd user service on Linux, and Task Scheduler on Windows. Windows installation requires elevated PowerShell.

The daemon runs four independent timers: account polling (`poll_interval_secs`), full cache refresh (`cache_refresh_interval_secs`, with warmup only when `auto_warmup` is on and `warmup_times` is empty), scheduled warmup (~60s, when `auto_warmup` is on and `warmup_times` is set), and proactive token refresh (`token_check_interval_secs`). Scheduled slots use `daemon.timezone` when set, otherwise the process local timezone. A switch happens only when at least two profiles exist and the current profile's 5-hour usage reaches `switch_threshold`.

By default, a switch is deferred while an interactive Codex process (`codex`, `codex resume`, `codex exec`) is running; the daemon records the pending switch and retries on the next poll. Long-lived MCP or app-server processes do not block a switch. Operational state lives in `daemon-state.json` and is shown by `daemon status`. Daemon switches cannot ask for confirmation: an untracked live `auth.json` is replaced after the normal backup rotation, so save or import an account first if you want to keep it selectable.

## Update the binary

Direct installs support the stable and rolling development channels, verify release checksums before replacing the binary, and restart a running daemon around the update. See [Updating](Updating) for channels, Homebrew rules, and legacy-install migration, and [Testing development releases](Development-Releases) for the dev channel.

```bash
codex-switch self-update --check
codex-switch self-update
```

## Automate safely

Most non-interactive commands support `--json` or `--json-pretty`. Structured output stays on stdout; progress and diagnostic messages use stderr. Commands that can consume a reset card or delete a profile require explicit non-interactive confirmation.

Never publish profile files, `auth.json`, provider API keys, unredacted debug output, proxy credentials, account IDs, email addresses, or workspace names.

## Next steps

- Need an exact command, flag, or TUI shortcut? Open the [Command reference](Command-Reference).
- Launching Codex against OpenRouter or another custom API? Open [Custom API providers](Providers).
- Tune paths, proxy, daemon, and launch behavior in [Configuration](Configuration).
- Something failed? Start with [Troubleshooting](Troubleshooting).
