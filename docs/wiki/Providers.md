# Custom API providers

A custom API provider is a saved third-party endpoint that `codex-switch launch` can hand to Codex CLI for one session. Typical case: OpenRouter, or another gateway that speaks Codex's Responses protocol.

Unlike a ChatGPT account profile, a provider has no `auth.json` and no quota dashboard. It stores one endpoint plus a bearer API key under `$CODEX_SWITCH_HOME`, and a list of models. Each model can carry its own reasoning effort and `web_search` setting. At launch those become `codex -c …` overrides. The Codex process is given its own `CODEX_HOME` under `$CODEX_SWITCH_HOME/providers/<alias>/codex-home`. Catalog, sessions, sqlite, and project trust all stay in that tree. The user's `~/.codex` is not read or written.

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

A small gateway can fill the list itself (Z.ai-style `/models`, not OpenRouter's hundreds):

```bash
printf '%s' "$KEY" | codex-switch provider add zai \
  --base-url https://api.example/v1 \
  --fetch-models \
  --api-key-stdin

codex-switch provider fetch-models zai
```

`--fetch-models` on add can be combined with `--model`: those ids stay first (and keep `--reasoning` / `--no-web-search`), then other chat slugs from the gateway are appended. `provider fetch-models` replaces the saved list; matching ids keep their reasoning / `web_search` settings, and the default stays if the gateway still lists it. A slug on `/models` is not a guarantee that Codex's `POST /responses` accepts it.

Optional flags:

| Flag | Default | Purpose |
|---|---|---|
| `--model ID` | required unless `--fetch-models` | Model id (OpenRouter: full slug). First is `default_model` |
| `--fetch-models` | off | `GET {base_url}/models` and save chat slugs (embedding/reranker omitted). Catalogs larger than 48 models must be picked with `--model` |
| `--reasoning EFFORT` | none | Attach `model_reasoning_effort` to the most recent `--model` |
| `--no-web-search` | off | Attach `web_search=disabled` to the most recent `--model` |
| `--env-key` | `CODEX_SWITCH_<ALIAS>_KEY` | Environment variable Codex reads the key from at launch |
| `--wire-api` | `responses` | Codex wire protocol; current Codex only accepts `responses` |
| `--set KEY=VALUE` | none | Extra provider-level `codex -c` override (repeatable) |
| `--metadata-fallback URL\|PATH\|none` | public OpenRouter `/models` | Catalog metadata fallback after the gateway `/models` call |
| `--api-key-stdin` | off | Read the key from stdin instead of a hidden prompt |

`--set` is for overrides that are not per-model. Values are passed to Codex verbatim — Codex, not codex-switch, decides which keys and values are valid — so only the `KEY=VALUE` shape is checked.

The alias follows the same rules as a ChatGPT profile (ASCII letters, digits, `_`, `-`, `.`; at most 64 characters) and must not collide with an existing profile, an existing provider, or Codex's reserved ids `openai`, `ollama`, and `lmstudio`.

Inspect, rename, and remove:

```bash
codex-switch provider list
codex-switch provider show openrouter
codex-switch provider fetch-models openrouter
codex-switch provider rename openrouter orouter
codex-switch provider remove openrouter
```

`show` prints a redacted key (`…` plus the last four characters). Rename moves the on-disk directory and re-derives `provider_id` / `env_key` from the new alias. Removal deletes the stored key immediately; unlike ChatGPT profile deletion, it is not archived under `deleted-profiles/`. Non-interactive and `--json` runs require `--yes`.

`--json` is supported on `provider add`, `list`, `show`, `rename`, `remove`, and `fetch-models`. JSON never includes the raw key.

Older single-`model` files still load: the model becomes the only `[[models]]` entry, and a provider-level `model_reasoning_effort` / `web_search=disabled` is moved onto that model.

## Launch Codex with a provider

Name the provider alias. Auto-select (`launch` with no alias) stays ChatGPT-only.

```bash
codex-switch launch openrouter
codex-switch launch openrouter --model deepseek/deepseek-r1-0528
codex-switch launch openrouter -- exec --json "review this"
codex-switch launch openrouter -- -s workspace-write -a never
```

`launch` does **not** replace `$CODEX_HOME/auth.json` and does not read or write the user's `$CODEX_HOME`. It starts `codex` with `-c` overrides that define and select the provider and the chosen model (or `default_model`), injects the API key into the child process environment under `env_key`, points the child's `CODEX_HOME` at `$CODEX_SWITCH_HOME/providers/<alias>/codex-home`, and writes a Codex model catalog next to `provider.toml` so `/model` lists the **saved** provider slugs. Those `-c` flags are global Codex options, so they are placed **in front of** a Codex subcommand such as `exec`. Extra arguments after `--` are appended as Codex CLI flags. `--model` before `--` must name a model saved on that provider; `--model` / `-m` after `--` is forwarded to Codex and drops the competing per-model `-c` pairs (`model`, `model_reasoning_effort`, `web_search`).

Put `--` before any Codex argv that could be mistaken for a codex-switch alias or flag (`exec`, `--json`, `--color`, a prompt that looks like a name). `codex-switch launch -- exec --json "…"` auto-selects a ChatGPT profile; it cannot target a provider.

On first launch Codex creates its runtime files in that isolated home. Provider-specific Codex settings (`--no-web-search`, `--reasoning`, `--set`) are stored on the provider and passed as `-c`; they are not written into the user's `$CODEX_HOME/config.toml`.

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

The generated Codex catalog only advertises thinking levels when a model has a saved effort (or the launch picker sets one). Otherwise it lists `none` and sets `supports_reasoning_summaries` false, so Codex does not send `reasoning.effort`. Gateways that 404 on a reasoning field (Cursor-style `composer-2.5`, which exposes a `fast` parameter rather than effort) stay usable. Do not set `--reasoning` on those models.

## TUI

`codex-switch tui` has three tabs: **Accounts** (ChatGPT OAuth, quota, scoring), **Providers** (alias, models, base URL), and **Settings** (`config.toml`). Switch with `Tab` / `Shift+Tab`.

On the Providers tab:

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Navigate |
| `a` | Add a provider (form dialog) |
| `Enter` / `o` | Launch: pick a saved model and reasoning for this session |
| `e` | Edit the selected provider |
| `n` | Rename |
| `d` | Remove (confirmation required) |
| `Tab` | Next tab (Settings) |
| `h` | Help |
| `q` | Quit |

Add and edit use the same form. Add starts typing the alias immediately; Enter commits a field and continues Alias → Base URL → API key → Models (env key, wire API, and extra `-c` stay on their defaults). Tab visits every field, including those three. `j`/`k` move inside the model list. The last row is `+ add model` — Enter (or `+` / `=` / `a`) adds a model and starts typing its id; `f` GETs `{base_url}/models` and replaces the list with chat slugs (embedding/reranker omitted; catalogs larger than 48 must be typed). If a model id is being edited, Esc first. `d` / `-` / Delete ask for confirmation (`y` removes, `n` / Esc keeps it). A provider must keep at least one model, so the last model cannot be removed. `←` / `→` cycle reasoning, `w` toggles web_search, `*` marks the default, `s` saves, Esc cancels. Edit starts on Base URL in navigation mode (Enter edits the focused cell). The API key is masked. On edit, an empty key keeps the stored one. Alias is the only name; rename is `n` on the list, not a second field. Extra `-c` overrides are `KEY=VALUE` items; commas inside a value are kept.

The stored key is never rendered in the table. `o` launches Codex on both tabs: Accounts starts the selected ChatGPT profile immediately; Providers opens a picker for a saved model, optional extra Codex argv (Tab), and a one-shot reasoning override, then Enter (or `o`) starts Codex. On the Providers list, Enter also opens that picker; `e` edits (including env key, wire API, and extra `-c` overrides). `←`/`→` in the picker change reasoning for this session only (the saved profile is unchanged). `l` is re-login on Accounts, never launch. Codex runs in the foreground; the TUI resumes when it exits.

## Storage and security

| Location | Purpose |
|---|---|
| `$CODEX_SWITCH_HOME/providers/<alias>/provider.toml` | Provider definition and API key (directory `0700`, file `0600`) |
| `$CODEX_SWITCH_HOME/providers/<alias>/models.json` | Generated Codex model catalog used at launch (`/model` list plus metadata) |
| `$CODEX_SWITCH_HOME/providers/<alias>/codex-home/` | Isolated Codex runtime for that provider (`CODEX_HOME` for the launched process) |

Defaults to `~/.codex-switch/providers/`. Relocate the whole tree with `CODEX_SWITCH_HOME`; this still does not change where Codex reads `auth.json`.

On disk, `name` always equals the alias (Codex requires `model_providers.<id>.name`). `default_model` must name one of the `[[models]]` entries. Example:

```toml
provider_id = "openrouter"
name = "openrouter"
base_url = "https://openrouter.ai/api/v1"
env_key = "CODEX_SWITCH_OPENROUTER_KEY"
default_model = "openai/gpt-5.3-codex"
wire_api = "responses"

[[models]]
id = "openai/gpt-5.3-codex"

[[models]]
id = "deepseek/deepseek-r1-0528"
reasoning = "medium"
```

The `api_key` field is stored in the same file but never printed by `list`, `show`, JSON, or the TUI.

Security contract:

- The key is never a CLI argument, so it does not appear in the process table as argv.
- At launch it exists only in the Codex child environment, under a codex-switch-owned variable (`CODEX_SWITCH_<ALIAS>_KEY` by default) rather than a vendor's conventional name, so a pre-exported `OPENAI_API_KEY` or `OPENROUTER_API_KEY` is not reused by accident.
- `list`, `show`, JSON output, and the TUI print a redacted form only.
- Launch writes nothing under the user's `$CODEX_HOME`. Codex runtime files for that session go to `providers/<alias>/codex-home`. ChatGPT `use` / `launch` locking and `auth.json` backup/restore do not apply.

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
