#!/bin/sh
# OxidePM Installer
# Usage: curl -fsSL https://oxidepm.com/install.sh | sh

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

select_version() {
    case "${OXIDEPM_VERSION:-}" in
        "") get_latest_version ;;
        v*) VERSION="$OXIDEPM_VERSION" ;;
        *) VERSION="v$OXIDEPM_VERSION" ;;
    esac
    case "$VERSION" in
        *[!A-Za-z0-9.-]*) error "Invalid release tag: $VERSION" ;;
        v[0-9]*) ;;
        *) error "Invalid release tag: $VERSION" ;;
    esac
}

# Print the sha256 of a file (sha256sum on Linux, shasum on macOS)
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        error "Neither sha256sum nor shasum is available, so the download cannot be checked"
    fi
}

verify_signature() {
    if ! command -v minisign >/dev/null 2>&1; then
        printf '%s\n' 'Release signature was not checked because minisign is unavailable.'
        return 0
    fi

    signature="$TEMP_DIR/checksums.txt.minisig"
    if status=$(curl -fsSL -w '%{http_code}' "$CHECKSUMS_URL.minisig" -o "$signature"); then
        :
    else
        if [ "$?" -eq 22 ] && [ "$status" = 404 ] && [ -n "${OXIDEPM_VERSION:-}" ] &&
            printf '%s\n' "$VERSION" | LC_ALL=C grep -Eq '^v0\.([0-3]\.[0-9]+(-[A-Za-z0-9.-]+)?|4\.0-rc\.[0-3])$'; then
            printf 'Release %s predates release signatures. Checking SHA-256 only.\n' "$VERSION"
            return 0
        fi
        error "Could not download $CHECKSUMS_URL.minisig"
    fi

    awk 'NR == 1 && $0 !~ /^untrusted comment: / { exit 1 } END { if (NR != 4) exit 1 }' "$signature" ||
        error "Release signature has an invalid format for $VERSION"

    for public_key in \
        'RWQMeVEZKn2/BHzgJLldWbECE2BqKV4a0rmNXEzn5N0lxrSi/dDuFjHC' \
        'RWTuJOozYEG8Y1hYBqUF8CBncU1PUQgqfJ/hr6Zcwgt1cDwfeRrZh+q5'; do
        if comment=$(minisign -V -H -Q -P "$public_key" -m "$TEMP_DIR/checksums.txt" -x "$signature" 2>/dev/null); then
            [ "$comment" = "oxidepm release checksums tag=$VERSION" ] ||
                error "Release signature has an unexpected trusted comment for $VERSION"
            success "Release signature verified for $VERSION"
            return 0
        fi
    done
    error "Release signature verification failed for $VERSION"
}

# Authenticate checksums.txt when possible, then check the archive's exact filename and digest.
verify_checksum() {
    ARCHIVE="$1"
    NAME="oxidepm-$TARGET.tar.gz"
    CHECKSUMS_URL="https://github.com/$REPO/releases/download/$VERSION/checksums.txt"

    curl -fsSL "$CHECKSUMS_URL" -o "$TEMP_DIR/checksums.txt" || error "Could not download $CHECKSUMS_URL"
    verify_signature

    # the line for exactly this file name (sha256sum output: "<hash>  <name>")
    EXPECTED=$(awk -v n="$NAME" '{ f = $2; sub(/^\*/, "", f); if (f == n) print $1 }' "$TEMP_DIR/checksums.txt")
    case "$EXPECTED" in
        "") error "checksums.txt does not list $NAME" ;;
        *[!0-9a-fA-F]*) error "checksums.txt has an invalid entry for $NAME" ;;
    esac
    if [ "${#EXPECTED}" -ne 64 ]; then
        error "checksums.txt has an invalid entry for $NAME"
    fi

    ACTUAL=$(sha256_of "$ARCHIVE")
    if [ "$ACTUAL" != "$EXPECTED" ]; then
        error "Checksum mismatch for $NAME (expected $EXPECTED, got $ACTUAL). Nothing was installed."
    fi
    success "Checksum verified (sha256 $ACTUAL)"
}

# Download and install
install() {
    DOWNLOAD_URL="https://github.com/$REPO/releases/download/$VERSION/oxidepm-$TARGET.tar.gz"

    info "Downloading from $DOWNLOAD_URL"

    TEMP_DIR=$(mktemp -d)
    trap 'rm -rf "$TEMP_DIR"' EXIT

    curl -fsSL "$DOWNLOAD_URL" -o "$TEMP_DIR/oxidepm.tar.gz"

    verify_checksum "$TEMP_DIR/oxidepm.tar.gz"

    info "Extracting..."
    tar -xzf "$TEMP_DIR/oxidepm.tar.gz" -C "$TEMP_DIR"

    info "Installing to $INSTALL_DIR"

    # Check if we need sudo
    if [ -w "$INSTALL_DIR" ]; then
        mv "$TEMP_DIR/oxidepm" "$INSTALL_DIR/"
        mv "$TEMP_DIR/oxidepmd" "$INSTALL_DIR/"
        # releases before the sandbox launcher do not have it
        if [ -f "$TEMP_DIR/oxidepm-sandbox" ]; then mv "$TEMP_DIR/oxidepm-sandbox" "$INSTALL_DIR/"; fi
    else
        warn "Need sudo to install to $INSTALL_DIR"
        sudo mv "$TEMP_DIR/oxidepm" "$INSTALL_DIR/"
        sudo mv "$TEMP_DIR/oxidepmd" "$INSTALL_DIR/"
        if [ -f "$TEMP_DIR/oxidepm-sandbox" ]; then sudo mv "$TEMP_DIR/oxidepm-sandbox" "$INSTALL_DIR/"; fi
    fi

    chmod +x "$INSTALL_DIR/oxidepm"
    chmod +x "$INSTALL_DIR/oxidepmd"
    if [ -f "$INSTALL_DIR/oxidepm-sandbox" ]; then chmod +x "$INSTALL_DIR/oxidepm-sandbox"; fi

    install_adapters
}

# The adapters a release carries (the runtimes among them) go to share/oxidepm/adapters next to
# the programs' directory, where "oxidepm adapter import --bundled" looks for them. A release
# without them installs as before.
install_adapters() {
    BUNDLE_SRC="$TEMP_DIR/share/oxidepm/adapters"
    BUNDLE_DIR=""
    if [ ! -f "$BUNDLE_SRC/index.json" ]; then
        return 0
    fi
    BUNDLE_DIR="$(dirname "$INSTALL_DIR")/share/oxidepm/adapters"
    info "Installing the adapters of this release to $BUNDLE_DIR"
    if [ -w "$INSTALL_DIR" ]; then
        rm -rf "$BUNDLE_DIR" && mkdir -p "$BUNDLE_DIR" && cp -R "$BUNDLE_SRC/." "$BUNDLE_DIR/"
    else
        sudo rm -rf "$BUNDLE_DIR" && sudo mkdir -p "$BUNDLE_DIR" && sudo cp -R "$BUNDLE_SRC/." "$BUNDLE_DIR/"
    fi
}

# The runtimes (node, npm, pnpm, yarn, python, go, rust, cargo) are adapters. One command imports
# and enables the ones the release carries; it is printed and run, and a failure does not undo
# the install.
import_runtimes() {
    if [ -z "$BUNDLE_DIR" ]; then
        warn "This release carries no adapters, so no runtime is installed. Import one with: oxidepm adapter import <name> --enable"
        return 0
    fi
    IMPORT="$INSTALL_DIR/oxidepm adapter import --bundled --tag runtime --enable --yes"
    info "Importing the runtimes: $IMPORT"
    if "$INSTALL_DIR/oxidepm" adapter import --bundled --tag runtime --enable --yes; then
        success "The runtimes are installed"
    else
        warn "The runtimes were not imported. Run this when the reason above is dealt with:"
        warn "  $IMPORT"
        warn "On a machine that cannot enforce the adapters' sandbox the command stops and says so; add --allow-unenforced if you accept that."
    fi
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
    echo "  ║   Process manager with adapters   ║"
    echo "  ╚═══════════════════════════════════╝"
    echo ""

    detect_platform
    select_version
    install
    verify
    import_runtimes
}

main
