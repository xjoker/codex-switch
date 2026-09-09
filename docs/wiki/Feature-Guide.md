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

The TUI has four tabs: **Accounts** (ChatGPT OAuth, quota, scoring), **Providers** (custom API endpoints), **Settings** (`config.toml`), and **Logs** (bounded `INFO` session diagnostics). `Tab` / `Shift+Tab` cycles them. Mouse users can click tabs and Accounts/Providers rows, double-click a row to open its account or provider launch menu, use the wheel in Logs, Help, and account menus, and click outside a dismissible popup to close it; modal forms and active edits do not click through. `q` and `h` apply on the main view; forms, text edits, menus, and confirmations keep their own `Esc` and confirmation rules. `o` launches Codex on Accounts and Providers. On Accounts, `u` (when no accounts are marked) switches the selected account and shows `Switching to …` progress, `t` toggles session-only auto-refresh, and the account menu's `w` performs a one-time warmup. Settings uses `j`/`k` for fields and `s` to save; `s` on Accounts still cycles sort. Unsaved Settings edits survive leaving the tab and require confirmation before quitting; while a field is being edited, `Tab` stays on Settings.

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

In the TUI Accounts page or account menu, `o` opens the same launch picker used by providers. `(Codex default)` adds no model or reasoning override; cached account models can supply a one-shot model and reasoning effort, and the picker also accepts extra Codex arguments.

```bash
codex-switch launch work -- --model gpt-5.4
codex-switch launch work -- exec --json "review this"
codex-switch launch -- exec --json "do the thing"
codex-switch launch -- -s workspace-write -a never
```

Arguments after `--` are Codex's, not codex-switch's. A known Codex subcommand (`exec`, `resume`, …) can start the argv without `--` (`codex-switch launch exec --json "…"`). Tokens on both sides of `--` are kept. The separator is still required when the Codex argv starts with a prompt that looks like an alias, or a flag that also exists on codex-switch (`--json`, `--color`, `--model`) immediately after the alias. Current Codex has no `--full-auto`; use `-a never`, `--sandbox`, or `--dangerously-bypass-approvals-and-sandbox`. `--json launch` prints one JSON object after Codex exits and captures at most 1 MiB from each Codex output stream; the corresponding `*_truncated` fields report clipping.

The launch lock serializes overlapping launch sessions. The restore delay is configurable (`launch.restore_delay_secs`) because Codex does not expose an authentication-read handshake.

## Launch Codex with a custom API provider

A provider profile is a third-party API endpoint plus a bearer key, stored under `$CODEX_SWITCH_HOME/providers/` rather than as a ChatGPT `auth.json`. Typical case: OpenRouter.

```bash
codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-5.3-codex
codex-switch launch openrouter
```

`launch <provider>` does not swap `$CODEX_HOME/auth.json`. It starts Codex with `-c` overrides that define and select the provider (and `launch --model` to pick a saved model), injects the key into the child environment only, and uses a persistent per-run Codex home under `provider-runs/<identity_id>/<run_id>` (prompts/skills/`AGENTS.md` linked to the user home; MCP merged back on exit). Concurrent launches of the same or different providers therefore keep independent sqlite/config state. Provider `resume` resolves IDs, unique names, `--last`, and the bare picker only inside the selected provider identity; it passes the exact session ID to Codex and applies Codex cwd/visibility filters. Auto-select (`launch` with no alias) and `use` remain ChatGPT-only. Launch performs no gateway request. `codex-switch provider probe <alias>` explicitly checks `POST /responses` without starting Codex, saves conclusive verdicts, and later launches refuse a model saved as unsupported.

A provider holds several models; reasoning effort and `web_search` are per model. In the TUI Providers tab, `Enter` / `o` opens a picker for a saved model, a one-shot reasoning override, and optional extra Codex argv; `e` edits the provider (including env key, wire API, and extra `-c`). The API key is read from a hidden prompt (or `--api-key-stdin`), never from argv. Full workflow, DeepSeek-via-OpenRouter, model-specific settings, TUI add/edit/rename, and the security contract are in [Custom API providers](Providers).

## Recover exhausted accounts

When the whole candidate pool is exhausted, an interactive `use` or `launch` can offer to consume the earliest-expiring reset card. Automation must opt in explicitly:

```bash
codex-switch use --consume-card
codex-switch reset-card work --yes
```

JSON or non-interactive execution never consumes a card without the explicit flag.

## Warm quota windows

Fresh paid accounts show no reset timer until their first real request. `warmup` sends a one-time minimal request to activate inactive 5h main and model-specific quota windows discovered from the official model response:

```bash
codex-switch warmup
codex-switch warmup work
```

Model names are discovered at runtime rather than maintained as a hardcoded compatibility list. Accounts with only a 7-day window (free plans), already-active 5h pools, and unavailable pools are skipped. In the TUI, use the selected account menu's `w` for a one-time warmup; the batch menu's `w` warms marked accounts that have a 5h window. The old `W` automatic-warmup shortcut and daemon schedule are removed. Press `t` on the Accounts page to toggle session-only automatic usage refresh.

## Optional OS scheduling

The binary no longer installs or owns a resident daemon, service, timer, or automatic account switcher. If periodic work is useful, install a user-level OS task yourself and invoke the one-time CLI operations. `list --force --json` refreshes usage, `warmup` activates quota windows, `use` without an alias selects and switches to the best eligible ChatGPT profile, and `launch` without an alias starts Codex with the best profile. Schedule `use` only when unattended credential changes are intended; a manual `use` affects the next Codex process, and an already-running Codex must be restarted.

The examples below schedule a one-time `warmup` every 30 minutes. These tasks run as your user; declare `CODEX_HOME` or `CODEX_SWITCH_HOME` in the task environment when you rely on non-default paths. Use `list --force --json` in the same task shape when you need a usage refresh instead.

**Windows Task Scheduler (PowerShell):**

```powershell
$codex = Join-Path $env:LOCALAPPDATA 'Programs\codex-switch\codex-switch.exe'
$task = '"{0}" warmup' -f $codex
schtasks.exe /Create /TN 'codex-switch-warmup' /SC MINUTE /MO 30 /TR $task /F
```

**Linux cron:** edit `crontab -e` and add:

```cron
*/30 * * * * /home/you/.local/bin/codex-switch warmup
```

**Linux systemd user timer:** create `~/.config/systemd/user/codex-switch-warmup.service`:

```ini
[Unit]
Description=Warm quota windows with codex-switch

[Service]
Type=oneshot
ExecStart=%h/.local/bin/codex-switch warmup
```

Create `~/.config/systemd/user/codex-switch-warmup.timer`:

```ini
[Unit]
Description=Run codex-switch warmup

[Timer]
OnBootSec=5min
OnUnitActiveSec=30min

[Install]
WantedBy=timers.target
```

Enable it with `systemctl --user daemon-reload` and `systemctl --user enable --now codex-switch-warmup.timer`.

**macOS launchd:** save this as `~/Library/LaunchAgents/com.example.codex-switch-warmup.plist`, replacing the executable path:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.example.codex-switch-warmup</string>
  <key>ProgramArguments</key><array>
    <string>/Users/you/.local/bin/codex-switch</string>
    <string>warmup</string>
  </array>
  <key>StartInterval</key><integer>1800</integer>
</dict></plist>
```

Load it with `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.example.codex-switch-warmup.plist`; remove it with `launchctl bootout gui/$(id -u)/com.example.codex-switch-warmup`. Syntax references: [Microsoft `schtasks`](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/schtasks-create), [systemd timers](https://www.freedesktop.org/software/systemd/man/latest/systemd.timer.html), [cron](https://man7.org/linux/man-pages/man5/crontab.5.html), and [Apple `launchd`](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html).

## Update the binary

Direct installs support the stable and rolling development channels and verify release checksums before replacing the binary. See [Updating](Updating) for channels, Homebrew rules, and legacy-install migration, and [Testing development releases](Development-Releases) for the dev channel.

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
- Tune paths, proxy, cache, selection, and launch behavior in [Configuration](Configuration).
- Something failed? Start with [Troubleshooting](Troubleshooting).
