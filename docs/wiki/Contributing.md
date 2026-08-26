# Contributing

> Canonical contract: [CONTRIBUTING.md](https://github.com/xjoker/codex-switch/blob/dev/CONTRIBUTING.md). GitHub surfaces that file in the pull-request flow; this page summarizes it.

Contributions normally target `dev`; `master` tracks stable releases. The short version of the contract:

- Keep one pull request focused on one behavior or concern, small enough to review and revert independently.
- Define behavior with a failing test before implementation when the change has a testable contract.
- Run the documented quality gate (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, `cargo audit`, installer syntax checks) and state exactly what was not run on your platform.
- Update the affected documentation — the relevant `docs/wiki/` page, `README.md`, and the changelog — in the same pull request.
- Preserve the safety contracts: JSON stdout, atomic file writes, cross-process locks, recoverable deletion, and the file-backed Codex credential-store requirement.
- For a substantial feature or architecture change, open an issue first so the boundary can be agreed before implementation.

Never attach credentials, auth files, provider API keys, personal account metadata, or unredacted debug output — in code, tests, issues, or pull requests. Read the full [contribution guidelines](https://github.com/xjoker/codex-switch/blob/dev/CONTRIBUTING.md) before opening a pull request.

## Next steps

- New contributor: follow [Developer onboarding](Developer-Onboarding).
- Auth, profile, daemon, or release change: read the [Architecture overview](Architecture-Overview).
