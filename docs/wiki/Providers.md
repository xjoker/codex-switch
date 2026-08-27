# Custom API providers

A custom API provider is a saved third-party endpoint that `codex-switch launch` can hand to Codex CLI for one session. Typical case: OpenRouter, or another gateway that speaks Codex's Responses protocol.

Unlike a ChatGPT account profile, a provider has no `auth.json` and no quota dashboard. It stores a model-provider definition plus a bearer API key under `$CODEX_SWITCH_HOME`, then at launch injects that definition as `codex -c …` overrides. Nothing is written to `~/.codex`.

> Never put an API key on the command line. `provider add` reads it from a hidden prompt, or from stdin with `--api-key-stdin`. The key is stored mode `0600` and never printed, listed, or placed in argv.

## Add a provider

```bash
codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-5.3-codex
```

The command then prompts for the API key without echoing it. For scripts, pass the key on stdin instead:

```bash
printf '%s' "$OPENROUTER_API_KEY" | codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-5.3-codex \
  --api-key-stdin
```

Optional flags:

| Flag | Default | Purpose |
|---|---|---|
| `--name` | the alias | Human-readable name Codex shows |
| `--env-key` | `CODEX_SWITCH_<ALIAS>_KEY` | Environment variable Codex reads the key from at launch |
| `--wire-api` | `responses` | Codex wire protocol; current Codex only accepts `responses` |
| `--reasoning EFFORT` | none | Save `model_reasoning_effort=EFFORT` for thinking models (see below) |
| `--no-web-search` | off | Save `web_search=disabled` for models that reject the built-in tool |
| `--set KEY=VALUE` | none | Extra `codex -c` override saved with the provider and applied at launch (repeatable) |
| `--api-key-stdin` | off | Read the key from stdin instead of a hidden prompt |

These save `codex -c KEY=VALUE` overrides with the provider, so a model-specific Codex setting is applied on every launch without retyping it after `--` (see [Model-specific request settings](#model-specific-request-settings)). `--reasoning` and `--no-web-search` are convenience shortcuts for the two most common settings; `--set` (repeatable) covers any other override. Values are passed to Codex verbatim — Codex, not codex-switch, decides which keys and values are valid — so only the `KEY=VALUE` shape is checked. An explicit `--set` wins over a convenience flag for the same key.

The alias follows the same rules as a ChatGPT profile (ASCII letters, digits, `_`, `-`, `.`; at most 64 characters) and must not collide with an existing profile, an existing provider, or Codex's reserved ids `openai`, `ollama`, and `lmstudio`.

Inspect and remove:

```bash
codex-switch provider list
codex-switch provider show openrouter
codex-switch provider remove openrouter
```

`show` prints a redacted key (`…` plus the last four characters). Removal deletes the stored key immediately; unlike ChatGPT profile deletion, it is not archived under `deleted-profiles/`. Non-interactive and `--json` runs require `--yes`.

`--json` is supported on `provider add`, `list`, `show`, and `remove`. JSON never includes the raw key.

## Launch Codex with a provider

Name the provider alias. Auto-select (`launch` with no alias) stays ChatGPT-only.

```bash
codex-switch launch openrouter
codex-switch launch openrouter -- --full-auto
```

`launch` does **not** replace `$CODEX_HOME/auth.json`. It starts `codex` with `-c` overrides that define and select the provider, injects the API key into the child process environment under `env_key`, and writes a Codex model catalog under `$CODEX_SWITCH_HOME/providers/<alias>/models.json` so `/model` lists the provider's slugs and Codex does not fall back to generic metadata. Extra arguments after `--` are appended as Codex CLI flags.

Some models need extra Codex request settings (disabling `web_search`, or setting a reasoning effort for thinking models) — see [Model-specific request settings](#model-specific-request-settings).

Because `-c` layers on top of `$CODEX_HOME/config.toml`, MCP servers, skills, and other Codex settings in that file stay in effect for the session.

`codex-switch use` does not accept a provider alias. A provider is applied only for the launched Codex process; a later bare `codex` invocation is unchanged.

## OpenRouter and DeepSeek

OpenRouter is the intended first provider: its `/api/v1` base URL plus a full model slug (including the vendor prefix) is what Codex expects.

Codex currently speaks only `wire_api = "responses"`. DeepSeek's official API is Chat Completions, so pointing `--base-url` at DeepSeek directly will not work. Route DeepSeek (or any other Chat Completions-only vendor) through OpenRouter or another Responses-capable gateway, and set `--model` to that gateway's slug:

```bash
codex-switch provider add deepseek \
  --base-url https://openrouter.ai/api/v1 \
  --model deepseek/deepseek-chat \
  --name "DeepSeek via OpenRouter"
```

Pick the slug from the gateway's catalog. If Codex rejects the model, the usual cause is a Chat Completions-only endpoint rather than a missing key.

## Model-specific request settings

Codex always sends the same Responses request shape (including its built-in `web_search` server tool). Whether a given model accepts it depends on the model, not on luck — the behavior is consistent per model, not intermittent.

### Model metadata

Codex only ships metadata for its own model slugs. A custom id such as `glm-5.3-flash` otherwise produces:

```
warning: Model metadata for `glm-5.3-flash` not found. Defaulting to fallback metadata; this can degrade performance and cause issues.
```

`launch` generates a catalog and passes it as `model_catalog_json`. Codex `/model` reads that file as a **replacement** for the bundled OpenAI list (it does not merge). Each injected entry uses `visibility: list` so it appears in the picker.

At launch, `GET {base_url}/models` (Bearer key, 8s timeout) fills `context_window`, display name, description, and input modalities when the gateway returns them:

- A small list (at most 48 models, typical of a single vendor) is injected wholesale, with the provider's `--model` first.
- A large list (OpenRouter is hundreds) is **not** dumped into `/model`. Only `--model` and any extra `--models SLUG` values are listed; matching rows still receive the fetched metadata.
- `--set model_context_window=N` still wins for the default slug. Other slugs use the fetched window, or 1,048,576 when no source reports one.
- If the gateway `/models` call fails or a catalog slug has no `context_window`, launch fills those fields from a **metadata fallback** (no provider key is sent):
  - Default: public OpenRouter `GET https://openrouter.ai/api/v1/models` (no login).
  - Override per provider with `--metadata-fallback URL|PATH|none`, or globally with `CODEX_SWITCH_METADATA_FALLBACK` / `CODEX_SWITCH_OPENROUTER_MODELS_URL`.
  - Matching is exact id, then a unique `vendor/{slug}` (a `:variant` suffix such as `:free` is ignored). Two vendors with the same model name are not guessed. The catalog `slug` stays the provider's id.
  - OpenRouter's full list is never injected into `/model`. A fallback whose host is already the provider `base_url` is skipped. `none` disables the fallback.
- Fetch failure (401, timeout, unrecognized JSON, or fallback unreachable) does not block launch: remaining gaps use the generated defaults.
- An explicit `--set model_catalog_json=/path/to/models.json` is left alone — that file must then include every slug you want in `/model`.

```bash
codex-switch provider add zai \
  --base-url https://api.z.ai/api/v1 \
  --model glm-5.3-flash \
  --models glm-5.3 \
  --metadata-fallback none
```

Omit `--metadata-fallback` to use public OpenRouter. Pass an HTTP(S) URL or a local JSON path instead of `none` to point at your own list.

### web_search server tool

Codex enables its built-in `web_search` server tool by default. Some models accept or ignore it (verified: `deepseek/deepseek-v3.2`, `moonshotai/kimi-k2`, `minimax/minimax-m3:free` all return HTTP 200), while others reject it (verified: `openai/gpt-oss-20b` returns HTTP 400 `Server tool request failed`). If a model rejects it, turn it off at the top level of `$CODEX_HOME/config.toml`:

```toml
web_search = "disabled"
```

per launch, since `launch` passes everything after `--` to Codex:

```bash
codex-switch launch openrouter -- -c web_search=disabled
```

or saved once with the provider so every launch applies it:

```bash
codex-switch provider add openrouter \
  --base-url https://openrouter.ai/api/v1 \
  --model openai/gpt-oss-20b \
  --no-web-search
```

(`--no-web-search` is shorthand for `--set web_search=disabled`.)

### Reasoning ("thinking") models

Codex defaults an unknown model to `reasoning effort: none`, which reads as reasoning disabled. Thinking models reject that with HTTP 400 `Reasoning is mandatory for this endpoint`. Give Codex a reasoning effort (verified with `deepseek/deepseek-r1-0528` and `moonshotai/kimi-k2-thinking`):

```bash
codex-switch launch openrouter -- -c model_reasoning_effort=medium
```

or save it with the provider so it is always applied:

```bash
codex-switch provider add r1 \
  --base-url https://openrouter.ai/api/v1 \
  --model deepseek/deepseek-r1-0528 \
  --reasoning medium
```

(`--reasoning medium` is shorthand for `--set model_reasoning_effort=medium`.)

Effort values (`none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`; Codex also accepts `ultra`) come from the Codex version in use, so codex-switch does not restrict them — pass any value with `--reasoning` (or `--set`) and Codex reports if it is invalid. Plain chat models (`deepseek/deepseek-v3.2`, `moonshotai/kimi-k2`, `openai/gpt-4o-mini`) need no reasoning flag. Combine `--reasoning` and `--no-web-search` (or repeat `--set`) when a model needs both.

## TUI

`codex-switch tui` has two tabs: **Accounts** (ChatGPT OAuth, quota, scoring) and **Providers** (alias, name, model, base URL). Switch with `Tab` / `Shift+Tab`.

On the Providers tab:

| Key | Action |
|---|---|
| `j` / `k` or `↑` / `↓` | Navigate |
| `a` | Add a provider (alias → base URL → model → reasoning → web_search → API key) |
| `l` / `Enter` | Launch Codex with the selected provider |
| `d` | Remove the selected provider (confirmation required) |
| `Tab` | Return to Accounts |
| `h` | Help |
| `q` | Quit |

The reasoning step is a single choice (`←`/`→`, default `(skip)` saves nothing); the web_search step is a toggle (`Space`, default leaves it enabled). Both are saved into the provider's `codex_config`. The API-key step is masked (`*`). The stored key is never rendered in the table. The wizard does not set `--name`, `--env-key`, or `--wire-api`; those keep the CLI defaults (`name` = alias, derived `env_key`, `responses`). Use the CLI (`--set`) for any override other than reasoning and web_search.

On the Accounts tab, press `o` to launch the selected ChatGPT profile, or open the account menu with `Enter` and press `o` there. Codex runs in the foreground; the TUI resumes when Codex exits.

## Storage and security

| Location | Purpose |
|---|---|
| `$CODEX_SWITCH_HOME/providers/<alias>/provider.toml` | Provider definition and API key (directory `0700`, file `0600`) |
| `$CODEX_SWITCH_HOME/providers/<alias>/models.json` | Generated Codex model catalog used at launch (`/model` list plus metadata) |

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
