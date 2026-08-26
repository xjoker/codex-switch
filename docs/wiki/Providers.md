# Custom API providers

A custom API provider is a saved third-party endpoint that `codex-switch launch` can hand to Codex CLI for one session. Typical case: OpenRouter, or another gateway that speaks Codex's Responses protocol.

Unlike a ChatGPT account profile, a provider has no `auth.json` and no quota dashboard. It stores one endpoint plus a bearer API key under `$CODEX_SWITCH_HOME`, and a list of models. Each model can carry its own reasoning effort and `web_search` setting. At launch those become `codex -c …` overrides. Nothing is written to `~/.codex`.

The alias is the only name. Codex's required `model_providers.<id>.name` is set to the alias.

> Never put an API key on the command line. `provider add` reads it from a hidden prompt, or from stdin with `--api-key-stdin`. The key is stored mode `0600` and never printed, listed, or placed in argv.

## Add a provider

```bash
codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-5.3-codex \
  --model deepseek/deepseek-r1-0528 --reasoning high
```

The first `--model` is the default. `--reasoning` and `--no-web-search` attach to the most recent `--model`. The command then prompts for the API key without echoing it. For scripts, pass the key on stdin instead:

```bash
printf '%s' "$OPENROUTER_API_KEY" | codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-5.3-codex \
  --api-key-stdin
```

Optional flags:

| Flag | Default | Purpose |
|---|---|---|
| `--model ID` | required, repeatable | Model id (OpenRouter: full slug). First is `default_model` |
| `--reasoning EFFORT` | none | Attach `model_reasoning_effort` to the most recent `--model` |
| `--no-web-search` | off | Attach `web_search=disabled` to the most recent `--model` |
| `--env-key` | `CODEX_SWITCH_<ALIAS>_KEY` | Environment variable Codex reads the key from at launch |
| `--wire-api` | `responses` | Codex wire protocol; current Codex only accepts `responses` |
| `--set KEY=VALUE` | none | Extra provider-level `codex -c` override (repeatable) |
| `--api-key-stdin` | off | Read the key from stdin instead of a hidden prompt |

`--set` is for overrides that are not per-model. Values are passed to Codex verbatim — Codex, not codex-switch, decides which keys and values are valid — so only the `KEY=VALUE` shape is checked.

The alias follows the same rules as a ChatGPT profile (ASCII letters, digits, `_`, `-`, `.`; at most 64 characters) and must not collide with an existing profile, an existing provider, or Codex's reserved ids `openai`, `ollama`, and `lmstudio`.

Inspect, rename, and remove:

```bash
codex-switch provider list
codex-switch provider show openrouter
codex-switch provider rename openrouter orouter
codex-switch provider remove openrouter
```

`show` prints a redacted key (`…` plus the last four characters). Rename moves the on-disk directory and re-derives `provider_id` / `env_key` from the new alias. Removal deletes the stored key immediately; unlike ChatGPT profile deletion, it is not archived under `deleted-profiles/`. Non-interactive and `--json` runs require `--yes`.

`--json` is supported on `provider add`, `list`, `show`, `rename`, and `remove`. JSON never includes the raw key.

Older single-`model` files still load: the model becomes the only `[[models]]` entry, and a provider-level `model_reasoning_effort` / `web_search=disabled` is moved onto that model.

## Launch Codex with a provider

Name the provider alias. Auto-select (`launch` with no alias) stays ChatGPT-only.

```bash
codex-switch launch openrouter
codex-switch launch openrouter --model deepseek/deepseek-r1-0528
codex-switch launch openrouter -- --full-auto
```

`launch` does **not** replace `$CODEX_HOME/auth.json`. It starts `codex` with `-c` overrides that define and select the provider and the chosen model (or `default_model`), and injects the API key into the child process environment under `env_key`. Extra arguments after `--` are appended as Codex CLI flags. `--model` must name a model saved on that provider.

Because `-c` layers on top of `$CODEX_HOME/config.toml`, MCP servers, skills, and other Codex settings in that file stay in effect for the session.

`codex-switch use` does not accept a provider alias. A provider is applied only for the launched Codex process; a later bare `codex` invocation is unchanged.

## OpenRouter and DeepSeek

OpenRouter is the intended first provider: its `/api/v1` base URL plus a full model slug (including the vendor prefix) is what Codex expects. One OpenRouter provider can hold every slug that shares that key:

```bash
codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-5.3-codex \
  --model deepseek/deepseek-chat \
  --model deepseek/deepseek-r1-0528 --reasoning medium
```

Codex currently speaks only `wire_api = "responses"`. DeepSeek's official API is Chat Completions, so pointing `--base-url` at DeepSeek directly will not work. Route DeepSeek (or any other Chat Completions-only vendor) through OpenRouter or another Responses-capable gateway.

Pick the slug from the gateway's catalog. If Codex rejects the model, the usual cause is a Chat Completions-only endpoint rather than a missing key.

## Model-specific request settings

Codex always sends the same Responses request shape (including its built-in `web_search` server tool). Whether a given model accepts it depends on the model, not on luck — the behavior is consistent per model, not intermittent. Those settings are stored on the model, not on the whole provider.

### web_search server tool

Codex enables its built-in `web_search` server tool by default. Some models accept or ignore it (verified: `deepseek/deepseek-v3.2`, `moonshotai/kimi-k2`, `minimax/minimax-m3:free` all return HTTP 200), while others reject it (verified: `openai/gpt-oss-20b` returns HTTP 400 `Server tool request failed`). Disable it on that model:

```bash
codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-oss-20b --no-web-search
```

Or per launch: `codex-switch launch openrouter -- -c web_search=disabled`.

### Reasoning ("thinking") models

Codex defaults an unknown model to `reasoning effort: none`, which reads as reasoning disabled. Thinking models reject that with HTTP 400 `Reasoning is mandatory for this endpoint`. Set the effort on that model (verified with `deepseek/deepseek-r1-0528` and `moonshotai/kimi-k2-thinking`):

```bash
codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-5.3-codex \
  --model deepseek/deepseek-r1-0528 --reasoning medium
```

Effort values (`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`; Codex also accepts `ultra`) come from the Codex version in use, so codex-switch does not restrict them on the CLI. The TUI form offers the common presets and `(skip)`. Plain chat models need no reasoning flag.

## TUI

`codex-switch tui` has two tabs: **Accounts** (ChatGPT OAuth, quota, scoring) and **Providers** (alias, models, base URL). Switch with `Tab` / `Shift+Tab`.

On the Providers tab:

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Navigate |
| `a` | Add a provider (form dialog) |
| `Enter` / `o` | Launch: pick a saved model and reasoning for this session |
| `e` | Edit the selected provider |
| `n` | Rename |
| `d` | Remove (confirmation required) |
| `Tab` | Return to Accounts |
| `h` | Help |
| `q` | Quit |

Add and edit use the same form. Add starts typing the alias immediately; Enter commits a field and continues to the next. Tab moves between alias, base URL, API key, and Models; `j`/`k` move inside the model list. The last row is `+ add model` — Enter (or `+` / `a`) adds a model and starts typing its id; `d` / `-` / Delete ask for confirmation (`y` removes, `n` / Esc keeps it). `←` / `→` cycle reasoning, `w` toggles web_search, `*` marks the default, `s` saves, Esc cancels. Edit starts on Base URL in navigation mode (Enter edits the focused cell). The API key is masked. On edit, an empty key keeps the stored one. Alias is the only name; rename is `n` on the list, not a second field.

The stored key is never rendered in the table. `o` launches Codex on both tabs: Accounts starts the selected ChatGPT profile immediately; Providers opens a picker for a saved model, then Enter (or `o` again) starts Codex. On the Providers list, Enter also opens that picker; `e` edits. `←`/`→` in the picker change reasoning for this session only (the saved profile is unchanged). `l` is re-login on Accounts, never launch. Codex runs in the foreground; the TUI resumes when it exits.

## Storage and security

| Location | Purpose |
|---|---|
| `$CODEX_SWITCH_HOME/providers/<alias>/provider.toml` | Provider definition and API key (directory `0700`, file `0600`) |

Defaults to `~/.codex-switch/providers/`. Relocate the whole tree with `CODEX_SWITCH_HOME`; this still does not change where Codex reads `auth.json`.

Security contract:

- The key is never a CLI argument, so it does not appear in the process table as argv.
- At launch it exists only in the Codex child environment, under a codex-switch-owned variable (`CODEX_SWITCH_<ALIAS>_KEY` by default) rather than a vendor's conventional name, so a pre-exported `OPENAI_API_KEY` or `OPENROUTER_API_KEY` is not reused by accident.
- `list`, `show`, JSON output, and the TUI print a redacted form only.
- Launch writes nothing under `$CODEX_HOME`. ChatGPT `use` / `launch` locking and `auth.json` backup/restore do not apply.

Do not commit `provider.toml`, paste keys into issues, or share unredacted `--debug` output.

## What this does not do

- Persist a provider for a subsequent bare `codex` run (`use` remains ChatGPT-only).
- Auto-select among providers, score them, or show quota / credits.
- Talk Chat Completions, or wrap a local proxy.
- Share an alias with a ChatGPT profile.

## Next steps

- Command flags and JSON shapes: [Command reference](Command-Reference#provider).
- ChatGPT account, quota, and `use` workflows: [Feature guide](Feature-Guide).
- Paths and `CODEX_SWITCH_HOME`: [Configuration](Configuration).
- Module and storage layout: [Architecture overview](Architecture-Overview).
