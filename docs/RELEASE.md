# Release process

The daily quality gate is defined by `.github/workflows/ci.yml`. Pushes to `dev` and pull requests targeting `dev` or `master` run tests, Clippy, and a build on Linux, macOS, and Windows. The Linux quality job also runs formatting, `cargo audit`, and installer syntax checks. Release builds are defined by `.github/workflows/release.yml` and run only for `v*` and `dev` tag events.

This document is for maintainers. Users should follow the installation and update instructions in the README and do not need to manage Git tags.

## Release eligibility

All of the following must be true before any release push:

- The local branch is `dev`, the worktree is clean, and all intended changes are committed.
- `VERSION` contains the target base version, `Cargo.toml` matches it, and the top of `docs/CHANGELOG.md` contains the matching release section. Ordinary dev builds may keep it Unreleased; allocate the final dev candidate's stable base from the real local date before publishing it so promotion requires no edit.
- Independent code review has no CRITICAL or HIGH findings. Authentication, update, or user-data changes also require security review.
- The local quality gate and a real CLI smoke test pass.
- `git push` has explicit authorization and the commit to publish is recorded.

A development release has two gates: push the branch and wait for all three CI hosts to pass, then move the `dev` tag to trigger the Release workflow. Never move the tag while branch CI is failing.

The final development release before a stable release has an additional acceptance gate:

- Finish code, tests, changelog, README, and repository-backed Wiki sources before publishing the final `dev` build.
- Record the exact commit SHA and ask the maintainer to test that build.
- After acceptance, make no code, documentation, formatting, lockfile, or metadata changes.
- Fast-forward `master` to that exact commit and create the stable tag on the same commit.
- If any change is needed, publish and test a new `dev` build; the previous acceptance no longer qualifies.

The Wiki sync workflow publishes the reviewed `docs/wiki/` sources from `dev`. This publication does not change the accepted source commit; the Wiki content must already match that commit.

## Version policy

Base versions use the SemVer-compatible `YYYYMMDD.N.0` format:

- `YYYYMMDD` is the version-allocation date captured with `date` immediately before publishing the candidate; 2026-07-12 becomes `20260712`. A stable promotion may happen on a later calendar date and keeps the accepted candidate's version.
- `N` is the release sequence allocated on that date, starting at `1`; the second candidate allocated that day is `20260712.2.0`.
- The final component is always `0` because Cargo and SemVer require `major.minor.patch`. Do not use the invalid two-component form `20260712.1`.
- Keep the date in `YYYYMMDD` order; `YYYYDDMM` breaks chronological sorting.
- Migrating from `0.0.x` to the calendar version is an upgrade. Never publish a smaller `0.x` version afterward because self-update will treat it as a downgrade.

| Pushed tag | Version produced by CI | GitHub Release name | Self-update channel | Homebrew |
|---|---|---|---|---|
| `dev` (rolling, overwritten) | `YYYYMMDD.N.0-dev` | `dev` | `--dev` | No |
| `vYYYYMMDD.N.0-<suffix>` (permanent prerelease) | `YYYYMMDD.N.0-<suffix>` | Same as tag | Unavailable to the hardcoded `dev` channel | No |
| `vYYYYMMDD.N.0` (stable) | `YYYYMMDD.N.0` | Same as tag | Default channel | Yes |

> The root `VERSION` file is the release source of truth. The `version` field in `Cargo.toml` mirrors it and never includes `-dev`; CI validates the match and adds the suffix during version injection.
> The final dev and stable builds come from the same commit. The Release workflow adds `-dev` for the rolling `dev` tag and leaves the manifest base unchanged for the stable tag; this version display difference does not require a source edit.
>
> The `--dev` path in `src/update.rs` calls `fetch_release(Some("dev"))`, so self-update cannot discover an independently named prerelease tag.

## ⚠ `dev` is both a branch and a tag

This repository uses `dev` as both the development branch and the rolling release tag. **Use full refspecs for every push, delete, and lookup** or Git can report:

```
error: src refspec dev matches more than one
```

or operate on the wrong ref.

## Publish a development release

Prerequisite: `dev` contains every intended commit and the local worktree is clean.

```bash
# 1) Run the local gate. This is a preflight, not the source of release artifacts.
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all
cargo audit
bash -n scripts/install.sh

# 2) Push the dev branch with a full refspec.
git push origin refs/heads/dev:refs/heads/dev

# 3) Wait for branch CI and confirm the remote branch points to this commit.
gh run list --branch dev --workflow CI --limit 1
git rev-parse refs/remotes/origin/dev

# 4) Delete the old remote dev tag before moving it.
git push origin :refs/tags/dev

# 5) Recreate the local dev tag at HEAD.
git tag -d dev && git tag dev

# 6) Push the tag to build six targets and replace the dev GitHub Release.
git push origin refs/tags/dev:refs/tags/dev
```

> Step 6 **must not use `git push origin dev`** because the branch and tag names are ambiguous. Use `refs/tags/dev:refs/tags/dev`.
>
> Step 2 likewise requires `refs/heads/dev:refs/heads/dev`.

GitHub Actions Release builds are the only distribution source of truth; do not publish from local `target/release`. The Release job verifies every archive against its `.sha256`, then uses GitHub artifact attestations to generate a Sigstore bundle before creating a GitHub Release. Artifacts are:

- Linux / macOS: `.tar.gz` archives named `cs-{linux,darwin}-{amd64,arm64}.tar.gz` plus `.sha256`
- Windows: `.zip` archives named `cs-windows-{amd64,arm64}.zip` plus `.sha256`
- Build provenance: `codex-switch-build-provenance.json`, covering every release archive
- `install.sh` / `install.ps1`
- User update path: `codex-switch self-update --dev`

After creating the GitHub Release, the `legacy-upgrade` job downloads the official `v0.0.19` binary on macOS, Linux, and Windows, runs its original self-update command against the new channel release, and verifies the resulting binary version. This is the compatibility floor for direct self-update; `v0.0.1` and `v0.0.2` remain installer-only.

The compatibility job runs for the rolling `dev` channel and stable releases. Permanent prerelease tags are not discoverable through either self-update channel, so they skip this channel-upgrade check.

Post-release verification must confirm at least:

- The GitHub Actions Release run succeeds, including all six builds and the release job.
- The macOS, Linux, and Windows `legacy-upgrade` jobs prove `v0.0.19` can replace itself with the published version.
- A platform archive downloaded from GitHub Releases matches its `.sha256`.
- A current GitHub CLI verifies that archive against `codex-switch-build-provenance.json` with the repository, `.github/workflows/release.yml`, exact tag ref, the full commit digest reached by that tag, and self-hosted runners denied.
- The unpacked release binary reports the CI-injected version with `codex-switch --version`.
- The original release path works, for example `codex-switch self-update --check --dev`.

## Publish a stable release

Do not run these commands until the maintainer has explicitly accepted the final `dev` build. First verify that the tested development tag, `dev`, and the local `dev` branch all resolve to the same commit.

```bash
# 1) Record and compare the accepted commit before changing master.
git rev-parse refs/heads/dev
git rev-parse refs/tags/dev

# 2) After explicit user acceptance, fast-forward master without edits.
git checkout master
git merge --ff-only refs/heads/dev
git push origin refs/heads/master:refs/heads/master

# 3) Tag that exact commit. This example is the first release on 2026-07-12.
git tag v20260712.1.0
git push origin refs/tags/v20260712.1.0:refs/tags/v20260712.1.0

# 4) CI builds six targets, creates the GitHub Release, and runs the Homebrew job.
```

After tagging, confirm `refs/heads/master`, `refs/tags/dev`, and the stable tag still point to the accepted SHA. A mismatch is a release blocker.

A stable promotion may happen on a later calendar date. Do not bump or edit `VERSION`, `Cargo.toml`, or `docs/CHANGELOG.md` after acceptance; doing so would invalidate the tested candidate.

Before publishing the final dev candidate:

- Run `date` to obtain the real local date, then bump `VERSION` and the synchronized `Cargo.toml` to that day's `YYYYMMDD.N.0`.
- Add the matching `## vYYYYMMDD.N.0 — YYYY-MM-DD` section at the top of `docs/CHANGELOG.md`.

## Troubleshooting

**`error: src refspec dev matches more than one`**
Use `refs/heads/dev:refs/heads/dev` for the branch or `refs/tags/dev:refs/tags/dev` for the tag.

**The `dev` branch push started Wiki but not CI**
Wiki sync only runs when `docs/wiki/**` or `.github/workflows/wiki.yml` changes. Branch quality is the `CI` workflow. Before moving the `dev` tag, confirm `gh run list --commit <sha> --workflow CI` lists Format and audit plus Test on linux, macos, and windows for that commit. An empty commit retriggers `CI` but not Wiki. If GitHub Actions is in an outage, the `push` event may create no workflow run; wait until Actions is operational, then push a new commit or use `workflow_dispatch`. Do not move the tag while that list is empty or failing.

**The dev tag was pushed but CI did not run**
Check whether the Release workflow was triggered and whether `on.push.tags` still includes `"dev"`.

**`self-update --dev` cannot find the new build**
The GitHub Release tag must be the lowercase literal `dev`. A separate tag such as `v20260712.1.0-dev` creates an independent prerelease that the client channel cannot see.

**Should the Cargo.toml version contain `-dev`?**
No. CI appends `-dev`; the local manifest keeps the clean `YYYYMMDD.N.0` base. Increment `N` before another candidate on the same date or clients will treat it as the version they already have.
