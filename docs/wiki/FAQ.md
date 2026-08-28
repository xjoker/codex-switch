# FAQ

## Does codex-switch support keyring-backed Codex credentials?

No, and this is permanent by design. OS keyrings provide no locking or atomic-replace semantics, and Codex's keyring entry format is an undocumented internal layout that has already changed between versions and platforms. Codex must use `cli_auth_credentials_store = "file"`; if it previously used a keyring store, switch the setting and log in again. See [why only the file store is supported](Configuration#why-only-the-file-store-is-supported).

## Does switching affect an already-running Codex session?

No. Codex reads authentication at startup. Restart Codex, or use `codex-switch launch` for a new process.

## Where is account data stored?

Saved profiles and application state default to `~/.codex-switch`; the live Codex file defaults to `~/.codex/auth.json`. Custom API providers live under `~/.codex-switch/providers/<alias>/` (`provider.toml`, generated `models.json`, isolated `codex-home/`). Provider `launch` copies MCP, `AGENTS.md`, prompts, and skills from `$CODEX_HOME` into that isolated home; it does not delete the originals. `CODEX_SWITCH_HOME` and `CODEX_HOME` relocate them independently.

## Can I point Codex at DeepSeek (or another Chat Completions API) directly?

No. Current Codex only accepts `wire_api = "responses"`. DeepSeek's official API is Chat Completions. Save an OpenRouter (or other Responses-capable gateway) provider and set `--model` to that gateway's slug. See [Custom API providers](Providers).

## Does `codex-switch use` switch a custom API provider?

No. `use` only stages a ChatGPT `auth.json` for the next Codex process. A provider is applied only by `codex-switch launch <alias>`, for that one Codex invocation. A later bare `codex` run is unchanged.

## Is a custom provider's API key put on the command line?

No. `provider add` reads it from a hidden prompt or `--api-key-stdin`. Launch injects it into the Codex child environment under a codex-switch-owned variable. `list`, `show`, JSON, and the TUI print a redacted form only.

## Is profile deletion permanent?

No. Inactive profiles are archived under `deleted-profiles/`. The active profile cannot be deleted.

## Is the daemon required?

No. It is an optional Beta feature. `codex-switch use`, `list`, `launch`, and the TUI work without it.

## What do the version numbers mean?

Releases use calendar versions in the form `YYYYMMDD.N.0`. Rolling dev builds end in `-dev`. Calendar versions are a normal upgrade from the earlier `0.0.x` series; see [Updating](Updating).

## How do I test the next release?

Use the rolling dev channel only when you are prepared to test prerelease behavior. Follow [Testing development releases](Development-Releases) for installation, verification, rollback, and issue-reporting steps.

## Are release binaries independently signed?

Not currently. Archives are checked against SHA256 files from the same GitHub Release, which detects corruption but shares the Release trust domain.

## Where should documentation fixes go?

These Wiki pages are generated from [`docs/wiki/` on the `dev` branch](https://github.com/xjoker/codex-switch/tree/dev/docs/wiki). Open a pull request against those sources; do not edit the published Wiki directly.

## Next steps

- New installation: [Getting started](Getting-Started).
- Daily workflows: [Feature guide](Feature-Guide).
- Custom API providers: [Custom API providers](Providers).
- Errors and recovery: [Troubleshooting](Troubleshooting).
