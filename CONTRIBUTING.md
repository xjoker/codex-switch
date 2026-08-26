# Contributing to codex-switch

Thank you for improving `codex-switch`. Contributions should be small enough to review and revert independently, and they must preserve local credential and profile safety.

## Before you start

1. Search existing issues and pull requests.
2. Read the [developer onboarding](docs/wiki/Developer-Onboarding.md) and [architecture overview](docs/wiki/Architecture-Overview.md) pages.
3. Base normal work on `dev`; `master` tracks stable releases.
4. For a substantial feature or architecture change, open an issue before implementation so the boundary can be agreed first.

Security vulnerabilities should not be posted with live credentials or exploit data in a public issue. Share only the minimum redacted reproduction material needed to assess the problem.

## Make the change

- Keep one pull request focused on one behavior or concern.
- Add a failing regression or behavior test before implementation when the change has a testable contract.
- Prefer existing modules and dependencies. Do not add a dependency when the standard library or an installed crate already solves the problem.
- Preserve JSON stdout, atomic file writes, cross-process locks, recoverable deletion, and the file-backed Codex credential-store requirement.
- Write code comments only when the reason is not clear from the code. Comments, rustdoc, commit messages, and in-repo explanations are English. User-facing Chinese translations live only in `README_CN.md` and `docs/wiki/Chinese-Guide.md`.
- Write user-facing text and documentation in English.

## Verify locally

Run the full local quality gate:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all
cargo audit
bash -n scripts/install.sh
```

If a command is unavailable on your platform, state exactly what was not run in the pull request. Platform-sensitive changes still require the GitHub Actions Linux, macOS, and Windows jobs to pass.

## Update documentation

Update documentation in the same pull request when behavior changes:

- `docs/wiki/` pages for user-visible behavior: `Feature-Guide.md`, `Providers.md`, `Command-Reference.md`, `Configuration.md`, `Updating.md`, `Troubleshooting.md`
- `README.md` when the quick start or installation flow is affected
- `docs/wiki/Architecture-Overview.md` for module, storage, or data-flow changes
- `docs/wiki/Developer-Onboarding.md` for engineering workflow changes
- `docs/CHANGELOG.md` for the current development cycle

The `docs/wiki/` sources are the reader documentation and are published to the GitHub Wiki by CI; never edit the published Wiki directly.

## Open the pull request

Target `dev` unless a maintainer requests otherwise. Include:

- the user-visible problem and intended behavior
- the implementation boundary and important trade-offs
- the test that failed before the fix and the command/result after the fix, when test-first applies
- the complete verification commands actually run
- platforms or real-account paths that remain unverified
- screenshots for visible TUI changes

Do not include tokens, auth files, unredacted debug output, personal account metadata, or proxy credentials.

## Review and release

Address review findings with focused commits. Maintainers own version changes, release tags, GitHub Releases, and Homebrew publication. A merged pull request is not itself a release.
