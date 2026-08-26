#!/usr/bin/env bash
set -euo pipefail

# codex-switch installer / uninstaller for macOS and Linux
# Usage:
#   curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash
#   curl -fsSL https://github.com/xjoker/codex-switch/releases/download/dev/install.sh | bash -s -- --dev
#   curl -fsSL .../install.sh | bash -s -- --system       # install system-wide (may require sudo)
#   curl -fsSL .../install.sh | bash -s -- --uninstall    # uninstall codex-switch
#   CS_VERSION=20260712.1.0 curl -fsSL .../install.sh | bash  # install specific version

REPO="xjoker/codex-switch"
PROVENANCE_ASSET="codex-switch-build-provenance.json"
RELEASE_WORKFLOW="xjoker/codex-switch/.github/workflows/release.yml"
USER_INSTALL_DIR="${HOME}/.local/bin"
SYSTEM_INSTALL_DIR="/usr/local/bin"
BINARY_NAME="codex-switch"
DATA_DIR="${HOME}/.codex-switch"
LEGACY_BIN="${SYSTEM_INSTALL_DIR}/${BINARY_NAME}"
SYSTEM_INSTALL_MARKER="${SYSTEM_INSTALL_DIR}/.codex-switch-system-install-v1"
PATH_BLOCK_BEGIN="# >>> codex-switch PATH >>>"
PATH_BLOCK_END="# <<< codex-switch PATH <<<"

info()  { printf '\033[0;34m[info]\033[0m  %s\n' "$*"; }
warn()  { printf '\033[0;33m[warn]\033[0m  %s\n' "$*" >&2; }
error() { printf '\033[0;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

# Verify the downloaded archive's Sigstore build provenance, the same guarantee
# `self-update` enforces. The SHA-256 check only proves the archive matches the
# checksum published in the *same* release, so an attacker who can replace both
# files is trusted; attestation instead proves the artifact was built by this
# repository's release workflow on a GitHub-hosted runner and cannot be forged.
#
# Uses offline `--bundle` mode, which needs neither `gh auth login` nor any
# GitHub API call, so it works during a fresh `curl | bash` install. When a
# GitHub CLI with attestation support is unavailable the archive is still
# checksum-verified; set CS_REQUIRE_PROVENANCE=1 to make that a hard failure.
verify_build_provenance() {
  local archive_path="$1"
  local require="${CS_REQUIRE_PROVENANCE:-0}"

  if ! command -v gh >/dev/null 2>&1 || ! gh attestation --help >/dev/null 2>&1; then
    if [ "$require" = "1" ]; then
      error "CS_REQUIRE_PROVENANCE=1 but a GitHub CLI with attestation support was not found. Install https://cli.github.com/ and retry."
    fi
    warn "GitHub CLI with attestation support not found; skipping build-provenance verification (the SHA-256 checksum was still verified). Install https://cli.github.com/ and re-run, or set CS_REQUIRE_PROVENANCE=1 to require it."
    return 0
  fi

  local bundle_url="${DOWNLOAD_URL%/*}/${PROVENANCE_ASSET}"
  local bundle_path="${TMP_DIR}/${PROVENANCE_ASSET}"
  if ! curl -fsSL "$bundle_url" -o "$bundle_path"; then
    if [ "$require" = "1" ]; then
      error "CS_REQUIRE_PROVENANCE=1 but the build-provenance bundle could not be downloaded from ${bundle_url}."
    fi
    warn "Could not download the build-provenance bundle (${bundle_url}); skipping provenance verification (the SHA-256 checksum was still verified)."
    return 0
  fi

  if gh attestation verify "$archive_path" \
    --bundle "$bundle_path" \
    --repo "$REPO" \
    --signer-workflow "$RELEASE_WORKFLOW" \
    --deny-self-hosted-runners >/dev/null 2>&1; then
    info "Build provenance verified: ${ASSET_NAME}"
  else
    error "Build-provenance verification failed for ${ASSET_NAME}; refusing to install. The artifact is not attested as built by ${RELEASE_WORKFLOW}."
  fi
}

resolve_profile_target() (
  local profile_target="$1"
  local link_target link_hops=0 physical_dir
  while [ -L "$profile_target" ]; do
    link_hops=$((link_hops + 1))
    [ "$link_hops" -le 40 ] || error "Too many symbolic links while resolving $1."
    link_target="$(readlink "$profile_target")" || error "Failed to resolve symbolic link $1."
    case "$link_target" in
      /*) ;;
      *) link_target="$(dirname "$profile_target")/${link_target}" ;;
    esac
    profile_target="$link_target"
  done
  physical_dir="$(CDPATH= cd -P "$(dirname "$profile_target")" && pwd -P)" || error "Failed to resolve profile directory for $1."
  printf '%s/%s\n' "$physical_dir" "$(basename "$profile_target")"
)

file_identity() (
  local path="$1" identity
  if identity="$(stat -f '%d:%i' "$path" 2>/dev/null)"; then
    printf '%s\n' "$identity"
  elif identity="$(stat -c '%d:%i' "$path" 2>/dev/null)"; then
    printf '%s\n' "$identity"
  else
    error "Failed to identify ${path}."
  fi
)

remove_path_block() (
  local profile_file="$1"
  local profile_target current_profile_target profile_identity current_profile_identity
  local profile_dir tmp_file=""
  [ -f "$profile_file" ] || return 0
  grep -F "$PATH_BLOCK_BEGIN" "$profile_file" >/dev/null 2>&1 || return 0
  profile_target="$(resolve_profile_target "$profile_file")"
  profile_identity="$(file_identity "$profile_target")"
  profile_dir="$(dirname "$profile_target")"
  tmp_file="$(mktemp "${profile_dir}/.${BINARY_NAME}.XXXXXX")" || error "Failed to create temporary profile file for ${profile_file}."
  trap '[ -z "$tmp_file" ] || rm -f "$tmp_file"' EXIT
  if ! cp -p "$profile_target" "$tmp_file"; then
    error "Failed to prepare temporary profile file for ${profile_file}."
  fi
  if ! awk -v begin="$PATH_BLOCK_BEGIN" -v end="$PATH_BLOCK_END" '
    $0 == begin {
      if (inside || seen_begin) invalid = 1
      inside = 1
      seen_begin = 1
      next
    }
    $0 == end {
      if (!inside || seen_end) invalid = 1
      inside = 0
      seen_end = 1
      next
    }
    !inside { print }
    END {
      if (invalid || !seen_begin || !seen_end || inside) exit 1
    }
  ' "$profile_target" > "$tmp_file"; then
    error "Failed to remove codex-switch PATH block from ${profile_file}."
  fi
  current_profile_target="$(resolve_profile_target "$profile_file")"
  if [ "$current_profile_target" != "$profile_target" ]; then
    error "Profile link changed while updating ${profile_file}; original file was left unchanged."
  fi
  current_profile_identity="$(file_identity "$current_profile_target")"
  if [ "$current_profile_identity" != "$profile_identity" ]; then
    error "Profile file changed while updating ${profile_file}; newer contents were left unchanged."
  fi
  if ! mv -f "$tmp_file" "$profile_target"; then
    error "Failed to replace ${profile_file} with the updated PATH configuration."
  fi
  tmp_file=""
  info "Removed codex-switch PATH entry from ${profile_file}."
)

remove_managed_path_blocks() {
  remove_path_block "${HOME}/.zprofile"
  remove_path_block "${HOME}/.bash_profile"
  remove_path_block "${HOME}/.profile"
  remove_path_block "${HOME}/.config/fish/config.fish"
}

# Parse arguments
USE_DEV=false
UNINSTALL=false
SYSTEM_INSTALL=false
for arg in "$@"; do
  case "$arg" in
    --dev)       USE_DEV=true ;;
    --uninstall) UNINSTALL=true ;;
    --system)    SYSTEM_INSTALL=true ;;
    *)           error "Unknown argument: $arg" ;;
  esac
done

if [ "$SYSTEM_INSTALL" = true ]; then
  INSTALL_DIR="$SYSTEM_INSTALL_DIR"
else
  INSTALL_DIR="$USER_INSTALL_DIR"
fi

# ── Uninstall ────────────────────────────────────────────
if [ "$UNINSTALL" = true ]; then
  info "Uninstalling codex-switch..."

  SERVICE_UNINSTALL_FAILED=false
  DAEMON_BIN="$(command -v codex-switch 2>/dev/null || true)"
  if [ -z "$DAEMON_BIN" ] && [ -x "${INSTALL_DIR}/${BINARY_NAME}" ]; then
    DAEMON_BIN="${INSTALL_DIR}/${BINARY_NAME}"
  elif [ -z "$DAEMON_BIN" ] && [ "$SYSTEM_INSTALL" = false ] && [ -x "$LEGACY_BIN" ]; then
    DAEMON_BIN="$LEGACY_BIN"
  fi
  if [ -n "$DAEMON_BIN" ]; then
    if "$DAEMON_BIN" daemon uninstall; then
      info "Removed daemon service."
    else
      warn "Failed to remove daemon service with '${DAEMON_BIN} daemon uninstall'."
      SERVICE_UNINSTALL_FAILED=true
    fi
  else
    case "$(uname -s)" in
      Darwin)
        PLIST_PATH="${HOME}/Library/LaunchAgents/com.codex-switch.daemon.plist"
        if [ -f "$PLIST_PATH" ]; then
          if ! launchctl unload "$PLIST_PATH"; then
            warn "Failed to unload LaunchAgent ${PLIST_PATH}."
            SERVICE_UNINSTALL_FAILED=true
          else
            rm -f "$PLIST_PATH"
            info "Removed LaunchAgent ${PLIST_PATH}."
          fi
        fi
        ;;
      Linux)
        UNIT_PATH="${HOME}/.config/systemd/user/codex-switch-daemon.service"
        if [ -f "$UNIT_PATH" ]; then
          if ! systemctl --user disable --now codex-switch-daemon; then
            warn "Failed to disable systemd user service codex-switch-daemon."
            SERVICE_UNINSTALL_FAILED=true
          else
            rm -f "$UNIT_PATH"
            systemctl --user daemon-reload || warn "Failed to reload systemd user units."
            info "Removed systemd user service ${UNIT_PATH}."
          fi
        fi
        ;;
    esac
  fi

  if [ "$SERVICE_UNINSTALL_FAILED" = true ]; then
    error "Daemon service cleanup failed; binary and data were kept. Resolve the service error and retry uninstall."
  fi

  # Check for Homebrew install
  BREW_BIN="$(command -v codex-switch 2>/dev/null || true)"
  if [ -n "$BREW_BIN" ]; then
    RESOLVED="$(readlink -f "$BREW_BIN" 2>/dev/null || realpath "$BREW_BIN" 2>/dev/null || echo "$BREW_BIN")"
    case "$RESOLVED" in
      */Cellar/codex-switch/*|*/Homebrew/*)
        info "Homebrew installation detected. Running: brew uninstall codex-switch"
        brew uninstall codex-switch || error "brew uninstall failed"
        info "Homebrew package removed."
        # Skip direct-install removal — Homebrew was the only install method
        BREW_REMOVED=true
        ;;
    esac
  fi

  # Remove direct-install binary (skip if we just removed the Homebrew package)
  if [ "${BREW_REMOVED:-false}" != true ]; then
    BIN_PATH="${INSTALL_DIR}/${BINARY_NAME}"
    if [ "$SYSTEM_INSTALL" = false ] && [ ! -f "$BIN_PATH" ] && [ -f "$LEGACY_BIN" ]; then
      BIN_PATH="$LEGACY_BIN"
    fi
    if [ -f "$BIN_PATH" ]; then
      BIN_RESOLVED="$(readlink -f "$BIN_PATH" 2>/dev/null || realpath "$BIN_PATH" 2>/dev/null || echo "$BIN_PATH")"
      case "$BIN_RESOLVED" in
        */Cellar/codex-switch/*|*/Homebrew/*)
          error "${BIN_PATH} is managed by Homebrew. Run 'brew uninstall codex-switch' instead."
          ;;
      esac
      BIN_DIR="${BIN_PATH%/*}"
      if [ "$BIN_PATH" = "$LEGACY_BIN" ] && [ -w "$BIN_DIR" ]; then
        rm -f "$BIN_PATH" "$SYSTEM_INSTALL_MARKER"
      elif [ "$BIN_PATH" = "$LEGACY_BIN" ]; then
        info "Removing ${BIN_PATH} (requires sudo)"
        sudo rm -f "$BIN_PATH" "$SYSTEM_INSTALL_MARKER"
      elif [ -w "$BIN_DIR" ]; then
        rm -f "$BIN_PATH"
      else
        info "Removing ${BIN_PATH} (requires sudo)"
        sudo rm -f "$BIN_PATH"
      fi
      info "Removed ${BIN_PATH}"
    fi
  fi

  if [ "$SYSTEM_INSTALL" = false ]; then
    remove_managed_path_blocks
  fi

  # Remove data directory
  if [ -d "$DATA_DIR" ]; then
    printf '%s' "[info]  Remove data directory ${DATA_DIR}? [y/N] "
    read -r answer < /dev/tty 2>/dev/null || answer="n"
    case "$answer" in
      [yY]|[yY][eE][sS])
        rm -rf "$DATA_DIR"
        info "Removed ${DATA_DIR}"
        ;;
      *)
        info "Kept ${DATA_DIR}"
        ;;
    esac
  fi

  info "codex-switch has been uninstalled."
  exit 0
fi

# ── Install ──────────────────────────────────────────────

# Detect OS and architecture
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$OS" in
  linux)  PLATFORM="linux" ;;
  darwin) PLATFORM="darwin" ;;
  *)      error "Unsupported OS: $OS" ;;
esac

case "$ARCH" in
  x86_64|amd64)   ARCH_NAME="amd64" ;;
  aarch64|arm64)   ARCH_NAME="arm64" ;;
  *)               error "Unsupported architecture: $ARCH" ;;
esac

# Check for Homebrew-installed codex-switch
BREW_BIN="$(command -v codex-switch 2>/dev/null || true)"
if [ -n "$BREW_BIN" ]; then
  RESOLVED="$(readlink -f "$BREW_BIN" 2>/dev/null || realpath "$BREW_BIN" 2>/dev/null || echo "$BREW_BIN")"
  case "$RESOLVED" in
    */Cellar/codex-switch/*|*/Homebrew/*)
      error "codex-switch is installed via Homebrew ($BREW_BIN). Please run 'brew uninstall codex-switch' first, then re-run this installer."
      ;;
  esac
fi

# A pre-user-install direct binary in /usr/local/bin would otherwise shadow the
# new user-owned binary. Validate sudo before downloading, then remove it only
# after the new binary is installed successfully.
MIGRATE_LEGACY=false
LEGACY_NEEDS_SUDO=false
if [ "$SYSTEM_INSTALL" = false ] && [ -e "$LEGACY_BIN" ]; then
  LEGACY_RESOLVED="$(readlink -f "$LEGACY_BIN" 2>/dev/null || realpath "$LEGACY_BIN" 2>/dev/null || echo "$LEGACY_BIN")"
  case "$LEGACY_RESOLVED" in
    */Cellar/codex-switch/*|*/Homebrew/*)
      error "codex-switch is installed via Homebrew ($LEGACY_BIN). Please run 'brew uninstall codex-switch' first, then re-run this installer."
      ;;
    *)
      if [ ! -w "$SYSTEM_INSTALL_DIR" ]; then
        info "Legacy system install detected at ${LEGACY_BIN}; migration requires sudo once."
        LEGACY_NEEDS_SUDO=true
      else
        info "Legacy system install detected at ${LEGACY_BIN}; it will be migrated."
      fi
      MIGRATE_LEGACY=true
      ;;
  esac
fi

ASSET_NAME="cs-${PLATFORM}-${ARCH_NAME}.tar.gz"

# Get release URL
if [ "$USE_DEV" = true ]; then
  VERSION="dev"
  DOWNLOAD_URL="https://github.com/${REPO}/releases/download/dev/${ASSET_NAME}"
else
  VERSION="${CS_VERSION:-latest}"
  if [ "$VERSION" = "latest" ]; then
    DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${ASSET_NAME}"
  else
    DOWNLOAD_URL="https://github.com/${REPO}/releases/download/v${VERSION}/${ASSET_NAME}"
  fi
fi

info "Detected: ${PLATFORM}/${ARCH_NAME}"
info "Downloading: ${DOWNLOAD_URL}"

# Download, verify, and extract
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$DOWNLOAD_URL" -o "${TMP_DIR}/${ASSET_NAME}" || error "Download failed. Check the URL or your network."
CHECKSUM_URL="${DOWNLOAD_URL}.sha256"
CHECKSUM_FILE="${TMP_DIR}/${ASSET_NAME}.sha256"
curl -fsSL "$CHECKSUM_URL" -o "$CHECKSUM_FILE" || error "Checksum download failed. The release is incomplete or your network is unavailable."

EXPECTED_SHA256="$(awk -v filename="$ASSET_NAME" '
  NF != 2 { exit 1 }
  length($1) != 64 || $1 !~ /^[[:xdigit:]]+$/ { exit 1 }
  $2 != filename && $2 != "*" filename { exit 1 }
  NR > 1 { exit 1 }
  { print tolower($1) }
  END { if (NR != 1) exit 1 }
' "$CHECKSUM_FILE")" || error "Invalid checksum file for ${ASSET_NAME}."
[ -n "$EXPECTED_SHA256" ] || error "Checksum file for ${ASSET_NAME} is empty."

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA256="$(sha256sum "${TMP_DIR}/${ASSET_NAME}" | awk '{print tolower($1)}')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA256="$(shasum -a 256 "${TMP_DIR}/${ASSET_NAME}" | awk '{print tolower($1)}')"
else
  error "Neither sha256sum nor shasum is available to verify the download."
fi

[ "$ACTUAL_SHA256" = "$EXPECTED_SHA256" ] || error "Checksum mismatch for ${ASSET_NAME}; refusing to extract it."
info "Checksum verified: ${ASSET_NAME}"
verify_build_provenance "${TMP_DIR}/${ASSET_NAME}"
tar xzf "${TMP_DIR}/${ASSET_NAME}" -C "$TMP_DIR"

if [ "$MIGRATE_LEGACY" = true ] && [ "$LEGACY_NEEDS_SUDO" = true ]; then
  sudo -v || error "Cannot migrate ${LEGACY_BIN} without sudo. Re-run with access to remove the legacy binary, or use --system."
fi

# Install
if [ "$SYSTEM_INSTALL" = true ]; then
  if [ -w "$INSTALL_DIR" ]; then
    install -m 0755 "${TMP_DIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    install -m 0644 /dev/null "$SYSTEM_INSTALL_MARKER"
  else
    info "Installing system-wide to ${INSTALL_DIR} (requires sudo)"
    sudo install -m 0755 "${TMP_DIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    sudo install -m 0644 /dev/null "$SYSTEM_INSTALL_MARKER"
  fi
else
  mkdir -p "$INSTALL_DIR"
  install -m 0755 "${TMP_DIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
fi

if [ "$MIGRATE_LEGACY" = true ]; then
  if [ "$LEGACY_NEEDS_SUDO" = true ]; then
    sudo rm -f "$LEGACY_BIN" "$SYSTEM_INSTALL_MARKER"
  else
    rm -f "$LEGACY_BIN" "$SYSTEM_INSTALL_MARKER"
  fi
  info "Removed legacy install: ${LEGACY_BIN}"
fi

if [ "$SYSTEM_INSTALL" = false ]; then
  case ":${PATH}:" in
    *":${USER_INSTALL_DIR}:"*) ;;
    *)
      case "${SHELL:-}" in
        */zsh)
          PROFILE_FILE="${HOME}/.zprofile"
          PATH_LINE='export PATH="$HOME/.local/bin:$PATH"'
          ;;
        */bash)
          if [ "$PLATFORM" = "darwin" ]; then
            PROFILE_FILE="${HOME}/.bash_profile"
          else
            PROFILE_FILE="${HOME}/.profile"
          fi
          PATH_LINE='export PATH="$HOME/.local/bin:$PATH"'
          ;;
        */fish)
          PROFILE_FILE="${HOME}/.config/fish/config.fish"
          PATH_LINE='fish_add_path "$HOME/.local/bin"'
          mkdir -p "${HOME}/.config/fish"
          ;;
        *)
          PROFILE_FILE=""
          PATH_LINE=""
          ;;
      esac
      if [ -n "$PROFILE_FILE" ]; then
        if ! grep -F "$PATH_BLOCK_BEGIN" "$PROFILE_FILE" >/dev/null 2>&1; then
          printf '\n%s\n%s\n%s\n' "$PATH_BLOCK_BEGIN" "$PATH_LINE" "$PATH_BLOCK_END" >> "$PROFILE_FILE"
          info "Added ${USER_INSTALL_DIR} to PATH in ${PROFILE_FILE}; restart your shell to apply it."
        fi
      else
        warn "Add ${USER_INSTALL_DIR} to your PATH to run codex-switch by name."
      fi
      ;;
  esac
fi

info "Installed: $(${INSTALL_DIR}/${BINARY_NAME} --version)"
info "Run 'codex-switch --help' to get started"
