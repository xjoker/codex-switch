#!/usr/bin/env bash
set -euo pipefail

# codex-switch installer for macOS and Linux
# Usage:
#   curl -fsSL https://github.com/xjoker/codex-switch/releases/latest/download/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- --dev          # install latest dev build
#   CS_VERSION=0.0.11 curl -fsSL .../install.sh | bash    # install specific version

REPO="xjoker/codex-switch"
INSTALL_DIR="/usr/local/bin"
BINARY_NAME="codex-switch"

info()  { printf '\033[0;34m[info]\033[0m  %s\n' "$*"; }
error() { printf '\033[0;31m[error]\033[0m %s\n' "$*" >&2; exit 1; }

# Parse arguments
USE_DEV=false
for arg in "$@"; do
  case "$arg" in
    --dev) USE_DEV=true ;;
    *)     error "Unknown argument: $arg" ;;
  esac
done

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

# Download and extract
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$DOWNLOAD_URL" -o "${TMP_DIR}/${ASSET_NAME}" || error "Download failed. Check the URL or your network."
tar xzf "${TMP_DIR}/${ASSET_NAME}" -C "$TMP_DIR"

# Install
if [ -w "$INSTALL_DIR" ]; then
  mv "${TMP_DIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
else
  info "Installing to ${INSTALL_DIR} (requires sudo)"
  sudo mv "${TMP_DIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
fi

chmod +x "${INSTALL_DIR}/${BINARY_NAME}"
info "Installed: $(${INSTALL_DIR}/${BINARY_NAME} --version)"
info "Run 'codex-switch --help' to get started"
