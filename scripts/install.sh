#!/bin/sh
# OxidePM Installer
# Usage: curl -fsSL https://oxidekit.github.io/oxidepm/install.sh | sh

set -e

REPO="oxidekit/oxidepm"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info() { printf "${BLUE}[INFO]${NC} %s\n" "$1"; }
success() { printf "${GREEN}[OK]${NC} %s\n" "$1"; }
warn() { printf "${YELLOW}[WARN]${NC} %s\n" "$1"; }
error() { printf "${RED}[ERROR]${NC} %s\n" "$1"; exit 1; }

# Detect OS and architecture
detect_platform() {
    OS="$(uname -s)"
    ARCH="$(uname -m)"

    case "$OS" in
        Linux)
            case "$ARCH" in
                x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
                aarch64) TARGET="aarch64-unknown-linux-gnu" ;;
                *)       error "Unsupported architecture: $ARCH" ;;
            esac
            ;;
        Darwin)
            case "$ARCH" in
                x86_64)  TARGET="x86_64-apple-darwin" ;;
                arm64)   TARGET="aarch64-apple-darwin" ;;
                *)       error "Unsupported architecture: $ARCH" ;;
            esac
            ;;
        *)
            error "Unsupported OS: $OS"
            ;;
    esac

    info "Detected platform: $TARGET"
}

# Get latest version from GitHub
get_latest_version() {
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -z "$VERSION" ]; then
        error "Could not determine latest version"
    fi
    info "Latest version: $VERSION"
}

# Download and install
install() {
    DOWNLOAD_URL="https://github.com/$REPO/releases/download/$VERSION/oxidepm-$TARGET.tar.gz"

    info "Downloading from $DOWNLOAD_URL"

    TEMP_DIR=$(mktemp -d)
    trap "rm -rf $TEMP_DIR" EXIT

    curl -fsSL "$DOWNLOAD_URL" -o "$TEMP_DIR/oxidepm.tar.gz"

    info "Extracting..."
    tar -xzf "$TEMP_DIR/oxidepm.tar.gz" -C "$TEMP_DIR"

    info "Installing to $INSTALL_DIR"

    # Check if we need sudo
    if [ -w "$INSTALL_DIR" ]; then
        mv "$TEMP_DIR/oxidepm" "$INSTALL_DIR/"
        mv "$TEMP_DIR/oxidepmd" "$INSTALL_DIR/"
    else
        warn "Need sudo to install to $INSTALL_DIR"
        sudo mv "$TEMP_DIR/oxidepm" "$INSTALL_DIR/"
        sudo mv "$TEMP_DIR/oxidepmd" "$INSTALL_DIR/"
    fi

    chmod +x "$INSTALL_DIR/oxidepm"
    chmod +x "$INSTALL_DIR/oxidepmd"
}

# Verify installation
verify() {
    if command -v oxidepm >/dev/null 2>&1; then
        success "OxidePM installed successfully!"
        echo ""
        oxidepm --version
        echo ""
        info "Get started with: oxidepm start ./your-app"
    else
        warn "Installation complete but oxidepm not in PATH"
        info "Add $INSTALL_DIR to your PATH or run: $INSTALL_DIR/oxidepm"
    fi
}

main() {
    echo ""
    echo "  ╔═══════════════════════════════════╗"
    echo "  ║       OxidePM Installer           ║"
    echo "  ║  Process Manager for Node & Rust  ║"
    echo "  ╚═══════════════════════════════════╝"
    echo ""

    detect_platform
    get_latest_version
    install
    verify
}

main
