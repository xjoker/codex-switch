# Provider capability review, 2026-10-03

## Decision

Providers can run useful Codex coding sessions, MCP tools and subagents through a Responses gateway. They do not currently establish or expose complete feature compatibility. Keep the Beta designation. Treat them as launch/configuration management, with inference and tool execution owned by Codex and protocol translation owned by the gateway.

The reported startup warning is fixed on `dev` in `f20414a`: provider launches explicitly select `--no-daemon` when the resolved executable supports it. Explicit server choices remain unchanged. This selects the mode already required by native profiles; it does not change the remote gateway or weaken sandbox policy.

## Scope and evidence

- Repository base: `d1238fe`, package version `20261001.5.0`, branch `dev`.
- Reviewed boundaries: account/auth policy and rotation, launch/process lifecycle, provider transport/catalog/session ownership, CLI/TUI workflows, usage/retry/selection, updater/installers and CI contracts. This is a repository-wide automated check plus focused source review, not proof of every possible runtime path.
- Local full gate passed on Windows: `cargo test --all`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `cargo audit`, Bash installer syntax, PowerShell installer parsing and `git diff --check`. The launch integration suite contains 43 passing tests, including the new fail-before/pass-after provider routing regression. Linux/macOS jobs were not run in this review.
- Real engine for the new smoke: official Codex `0.159.3`, tag commit `01fc69f4026735edfdf6789820549727a4867b11`. The app task's default PATH resolved an older bundled engine, so the smoke selected the downloaded release explicitly; no installed executable or user configuration was replaced.
- CLIProxyAPI source reference: `2044a01f422998de79a5da8015141b878886534d`. The user's deployed gateway version, configuration and upstream accounts are unknown. No real provider key or paid model was used.

The repeatable [capability smoke](../../scripts/ci/provider-capability-smoke.py) starts a loopback Responses server and a read-only stdio MCP fixture, then launches the real Codex through codex-switch with temporary homes and a fake key. The server emits deterministic calls. This verifies plumbing and results, not model intelligence, gateway translation, concurrent production load or tool-selection quality.

| Scenario | Observed result |
|---|---|
| Generic `/models` list | MCP echo completes; V1 spawn and wait return the child's result |
| Native Codex catalog | Responses Lite carries the tool definitions; MCP echo and V2 spawn/wait complete |
| Native catalog, different child model | Parent uses `audit-model`; child uses `audit-child-model`; child result reaches the parent |
| Isolation and routing in all successful runs | Parent/child requests use the mock endpoint and expected bearer; base config stays byte-identical; no `auth.json` is created |
| Tool exposure | Generic catalog exposes V1 collaboration; native catalog exposes V2 collaboration and `apply_patch` |

Exposure is weaker evidence than execution: this smoke executes MCP and spawn/wait, not shell, patch, image, web-search, Code Mode, follow-up, interrupt or remote connector tools. An initial fixture attempt correctly failed MCP approval in noninteractive mode; the successful fixture explicitly approves only its known read-only echo server and disables app/plugin auto-sync. No production permission settings were changed.

Example invocation after building codex-switch, using a separately installed official engine:

```powershell
python scripts/ci/provider-capability-smoke.py --codex C:/path/to/codex.exe --switch target/debug/codex-switch.exe --output target/provider-audit/native --catalog native --child-model
```

Omit `--catalog native --child-model` to exercise generic metadata and inherited model selection. Output includes request captures, command logs and a summary. All credentials in these fixtures are synthetic. The script fails if MCP, child result return, model routing or isolation assertions fail.

## Findings and remediation

Remediation on `dev` after this review: the two P2 reproductions below are now fixed for the stated paths. Inspection redacts sensitive overrides and saved literal HTTP headers travel through child-only environment references. JSON inventory reports partial failures with nonzero status. `provider diagnose` and CLI/TUI catalog displays now expose local provenance/capabilities and validate child-model membership. Other arbitrary `--set` secrets remain the caller's responsibility; use environment references. The original evidence below is retained as the basis for the regressions, not as a description of current output.

The real 0.159.3 generic and native/different-child-model smokes passed again after these changes, including private-header delivery on both parent and child requests, offline diagnostic behavior and recorded catalog provenance. Real deployed gateway translation and the unexecuted tool categories remain unverified.

### P2: arbitrary extra configuration is not covered by key redaction

`src/commands/provider.rs::list/show` serializes `codex_config` verbatim; human `show` also prints each entry. `ProviderProfile::codex_config_args_with` passes those same values in argv. A legitimate gateway integration using a literal `model_providers.<id>.http_headers.X-Api-Key` therefore exposes that credential in inspection output and the process command line. The dedicated `api_key` remains correctly redacted and injected through an environment variable.

Isolated reproduction: save `X-Api-Key="audit-header-secret"`; both JSON `show` and `list` contain that exact synthetic value, while `audit-primary-secret` does not appear. This is not a disclosure of the user's real keys. The security promise must distinguish the dedicated key from arbitrary `--set` values. Prefer `env_http_headers` for secrets now; follow-up should add structured redaction and avoid secret-valued argv while retaining configuration precedence.

### P2: JSON provider listing silently hides damaged profiles

`src/commands/provider.rs::list` uses `filter_map(|alias| provider::load(alias).ok())` in JSON mode. A malformed `providers/broken/provider.toml` disappears from the result, produces no error and exits zero. The same test retains the valid provider. Automation cannot distinguish complete inventory from a partial read. Human listing and the TUI have better error visibility.

Follow-up: return valid entries plus explicit per-alias failures, with a documented partial-result status. Do not erase valid rows or expose raw secret-bearing TOML diagnostics.

### Capability diagnostics are incomplete

`provider probe` submits only a model field. HTTP validation or success can establish a reachable Responses route, not usable SSE completion, function/custom tools, images, compaction, Lite items, WebSockets or Agent workflows. `doctor` checks executable versions, not gateway capability or agent model reachability. Neither should be presented as a complete compatibility certification.

The provider picker also lacks catalog provenance, age and capability display. Users cannot readily see that a manually entered model is using generated metadata, why V2 or patch tools are absent, or whether a configured child model is outside the saved catalog.

## Capability assessment

| Capability | Current assessment and boundary |
|---|---|
| Responses HTTP/SSE | Working in the real-engine mock; gateway-specific translation still requires live testing |
| Native model metadata | Preserved, including unknown extension fields; native V2/Lite metadata exercised successfully |
| Generic model lists | Useful baseline; generated metadata does not promise image, freeform patch, Lite, Code Mode or V2 support |
| Local MCP | Configuration remains shared; read-only stdio discovery/call/result round trip tested |
| Skills, AGENTS.md, hooks, local plugins | Shared home/project lookup remains available; existing tests preserve resource files. Execution, trust and plugin-specific dependencies remain Codex responsibilities |
| Same-provider subagents | V1 and V2 spawn/wait tested; both inherited and explicitly selected saved child models reach the expected gateway |
| Cross-provider subagents | No codex-switch routing UI/contract. In 0.159.3, child config inherits the provider; the typed custom-role override does not include `model_provider` |
| Mixed vendors behind CLIProxyAPI | Can fit the same-provider model: use distinct saved model IDs routed by one gateway. Translator/model capability must match the Codex protocol |
| Multi-Agent V2 follow-up/message/interrupt | Exposed in native smoke, not executed; not certified by spawn/wait alone |
| `apply_patch`, shell and images | Metadata/tool availability is conditional; patch exposed in native smoke. No image understanding or sandboxed shell/patch execution claim |
| Web search | Depends on gateway/model tool support and launch settings; a working MCP search tool is a separate integration |
| Apps/cloud connectors | Additional account, OAuth, workspace and product permissions may be required; a provider API key does not grant them |
| WebSockets, realtime/audio, remote control | No dedicated end-to-end coverage here; explicit settings are not proof the gateway implements the relevant protocols |
| Provider resume | Identity, model, locks and native profile retention have integration coverage; subagents are excluded from default interactive history selection |
| Provider quotas/failover | Not implemented by codex-switch; ChatGPT quota scoring and account auto-selection do not apply to Providers |

Codex 0.159.3's [child configuration](https://github.com/openai/codex/blob/rust-v0.159.3/codex-rs/core/src/agent/child_config.rs) carries forward the provider and validates explicit model overrides against its available catalog. Its [role override](https://github.com/openai/codex/blob/rust-v0.159.3/codex-rs/core/src/agent/role.rs) is narrower than a general provider router. Register intended child models on the provider; do not assume a ChatGPT-oriented default agent model exists on a third-party endpoint. Generic models with empty reasoning capability lists can also reject explicit child effort selections even when the gateway might accept them.

Native `model_messages`, `multi_agent_version`, `use_responses_lite`, modalities and patch metadata affect the tool surface. The retained `supports_parallel_tool_calls` catalog field is absent from the 0.159.3 `ModelInfo` schema; do not interpret that old field alone as controlling current subagent concurrency. Parallel tool calls and concurrently running agents are separate concepts. See the pinned [model schema](https://github.com/openai/codex/blob/rust-v0.159.3/codex-rs/protocol/src/openai_models.rs) and [tool registration](https://github.com/openai/codex/blob/rust-v0.159.3/codex-rs/core/src/tools/spec_plan.rs).

Saved `no_web_search` is applied to the model chosen at launch. Codex's in-session `/model` operation is not a codex-switch relaunch: there is no watcher that reapplies another saved model's launch flags. Likewise, `--proxy`/`CS_PROXY` configure this application's HTTP client; the launcher does not translate that setting into Codex HTTP proxy environment variables. Configure Codex's own network environment for inference when a proxy is required.

## CLIProxyAPI guidance

This project's model fetch already includes `client_version`. At the pinned CLIProxyAPI revision, the [models handler](https://github.com/router-for-me/CLIProxyAPI/blob/2044a01f422998de79a5da8015141b878886534d/sdk/api/handlers/openai/openai_handlers.go) uses that query parameter to return the native Codex catalog. For an existing provider, run `codex-switch provider fetch-models <alias>` and select the models needed by both parent and children. A profile created only with `--model` has fallback metadata until explicitly synchronized.

Set the provider base URL to the remote gateway's `/v1` root. `--remote` addresses a Codex app-server, not an HTTP inference gateway. Do not copy the gateway's complete OAuth-mode example into `--set`: codex-switch uses a dedicated `env_key`, so a literal `experimental_bearer_token` conflicts with its saved-key mode. Native profile/catalog loading already supplies local model discovery; it does not require replacing shared `auth.json`.

The pinned CLIProxyAPI [configuration example](https://github.com/router-for-me/CLIProxyAPI/blob/2044a01f422998de79a5da8015141b878886534d/config.example.yaml) exposes `client.codex.optimize-multi-agent-v2`, `client.codex.enable-apply-patch` and session-affinity controls. These are server-version-specific options, not universally correct toggles. V2 optimization affects catalog and message translation; apply-patch advertisement depends on backend support. Affinity can improve cache reuse while concentrating work on one upstream credential. Confirm the deployed version before changing names or enabling capabilities.

## Development priorities

1. Close the two reproduced inspection/diagnostic defects; narrow the blanket key-redaction wording.
2. Add provider capability/provenance display and a diagnostic that checks saved/default child model reachability, effective connection and config conflicts without making a paid request.
3. Offer an explicitly invoked real-gateway smoke for SSE, one function tool, one MCP tool and one child result. Record each capability separately instead of a single supported boolean. Never run billable probes automatically.
4. Keep a pinned real-Codex compatibility job alongside fake-argv tests. Exercise generic/native catalogs, Lite, child model overrides and resume; add shell/patch, tool search, Code Mode, image and WebSocket cases with suitable fixtures.
5. Split the large provider module along transport, catalog and session ownership boundaries before adding more protocols. Preserve existing locks, secret handling, native profiles and legacy resume behavior.

The existing design is extensible through saved overrides and native metadata preservation. Its weak points are discoverability, diagnostics and the breadth of end-to-end evidence. A compatibility layer with model-specific evidence would improve usability more than automatically turning every feature on.
