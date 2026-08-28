# Troubleshooting

Start with the complete error message, its file path, and the command that produced it. Configuration, login, update, and permission failures include the path or next command when recovery is known.

| Symptom | Action |
|---|---|
| No saved profiles | Run `codex-switch login` or `codex-switch import <path>`. |
| `Profile '<alias>' not found` on `use` or ChatGPT `launch` | That alias is not a ChatGPT profile. Custom API providers are launched with `codex-switch launch <alias>` and listed by `codex-switch provider list`; `use` does not accept them. |
| Codex rejects a custom provider / Chat Completions error | Current Codex only speaks `wire_api = "responses"`. Point `--base-url` at a Responses-capable gateway (OpenRouter) rather than a Chat Completions-only vendor API. See [Custom API providers](Providers). |
| `/mcp` empty (or custom prompts missing) after provider `launch` | Older builds used an empty isolated `CODEX_HOME`, or copied prompts into a fork that never wrote back. Current builds link `prompts/` / `skills/` / `AGENTS.md` to the user home and merge MCP on exit. Confirm `~/.codex/config.toml` still has `[mcp_servers]` and update with `self-update --dev`. |
| Some models 404 on `{base_url}/responses` | The gateway listed the slug (or Chat Completions works) but Codex's path is `/responses`. Probe with `codex-switch provider probe <alias> [--model <id>]` (no `input`, so it should not bill). Launch refuses an unsupported slug. |
| Custom provider `exec` hits `api.openai.com` 401 | Codex 0.149 ignores `-c` in front of `exec`. Use a build that places provider overrides after the subcommand (`codex exec -c …`). |
| Custom provider 404 on `{base_url}/responses` after skip | Codex 0.150 still sent a leftover thinking level from `config.toml` (or catalog `effort: none`). Use a build that omits `model_reasoning_effort` from the per-launch home and catalogs skip with no default. If it still 404s, that slug has no Responses channel; pick another. See [Custom API providers](Providers#reasoning-thinking-models). |
| Custom provider fails with `Server tool request failed` (HTTP 400) | The model rejects Codex's built-in `web_search` server tool. Disable it on that model in the TUI form (`w`) or with `--no-web-search` on `provider add`. See [Model-specific request settings](Providers#model-specific-request-settings). |
| Custom provider fails with `Reasoning is mandatory for this endpoint` (HTTP 400) | The model is a thinking model but Codex defaulted it to no reasoning. Set a reasoning effort on that model in the TUI form or with `--reasoning` after `--model`. See [Model-specific request settings](Providers#model-specific-request-settings). |
| `'<alias>' already names a ChatGPT profile` when adding a provider | Aliases are a single namespace. Choose a different provider alias. |
| A removed provider cannot be recovered | Provider deletion removes the stored key immediately; unlike ChatGPT profiles, it is not archived under `deleted-profiles/`. Re-add it with `provider add`. |
| Credential store is not file-backed | Set `cli_auth_credentials_store = "file"` in `$CODEX_HOME/config.toml`. |
| Headless login cannot open a browser | Run `codex-switch login --device`. |
| Windows daemon installation is denied | Open PowerShell as Administrator and retry. |
| Windows daemon stop says credential work is still in flight | Wait briefly and run `codex-switch daemon stop` again. The process is intentionally left running instead of force-killed while a refresh token may be rotating. |
| TUI layout is broken in Git Bash | Use Windows Terminal or PowerShell. |
| Direct update does not replace a Homebrew binary | Run `brew upgrade xjoker/tap/codex-switch`. |
| A Homebrew installation cannot switch to dev | Run `brew uninstall codex-switch`, then follow [Testing development releases](Development-Releases#install-the-rolling-dev-build). |
| A direct dev installation should return to Homebrew | Run the direct uninstaller, keep the data directory when prompted, then run `brew install xjoker/tap/codex-switch`. |
| macOS/Linux self-update reports that the install directory is not writable | Rerun the current installer once to migrate a legacy `/usr/local/bin` direct install to `$HOME/.local/bin`; see [Updating](Updating#legacy-direct-installs). Use `sudo codex-switch self-update` only for an intentional `--system` install. |
| A dev build should return to stable | Run `codex-switch self-update --stable`. |
| Self-update reports that `gh attestation verify` is unavailable | Install or upgrade [GitHub CLI](https://cli.github.com/), then retry. Direct self-update fails closed until it can verify the release provenance bundle. |
| An installed daemon ignores `CODEX_SWITCH_HOME` | `daemon install` captures `CODEX_SWITCH_HOME` from the shell that runs it; re-run `daemon install` with the variable set so its value lands in the service definition. See [Configuration](Configuration#platform-integration). |
| HTTPS fails with `invalid peer certificate: UnknownIssuer` | An intercepting proxy is re-signing traffic. See [HTTPS fails with an unknown issuer](#https-fails-with-invalid-peer-certificate-unknownissuer). |
| An account reports `re-login required (refresh_token_reused)` | The stored refresh token was already spent and cannot be recovered. Run `codex-switch login <alias>` for that profile. The verdict is remembered, so the account costs no further requests until you sign in again; `codex-switch list -f` asks the server anyway. |
| Import reports a quarantined rotated credential | The server replaced the source file's one-time token before identity or managed-policy validation failed. Keep the named file under `~/.codex-switch/recovery/` private, sign in again, then remove it only after the account works. Recovery files are deliberately not selectable profiles. |

For network or API failures, rerun the smallest failing command with `--debug`:

```bash
codex-switch --debug list
codex-switch --debug self-update --check
```

Debug output can contain account or infrastructure identifiers. Before opening an issue, remove tokens, email addresses, account IDs, workspace names, filesystem paths that reveal identity, and proxy credentials.

## HTTPS fails with `invalid peer certificate: UnknownIssuer`

An intercepting proxy — a debugging tool such as Proxyman or Charles, or a
corporate MITM gateway — presents its own certificate instead of the real one.
The browser and `curl` accept it because its CA is installed in the operating
system, so only `codex-switch` appears to be broken.

`codex-switch` reads the OS trust store, so installing the proxy's CA there is
normally enough. Reaching this error means the CA is missing from that store, or
is trusted only for the current user in a way the store does not expose. Point at
the certificate explicitly:

```bash
# macOS: export the CA, substituting the name shown in Keychain Access
security find-certificate -c "Proxyman CA" -p > ~/.codex-switch/proxy-ca.pem
export CODEX_CA_CERTIFICATE=~/.codex-switch/proxy-ca.pem
```

Set the variable in the shell profile so the TUI and the daemon inherit it, not
just the current shell. `SSL_CERT_FILE` works as a fallback in the same order
Codex itself uses. Turning off interception is equally valid when a capture is
not needed.

The failure is intermittent when the proxy only intercepts part of the time, so
the same command can succeed minutes later. Login is affected the same way: the
browser step completes while the token exchange behind it fails, which looks like
a rejected sign-in rather than a certificate problem.

## Recover a deleted profile

Deletion moves an inactive profile into recoverable storage rather than erasing it. Stop the daemon, move the newest matching directory back into `profiles/`, and confirm that it appears:

```bash
codex-switch daemon stop
# Move deleted-profiles/<alias>.backup-<timestamp> to profiles/<alias>
codex-switch list
```

The base directory is `~/.codex-switch`, `%USERPROFILE%\.codex-switch` on Windows, or the value of `CODEX_SWITCH_HOME`.

## Reset-card outcome is uncertain

If a reset-card request reports that consumption may have occurred, do not immediately retry. Refresh the account state and verify the card count and quota first. This warning means the request reached the service but the client could not prove the final result.

## Report an issue

Include the operating system, terminal, `codex-switch --version`, exact command, expected behavior, actual behavior, and redacted diagnostic output. Use the [GitHub issue tracker](https://github.com/xjoker/codex-switch/issues).

## Next steps

- Check short behavior and security answers in the [FAQ](FAQ).
- Review paths and settings in [Configuration](Configuration).
- Custom API provider launch and key handling: [Custom API providers](Providers).
- If the problem remains, report the redacted reproduction in the [GitHub issue tracker](https://github.com/xjoker/codex-switch/issues).
