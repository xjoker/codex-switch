# Codex source alignment, 2026-09-30

The development candidate is based on project commit `be8ea732d35447a789741aa061aed320d05cb4de`, including the native-profile, daemon deadline, semantic-auth comparison, Windows ACL and dependency fixes already on `dev`.

## Upstream reference

- Released contract: [Codex rust-v0.159.2](https://github.com/openai/codex/releases/tag/rust-v0.159.2), commit `ff6aec96948b70d94983af2641a6b67c94faeff5`.
- Additional source cross-check: [main at 8c3612f](https://github.com/openai/codex/tree/8c3612fb638d356579c446de68421288164484dc), retrieved on 2026-09-30.
- Primary contracts: `codex-rs/protocol/src/openai_models.rs`, `models-manager/src/model_info.rs`, `model-provider/src/models_endpoint.rs`, `model-provider-info/src/lib.rs`, `codex-api/src/endpoint/models.rs`, and `backend-client/src/client/rate_limit_resets.rs`.

The version is an explicit reference point, not a promise that an arbitrary later Codex release has the same private backend contract. The ChatGPT Codex backend and a provider's generic OpenAI-compatible model list have different metadata shapes; a model being unavailable through the public API does not exclude it from a ChatGPT-authenticated catalog.

## Review and repair boundaries

| Boundary | Corrected behavior |
| --- | --- |
| Native model metadata | Preserve upstream instructions, tool support, reasoning levels, modality information and unknown extension fields. |
| Generic model lists | Fill the Codex schema without advertising capabilities that the source did not provide; keep explicit user choices distinct from discovered capability limits. |
| Provider HTTP connection | Resolve the same saved provider overrides used at launch, including explicit model catalogs, authentication headers and routing queries. |
| Responses probes | Treat temporary and ambiguous failures as unknown; scope and expire conclusive results instead of permanently trusting an old boolean. A saved denial is re-checked live at launch before it can refuse. |
| Account authentication | Retry models authentication in a bounded way and use compare-and-swap persistence for rotated credentials. |
| Account routing | Apply account and FedRAMP routing consistently across model, response, usage and reset-credit calls. |
| Session recovery | Scan first JSONL metadata records in active and archived rollouts; retain profiles whenever liveness cannot be established safely, including an unavailable Codex home and linked or damaged rollout metadata. |
| Launch and daemon integration | Preserve Claude's daemon/profile fixes and cover platform-specific compilation, command deadlines and argv parsing. Unknown daemon support stops account staging; failed child-PID persistence stops and reaps the new process while retaining session state. |

## Recent project activity

The review window covers items created from 2026-08-30 through 2026-09-30. GitHub issue bodies, comments, PR descriptions and available changes were cross-checked against the current source; an open dependency PR does not necessarily mean its dependency is still absent from `dev`.

| Items | Assessment in this candidate |
| --- | --- |
| [#99](https://github.com/xjoker/codex-switch/issues/99), [#103](https://github.com/xjoker/codex-switch/pull/103) | Keep the completed-SSE warmup contract and Luna selection. Extend the model/authentication paths without reverting that behavior. Real-account quota activation is distinct from mock-stream completion. |
| [#100](https://github.com/xjoker/codex-switch/issues/100) | Preserve the Windows ACL fast path, semantic auth comparison and bounded optional daemon restart. The remaining intentional restart cost is controlled by `use.restart_app_server`. |
| [#104](https://github.com/xjoker/codex-switch/issues/104), [#105](https://github.com/xjoker/codex-switch/pull/105) | Preserve shared-daemon account-switch handling and embedded launches. Correct platform-specific helper compilation and strengthen command parsing/deadlines. |
| [#98](https://github.com/xjoker/codex-switch/pull/98), [#101](https://github.com/xjoker/codex-switch/pull/101), [#102](https://github.com/xjoker/codex-switch/pull/102) | `dirs` 7.0.0, `toml` 1.1.6 and `clap` 4.6.7 are already present in the baseline lockfile. Their still-open PRs do not require duplicate merges. |
| [#97](https://github.com/xjoker/codex-switch/pull/97) | The intermediate `toml` 1.1.5 proposal is superseded by 1.1.6. |
| [#92](https://github.com/xjoker/codex-switch/pull/92), [#93](https://github.com/xjoker/codex-switch/pull/93), [#94](https://github.com/xjoker/codex-switch/pull/94), [#95](https://github.com/xjoker/codex-switch/pull/95), [#96](https://github.com/xjoker/codex-switch/pull/96) | Release-action, webbrowser, owo-colors, flate2 and thiserror updates are already integrated. Preserve the pinned release action and existing upgrade/provenance gates. |

## CI and release validation

Recent failures span separate causes. The baseline includes the macOS errno portability change and rustls 0.23.45 for RUSTSEC-2026-0285. The latest baseline CI passed Windows and audit but failed Linux/macOS tests; the source still referenced Windows-only test helpers inside `if cfg!(windows)`, which type-checks both branches. This candidate uses compilation attributes instead.

Release eligibility requires the local gate and all three hosts on the exact candidate commit, followed by the six-target Release workflow, legacy upgrade gate and artifact verification described in [RELEASE.md](RELEASE.md). Historical job status alone is not proof that a new candidate is ready to publish.

Independent review covered provider transport, OAuth rotation/routing, native profile cleanup and launch failure paths. Follow-up repairs include path traversal rejection for damaged profile names and removal of endpoint credentials and untrusted authentication response text from diagnostics.

## Candidate validation

Candidate base version: `20260930.3.0`. Local validation ran on Windows against the combined baseline and these repairs:

- `cargo test --all --offline`: 838 passed (678 unit and 160 integration tests), zero failed or ignored. Unix-only process/signal tests still require the Linux/macOS CI jobs.
- `cargo fmt --check` and `cargo clippy --all-targets --offline -- -D warnings`: passed.
- `cargo audit`: passed after refreshing 1,277 RustSec advisories and scanning 384 dependencies; the only dependency changes in this candidate are `thiserror` 2.0.21 and `rand` 0.10.3, which that audit run did not cover.
- Bash and PowerShell installer syntax: passed.
- Official Codex `0.159.2` executable: passed a local HTTP/SSE smoke covering model sync, probe, launch, a second launch that sweeps old runs, and exact-session resume. All five requests used the expected routing, headers and credentials; the model query used `client_version=0.159.2`. The catalog supplied nonempty instructions, resume reused its original native profile, and shared `auth.json`/`config.toml` were not created or replaced.
- The smoke used temporary homes and a mock provider; it did not validate paid-account quota activation against the production ChatGPT backend.

Full logs from [baseline CI 36661218187](https://github.com/xjoker/codex-switch/actions/runs/36661218187) confirm that Unix tests incorrectly compiled references to Windows-only helpers. Logs from 36660817223 show that defect together with the macOS `libc::__errno_location` error and RUSTSEC-2026-0285. Runs 36659156361, 36657765597 and 36657253340 confirm the latter two failures. This candidate fixes the test compilation boundary and preserves Claude's portable errno and rustls 0.23.45 repairs. Older PR runs 36018117969, 36014601650 and 36557685054 return `log not found`; their public metadata and associated source changes were reviewed, but their full logs are unavailable.

[First combined CI 36667616283](https://github.com/xjoker/codex-switch/actions/runs/36667616283) passed Linux, Windows and format/audit. Its macOS job exposed a cold-start timeout in the Python test wrapper at the two-second required help probe. The routing probe now has a separate ten-second bound, while the optional version probe remains at two seconds. Regressions cover a three-second valid help response and refusal before any live credential write when help fails. Unknown routing still fails closed.

Remote publication remains gated on all three CI hosts passing for the exact candidate commit. Only then may the rolling `dev` tag move to trigger the six-target release workflow, legacy-upgrade jobs and published-artifact verification. The repository's Actions and rolling release record the remote results; local mock tests do not substitute for those gates.
