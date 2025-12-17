#!/bin/bash
set -euo pipefail

# Installer for sway-mirror
# Usage: curl -fsSL https://raw.githubusercontent.com/pescheckit/sway-mirror/main/install.sh | bash

REPO="pescheckit/sway-mirror"
INSTALL_DIR="${HOME}/.local/bin"
BINARY_NAME="sway-mirror"

echo "=== sway-mirror installer ==="
echo ""

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    x86_64) ARCH_SUFFIX="linux-x86_64" ;;
    aarch64) ARCH_SUFFIX="linux-aarch64" ;;
    *)
        echo "Unsupported architecture: $ARCH"
        echo "Please build from source: cargo install --git https://github.com/$REPO"
        exit 1
        ;;
esac

# Create install directory
mkdir -p "$INSTALL_DIR"

# Get latest release
echo "Fetching latest release..."
if command -v curl &>/dev/null; then
    LATEST=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null | grep '"tag_name"' | cut -d'"' -f4) || LATEST=""
elif command -v wget &>/dev/null; then
    LATEST=$(wget -qO- "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null | grep '"tag_name"' | cut -d'"' -f4) || LATEST=""
fi

if [ -z "$LATEST" ]; then
    echo "No release found. Building from source..."
    if ! command -v cargo &>/dev/null; then
        echo "Error: cargo not found. Install Rust from https://rustup.rs/"
        exit 1
    fi
    cargo install --git "https://github.com/$REPO"
    exit 0
fi

echo "Latest release: $LATEST"

# Download binary and checksum
DOWNLOAD_URL="https://github.com/$REPO/releases/download/$LATEST/$BINARY_NAME-$ARCH_SUFFIX"
CHECKSUM_URL="$DOWNLOAD_URL.sha256"

TEMP_FILE=$(mktemp)
TEMP_CHECKSUM=$(mktemp)
trap 'rm -f "$TEMP_FILE" "$TEMP_CHECKSUM"' EXIT

echo "Downloading $BINARY_NAME..."
if command -v curl &>/dev/null; then
    curl -fsSL "$DOWNLOAD_URL" -o "$TEMP_FILE"
    curl -fsSL "$CHECKSUM_URL" -o "$TEMP_CHECKSUM" 2>/dev/null || true
elif command -v wget &>/dev/null; then
    wget -qO "$TEMP_FILE" "$DOWNLOAD_URL"
    wget -qO "$TEMP_CHECKSUM" "$CHECKSUM_URL" 2>/dev/null || true
fi

# Verify checksum
if [ -s "$TEMP_CHECKSUM" ]; then
    echo "Verifying checksum..."
    EXPECTED=$(awk '{print $1}' "$TEMP_CHECKSUM")
    ACTUAL=$(sha256sum "$TEMP_FILE" | awk '{print $1}')
    if [ "$EXPECTED" != "$ACTUAL" ]; then
        echo "ERROR: Checksum verification failed!"
        exit 1
    fi
    echo "Checksum verified OK"
else
    echo "Warning: No checksum available"
fi

# Install
mv "$TEMP_FILE" "$INSTALL_DIR/$BINARY_NAME"
chmod +x "$INSTALL_DIR/$BINARY_NAME"

echo ""
echo "Installed to $INSTALL_DIR/$BINARY_NAME"

# Check PATH
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    echo ""
    echo "Add this to your ~/.bashrc or ~/.zshrc:"
    echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

echo ""
echo "Run '$BINARY_NAME --help' to get started."
