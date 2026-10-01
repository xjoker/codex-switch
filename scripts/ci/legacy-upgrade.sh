#!/usr/bin/env bash
# Pre-publish upgrade gate (macOS/Linux): a published legacy release
# self-updates to this workflow's unpublished build artifacts, which a local
# GitHub API mock serves. Nothing is published before every gate passes.
#
# usage: legacy-upgrade.sh <legacy-version>
#   0.0.19        plain self-update (no provenance check in that updater)
#   20260804.1.0  last stable with the removed daemon: its updater verifies
#                 build provenance and restarts the daemon with the new binary
#
# env: EXPECTED_VERSION, IS_DEV, GITHUB_SHA, RUNNER_TEMP; ARTIFACTS_DIR
# (default: artifacts). The provenance check runs `gh attestation verify`
# against the bundle offline, like a user's machine without `gh auth login`.
set -euo pipefail

legacy="${1:?legacy version required}"
: "${EXPECTED_VERSION:?}" "${IS_DEV:?}" "${GITHUB_SHA:?}" "${RUNNER_TEMP:?}"
artifacts_dir="${ARTIFACTS_DIR:-artifacts}"
repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
port=8765
label="com.codex-switch.daemon"

platform="$(uname -s)-$(uname -m)"
case "$platform" in
  Linux-x86_64) asset="cs-linux-amd64.tar.gz" ;;
  Linux-aarch64 | Linux-arm64) asset="cs-linux-arm64.tar.gz" ;;
  Darwin-x86_64) asset="cs-darwin-amd64.tar.gz" ;;
  Darwin-arm64) asset="cs-darwin-arm64.tar.gz" ;;
  *) echo "Unsupported legacy-upgrade runner: $platform" >&2; exit 1 ;;
esac

# Reviewed fixtures: the published archives must never change underneath us.
case "$legacy/$asset" in
  0.0.19/cs-linux-amd64.tar.gz) pinned_hash="3589fdac3d480aea83ab61dd4fb0a7592c018a44415842083df2d5f1d0bb0d2f" ;;
  0.0.19/cs-linux-arm64.tar.gz) pinned_hash="5db981cc5f1380f3bf9ac2d66484c0cc67d06712e617c2cd0457e3058dcb12b0" ;;
  0.0.19/cs-darwin-amd64.tar.gz) pinned_hash="cbc4229285c5e8ea02c9463b7868cd03f2021f5125f5b187ff99e3bebe16e278" ;;
  0.0.19/cs-darwin-arm64.tar.gz) pinned_hash="2e365dc8273c04ee634d593eeada338a46ba7c7db2a1ca8f5c2aff30aec57a98" ;;
  20260804.1.0/cs-linux-amd64.tar.gz) pinned_hash="649bdaed3c380b60537321c59e5ab5e36959d4aed582fb4f587efacdb81763eb" ;;
  20260804.1.0/cs-linux-arm64.tar.gz) pinned_hash="f1b1fd3eb3843d5e1b9eb578d5296eb6938b828c0a34d1aa7b996d0abb6cdbac" ;;
  20260804.1.0/cs-darwin-amd64.tar.gz) pinned_hash="459c01026ccd46660408c36944b2001ba0337248fdae2c094515410a587ac986" ;;
  20260804.1.0/cs-darwin-arm64.tar.gz) pinned_hash="b89e3d82a300795feaba9aa8e51d1675877f0072bfc44fb4a70a9d05f261bd41" ;;
  *) echo "No reviewed fixture for v$legacy $asset" >&2; exit 1 ;;
esac

work="$(mktemp -d "${RUNNER_TEMP}/legacy-upgrade-${legacy}.XXXXXX")"
home="${work}/home"
api_root="${work}/api"
mkdir -p "$home" "$api_root/artifacts"
cp "$artifacts_dir"/* "$api_root/artifacts/"

if [[ "$IS_DEV" == "true" ]]; then
  api_path="repos/xjoker/codex-switch/releases/tags/dev"
  release_tag="dev"
  release_name="dev (${EXPECTED_VERSION})"
  channel="--dev"
else
  api_path="repos/xjoker/codex-switch/releases/latest"
  release_tag="v${EXPECTED_VERSION}"
  release_name="$release_tag"
  channel="--stable"
fi
python3 "$repo_root/scripts/prepare-release-metadata.py" \
  --artifacts-dir "$api_root/artifacts" \
  --output "${api_root}/${api_path}" \
  --base-url "http://127.0.0.1:${port}/artifacts" \
  --tag "$release_tag" \
  --name "$release_name" \
  --extra-asset codex-switch-build-provenance.json \
  --tag-ref-output "${api_root}/repos/xjoker/codex-switch/git/ref/tags/${release_tag}" \
  --commit-sha "$GITHUB_SHA"

python3 -m http.server "$port" --bind 127.0.0.1 --directory "$api_root" >/dev/null 2>&1 &
server_pid=$!
cleanup() {
  kill "$server_pid" 2>/dev/null || true
  if [[ "$(uname -s)" == "Darwin" ]]; then
    launchctl remove "$label" 2>/dev/null || true
  fi
}
trap cleanup EXIT
for attempt in 1 2 3 4 5; do
  curl -fsS "http://127.0.0.1:${port}/${api_path}" >/dev/null && break
  test "$attempt" -lt 5
  sleep 1
done

base="https://github.com/xjoker/codex-switch/releases/download/v${legacy}"
curl -fsSL "${base}/${asset}" -o "${work}/${asset}"
curl -fsSL "${base}/${asset}.sha256" -o "${work}/${asset}.sha256"
(
  cd "$work"
  printf '%s  %s\n' "$pinned_hash" "$asset" | shasum -a 256 -c -
  shasum -a 256 -c "${asset}.sha256"
  tar xzf "$asset"
)
bin="${work}/codex-switch"
test "$("$bin" --version | head -n 1)" = "codex-switch ${legacy}"

# Every legacy binary below runs against the isolated home only.
export HOME="$home"
export CS_GITHUB_API_URL="http://127.0.0.1:${port}"
unset CODEX_HOME CODEX_SWITCH_HOME
pidfile="${home}/.codex-switch/daemon.pid"

assert_upgraded() {
  test "$("$bin" --version | head -n 1)" = "codex-switch ${EXPECTED_VERSION}"
}

wait_until() {
  local description="$1"
  shift
  for _ in $(seq 1 60); do
    if "$@"; then return 0; fi
    sleep 0.5
  done
  echo "Timed out waiting for: $description" >&2
  return 1
}

no_daemon_process() { ! pgrep -f "${bin} daemon start" >/dev/null; }
launch_job_absent() { ! launchctl list "$label" >/dev/null 2>&1; }

if [[ "$legacy" == "0.0.19" ]]; then
  "$bin" self-update "$channel"
  assert_upgraded
  exit 0
fi

if [[ "$(uname -s)" == "Darwin" ]]; then
  # The real failure mode: a KeepAlive LaunchAgent restarted by the old
  # updater with the new binary. The new binary must unregister it.
  "$bin" daemon install
  plist="${home}/Library/LaunchAgents/${label}.plist"
  test -f "$plist"
  wait_until "old daemon to start under launchd" test -s "$pidfile"
  "$bin" self-update "$channel"
  assert_upgraded
  wait_until "LaunchAgent plist removal" test ! -e "$plist"
  wait_until "LaunchAgent job removal" launch_job_absent
  wait_until "daemon process exit" no_daemon_process
  exit 0
fi

# Linux: no systemd user session on the runner, so cover the detached
# daemon through the old updater and the unit cleanup directly.
"$bin" daemon start
wait_until "old detached daemon to start" test -s "$pidfile"
set +e
"$bin" self-update "$channel" >"${work}/self-update.log" 2>&1
status=$?
set -e
cat "${work}/self-update.log"
assert_upgraded
if [[ "$status" -ne 0 ]]; then
  # Expected from the old updater: it restarts a detached daemon and the new
  # binary has none to start. The binary itself was replaced.
  grep -q "self-update completed, but daemon restart failed" "${work}/self-update.log"
fi
wait_until "daemon process exit" no_daemon_process

unit="${home}/.config/systemd/user/codex-switch-daemon.service"
mkdir -p "$(dirname "$unit")"
printf '[Service]\nExecStart=%s daemon start --foreground\n' "$bin" >"$unit"
"$bin" daemon start --foreground
test ! -e "$unit"
