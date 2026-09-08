# Developer onboarding

This guide is for engineers and coding agents taking over `codex-switch`. Read the [Architecture overview](Architecture-Overview) before changing authentication, switching, selection, or release behavior.

## Prepare the repository

Requirements:

- Rust 1.88 or newer
- Git
- Python 3 (used by the launch passthrough and distribution-contract integration fixtures)
- `cargo-audit` for the full local quality gate
- Bash for Unix installer validation; PowerShell for Windows installer validation

```bash
git clone https://github.com/xjoker/codex-switch.git
cd codex-switch
git checkout dev
cargo test --all
```

Development and pull requests normally target `dev`. The `master` branch represents stable releases. Do not confuse the `dev` branch with the rolling `dev` tag.

## Find the change boundary

| Change | Primary files | Related verification |
|---|---|---|
| CLI shape | `src/cli.rs`, `src/commands/` | CLI integration tests and `--help` smoke test |
| Authentication or storage | `src/auth.rs`, `src/profile.rs` | Unit tests plus isolated-home integration tests |
| Custom API providers | `src/provider.rs`, `src/commands/provider.rs`, launch provider path in `src/launch.rs`, TUI tabs in `src/tui/` | Isolated-home unit tests; the key must never appear in argv |
| Usage parsing/API | `src/usage/api.rs`, `src/usage/parse.rs` | Mock HTTP and parser tests |
| Account selection | `src/usage/scoring.rs`, `src/commands/profile.rs` | Pure scoring tests and end-to-end scoring tests |
| TUI behavior | `src/tui/` | State/render unit tests and terminal smoke test |
| Installer/update | `scripts/`, `src/update.rs`, release workflow | Distribution contract tests and release artifact checks |
| Configuration | `src/config.rs`, the [Configuration](Configuration) page | Parsing/default/warning tests |

The direct-install matrix is an intentional compatibility contract: Unix defaults to `$HOME/.local/bin`, Unix `--system` uses `/usr/local/bin`, Windows uses `%LOCALAPPDATA%\Programs\codex-switch`, and Homebrew retains ownership of Cellar installs. Keep legacy Unix migration and self-update preflight behavior covered by `tests/test_distribution_contract.rs`; do not replace the platform-specific paths with a single cross-platform abstraction.

Search for the current signature and existing tests before assuming an API or path. Preserve module ownership: command modules coordinate work; domain modules implement reusable behavior.

## Develop behavior test-first

For a bug or a behavior contract, add the smallest test that fails for the intended reason before changing the implementation:

```bash
cargo test descriptive_test_name
# Confirm the new test fails for the expected reason.

# Implement the minimum change.
cargo test descriptive_test_name
```

Pure documentation, configuration-only edits, and visual-only TUI changes do not require an artificial red test. Explain how they were verified instead.

Tests live beside pure module logic and under `tests/` for process, network-mock, distribution, and scoring integration behavior. Tests that touch user state must override `HOME`, `CODEX_HOME`, or `CODEX_SWITCH_HOME` with isolated temporary directories.

## Run the quality gate

Before opening a pull request:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all
cargo audit
bash -n scripts/install.sh
```

On Windows, also parse the installer without executing it:

```powershell
$tokens = $null
$errors = $null
[System.Management.Automation.Language.Parser]::ParseFile(
  (Resolve-Path 'scripts/install.ps1'),
  [ref]$tokens,
  [ref]$errors
) | Out-Null
if ($errors.Count -gt 0) { $errors; exit 1 }
```

GitHub Actions repeats the core checks on Linux, macOS, and Windows. A local pass does not replace the three-host gate for platform-sensitive changes.

## Preserve safety contracts

- Never log or commit tokens, profile files, emails, account IDs, workspace names, or proxy credentials.
- Validate external JSON and CLI input at their boundaries; do not scatter duplicate internal checks.
- Keep JSON stdout machine-readable. Route progress and diagnostics to stderr.
- Serialize live-auth mutations with `auth.lock` and temporary launch staging with `launch.lock`.
- Use atomic replacement for credentials and cache.
- Keep profile deletion recoverable and refuse deletion of the active profile.
- Keep the update trust layers distinct: installers always verify release checksums and may optionally verify repository/workflow provenance; `self-update` must additionally pin the exact tag ref and full source commit digest before replacement.

## Update documentation

Behavior changes must update the closest reader-facing document in the same pull request:

- User-visible commands or behavior: the relevant `docs/wiki/` page ([Feature guide](Feature-Guide), [Custom API providers](Providers), [Command reference](Command-Reference), [Configuration](Configuration), [Updating](Updating), [Troubleshooting](Troubleshooting)) and `README.md` when the quick start is affected
- Module boundaries or data flow: [Architecture overview](Architecture-Overview) (`docs/wiki/Architecture-Overview.md`)
- Contributor workflow: [`CONTRIBUTING.md`](https://github.com/xjoker/codex-switch/blob/dev/CONTRIBUTING.md) or this page
- Release behavior: [`docs/RELEASE.md`](https://github.com/xjoker/codex-switch/blob/dev/docs/RELEASE.md)
- Release notes: the current cycle's section in [`docs/CHANGELOG.md`](https://github.com/xjoker/codex-switch/blob/dev/docs/CHANGELOG.md)

Wiki pages are sourced from `docs/wiki/` and published by CI; never edit the published Wiki directly.

## Diagnose failures

Use a single failing test or command to reduce the feedback loop. If an assumed path, method, or upstream response shape fails, stop and inspect the source or official upstream contract before retrying.

For HTTP behavior, use the existing mock server and response transformers rather than live personal credentials. User-managed OS scheduling examples belong in the reader-facing docs; the project does not add a service manager abstraction.

## Release handoff

Contributors do not move release tags. Maintainers follow [the release process](https://github.com/xjoker/codex-switch/blob/dev/docs/RELEASE.md), including independent review, local verification, branch CI, GitHub Actions artifact construction, checksum validation, and post-release smoke tests.

## Next steps

- Learn the system boundaries in [Architecture overview](Architecture-Overview).
- Before opening a pull request, follow [Contributing](Contributing).
