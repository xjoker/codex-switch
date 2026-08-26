# Documentation

Reader-facing documentation lives in [`docs/wiki/`](wiki) and is published automatically to the [GitHub Wiki](https://github.com/xjoker/codex-switch/wiki). The Wiki pages are the single set of user and contributor documentation; they are reviewed in pull requests with the code they describe.

## Choose a starting point

| Reader | Start here | Continue with |
|---|---|---|
| New user | [Getting started](wiki/Getting-Started.md) | [Feature guide](wiki/Feature-Guide.md) |
| Operator | [Configuration](wiki/Configuration.md) | [Troubleshooting](wiki/Troubleshooting.md) |
| Custom API / OpenRouter | [Custom API providers](wiki/Providers.md) | [Command reference](wiki/Command-Reference.md) |
| Contributor | [Contributing](../CONTRIBUTING.md) | [Developer onboarding](wiki/Developer-Onboarding.md) |
| Maintainer | [Architecture overview](wiki/Architecture-Overview.md) | [Release process](RELEASE.md) |
| Release reader | [Changelog](CHANGELOG.md) | [GitHub Releases](https://github.com/xjoker/codex-switch/releases) |

## Repository-only documents

These stay outside the Wiki because they are maintainer- or process-facing:

- [Release process](RELEASE.md) defines the maintainer-only release gates.
- [Changelog](CHANGELOG.md) records release-level behavior changes.
- [Wiki maintenance](WIKI.md) explains how `docs/wiki/` is authored and published.
- [Architecture decision records](adr/) capture significant one-time decisions.
- [Contributing](../CONTRIBUTING.md) defines the pull request contract (kept at the repository root so GitHub surfaces it).

## Documentation contract

- Keep English as the canonical documentation language. Chinese companion pages may summarize tasks and link back to the English source, but they must not become a second specification.
- Keep warnings and prerequisites near the top.
- Describe observed behavior, not planned behavior.
- Link to source files for implementation details that may change.
- Update the relevant `docs/wiki/` page in the same pull request as a behavior change.
- Do not edit the published Wiki as an independent copy; update `docs/wiki/` and let CI sync it.
