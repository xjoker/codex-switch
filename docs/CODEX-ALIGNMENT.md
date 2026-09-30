# Codex source alignment, 2026-09-30

The development candidate is based on project commit `be8ea732d35447a789741aa061aed320d05cb4de`, including the native-profile, daemon deadline, semantic-auth comparison, Windows ACL and dependency fixes already on `dev`.

## Upstream reference

- Released contract: [Codex rust-v0.159.2](https://github.com/openai/codex/releases/tag/rust-v0.159.2), commit `ff6aec96948b70d94983af2641a6b67c94faeff5`.
- Additional source cross-check: [main at 8c3612f](https://github.com/openai/codex/tree/8c3612fb638d356579c446de68421288164484dc), retrieved on 2026-09-30.
- Primary contracts: `codex-rs/protocol/src/openai_models.rs`, `models-manager/src/model_info.rs`, `model-provider/src/models_endpoint.rs`, `model-provider-info/src/lib.rs`, `codex-api/src/endpoint/models.rs`, and `backend-client/src/client/rate_limit_resets.rs`.

The version is an explicit reference point, not a promise that an arbitrary later Codex release has the same private backend contract. The ChatGPT Codex backend and a provider's generic OpenAI-compatible model list have different metadata shapes; a model being unavailable through the public API does not exclude it from a ChatGPT-authenticated catalog.

## Minimum supported executable and desktop engines

The project support minimum and the current Codex alignment baseline are both `0.159.2`. This is a codex-switch support policy, not a claim that older Codex versions cannot work independently. `launch` resolves one PATH executable and checks that exact executable before ChatGPT selection, reset-card consumption, provider run creation, or credential staging; the version probe is bounded to four seconds and uses a temporary `CODEX_HOME`. The existing `--help` capability probe remains separate and still determines ChatGPT `--no-daemon` routing.

`codex-switch doctor` reports the PATH CLI. `doctor --desktop-codex <path>` optionally probes a caller-selected desktop-bundled engine too; it does not scan app bundles or guess paths. A missing/unreadable version or a version below minimum fails the report. Omitting the desktop path is `not_checked`, not a failure. Engines above the alignment baseline but at or above the minimum are `above_baseline_unverified`; if both versions are known, `versions_match` compares core and prerelease versions and ignores build metadata. `version_relation` is `same`, `desktop_engine_newer`, or `desktop_engine_older`, relative to the PATH CLI. A mismatch is informational and does not certify every desktop workflow or daemon integration. Doctor only probes versions: it does not validate auth, system/managed requirements, desktop UI behavior, or daemon compatibility. See OpenAI's [Codex troubleshooting guidance](https://learn.chatgpt.com/docs/reference/troubleshooting) and [Windows app / WSL guidance](https://learn.chatgpt.com/docs/windows/windows-app) for the distinction between app surfaces and execution environments.

The gate, doctor and follow-up audit repairs were verified locally on the Windows host: `cargo test --all --quiet` (906 tests: 724 unit and 182 integration; eight integration binaries contain tests, the Windows TUI-shutdown integration binary has zero applicable tests, and doc-tests contain none), `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo audit` (384 crates checked against 1,277 current RustSec advisories, no advisories found), Git Bash `bash -n scripts/install.sh`, PowerShell parsing of `scripts/install.ps1`, and `git diff --check`.

The official Codex 0.159.2 executable passed `doctor` against the PATH CLI and an explicitly supplied desktop-engine path resolving to the same executable (`aligned`, `versions_match: true`, exit 0). Four isolated JSON cases also passed: a newer 0.160.0 executable is accepted as `above_baseline_unverified`; 0.159.1 fails with exit 1; 0.159.2 with build metadata remains aligned and matches; and JSON output is single-envelope, stdout-only with no stderr diagnostics. These checks used temporary homes and fake executables where applicable, with no real account or production request.

Additional isolated CLI smoke tests confirmed that WIF rejects ChatGPT launch before creating live credentials or provider-run state, with one JSON error and no stderr output; a fake-key provider still launches the official engine's `--help` with WIF and a keyring store present, without creating or swapping `auth.json`. This checks the provider branch and process launch, not a real provider completion. No real account or production backend request was used.

These are local checks only. These post-`97a220b` changes have not been pushed; CI on other operating systems and release-artifact verification have not run. The prior `20260930.3.0` validation record elsewhere in this document remains a historical result for that candidate and is not replaced by these results. The candidate remains `20260930.4.0`; this validation does not publish or release it.

## Authentication and audit follow-up

The six actionable findings in the [independent audit](audits/20260930-independent-review.md) are repaired: WIF preflight, managed authentication sources, provider command-auth conflicts, quoted TUI argv, preserved TUI lists after read errors, and reset-credit override routing. ChatGPT policy checks are read-only and run before relevant file-login operations or backend requests; requirements override managed/user defaults. Windows resolves the OS ProgramData known folder, Unix reads `/etc/codex/requirements.toml` and legacy managed defaults, and macOS accepts only forced `com.openai.codex` MDM preferences. Unsupported custom ChatGPT backend routing fails explicitly. This resolver is scoped to authentication constraints; it does not replace Codex's own enforcement of other managed settings.

Refresh remains a special persistence boundary: a received replacement token reaches its saved profile under the existing CAS and refresh identity rules before mutable policy is consulted for live activation. Policy refusal skips activation without discarding that credential; genuine live-auth I/O failures still propagate to the caller. A losing refresh response is not sent or cached; exact credentials already persisted by that same request are accepted idempotently. Strict import and re-login identity checks remain separate.

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

Candidate base version: `20260930.3.0` (commit `7a1ec3c`). Local validation was re-run on Windows (rustc/cargo 1.98.0) against that commit, including the `thiserror` 2.0.21 and `rand` 0.10.3 dependency bumps:

- `cargo test --all`: 854 passed (687 unit and 167 integration tests across eight integration binaries), zero failed or ignored; doc-tests contain none. Unix-only process/signal tests still require the Linux/macOS CI jobs.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`: passed.
- `cargo audit`: passed with no advisories reported, scanning `Cargo.lock` (384 crate dependencies) against 1,277 RustSec advisories. This run covers `thiserror` 2.0.21 and `rand` 0.10.3, the only dependency changes in this candidate.
- Bash and PowerShell installer syntax: passed.
- Official Codex `0.159.2` executable: passed a local HTTP/SSE smoke covering model sync, probe, launch, a second launch that sweeps old runs, and exact-session resume. All five requests used the expected routing, headers and credentials; the model query used `client_version=0.159.2`. The catalog supplied nonempty instructions, resume reused its original native profile, and shared `auth.json`/`config.toml` were not created or replaced.
- The smoke used temporary homes and a mock provider; it did not validate paid-account quota activation against the production ChatGPT backend. It was not repeated for the later `20260930.3.0` changes (launch-time re-probe of a saved Responses denial, warmup retry and refresh recovery, non-ASCII provider headers), which are covered by the unit and integration tests counted above.

Full logs from [baseline CI 36661218187](https://github.com/xjoker/codex-switch/actions/runs/36661218187) confirm that Unix tests incorrectly compiled references to Windows-only helpers. Logs from 36660817223 show that defect together with the macOS `libc::__errno_location` error and RUSTSEC-2026-0285. Runs 36659156361, 36657765597 and 36657253340 confirm the latter two failures. This candidate fixes the test compilation boundary and preserves Claude's portable errno and rustls 0.23.45 repairs. Older PR runs 36018117969, 36014601650 and 36557685054 return `log not found`; their public metadata and associated source changes were reviewed, but their full logs are unavailable.

[First combined CI 36667616283](https://github.com/xjoker/codex-switch/actions/runs/36667616283) passed Linux, Windows and format/audit. Its macOS job exposed a cold-start timeout in the Python test wrapper at the two-second required help probe. The routing probe now has a separate ten-second bound, while the optional version probe remains at two seconds. Regressions cover a three-second valid help response and refusal before any live credential write when help fails. Unknown routing still fails closed.

Remote publication remains gated on all three CI hosts passing for the exact candidate commit. Only then may the rolling `dev` tag move to trigger the six-target release workflow, legacy-upgrade jobs and published-artifact verification. The repository's Actions and rolling release record the remote results; local mock tests do not substitute for those gates.
