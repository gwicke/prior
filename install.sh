#!/bin/sh
# Install script for prior
# Usage: curl -sSfL https://raw.githubusercontent.com/jondot/prior/main/install.sh | sh

set -e

# ============================================================================
# Project-specific configuration - modify these for your project
# ============================================================================
PROJECT_NAME="prior"
BINARY_NAME="prior"
GITHUB_OWNER="jondot"
GITHUB_REPO="prior"
# Shipped alongside the main binary; a missing one only disables a feature.
# `bsed` backs the `sed` tool; `prior-egress` is bwrap's payload when the
# sandbox has network egress (without it, `network = true` fails with an
# install hint rather than silently granting no egress).
OPTIONAL_BINARIES="bsed prior-egress"
# ============================================================================

# Some Linux distributions don't set HOME
get_home() {
    if [ -n "${HOME:-}" ]; then
        echo "$HOME"
    elif [ -n "${USER:-}" ]; then
        getent passwd "$USER" | cut -d: -f6
    else
        getent passwd "$(id -un)" | cut -d: -f6
    fi
}

# Find a suitable installation directory
get_install_dir() {
    if [ -n "${XDG_BIN_HOME:-}" ]; then
        echo "$XDG_BIN_HOME"
    elif [ -n "${XDG_DATA_HOME:-}" ]; then
        echo "$XDG_DATA_HOME/../bin"
    else
        echo "$(get_home)/.local/bin"
    fi
}

# Default installation directory
INSTALL_DIR="${INSTALL_DIR:-$(get_install_dir)}"

# Parse command line arguments
while [ $# -gt 0 ]; do
    case "$1" in
        -b|--bin-dir)
            INSTALL_DIR="$2"
            shift 2
            ;;
        -h|--help)
            echo "Usage: $0 [OPTIONS]"
            echo "Options:"
            echo "  -b, --bin-dir DIR    Installation directory (default: $INSTALL_DIR)"
            echo "  -h, --help           Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            echo "Use -h or --help for usage information"
            exit 1
            ;;
    esac
done

# Detect OS and architecture
detect_platform() {
    OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
    ARCH="$(uname -m | tr '[:upper:]' '[:lower:]')"
    
    case "$ARCH" in
        x86_64|amd64)
            ARCH="x86_64"
            ;;
        arm64|aarch64)
            ARCH="aarch64"
            ;;
        *)
            echo "Error: Unsupported architecture: $ARCH"
            exit 1
            ;;
    esac
    
    case "$OS" in
        linux)
            PLATFORM="linux"
            ARCHIVE_EXT="tar.gz"
            ;;
        darwin)
            PLATFORM="macos"
            ARCHIVE_EXT="tar.gz"
            ;;
        *)
            echo "Error: Unsupported operating system: $OS"
            exit 1
            ;;
    esac
}

# Get the latest release version from GitHub API
get_latest_version() {
    API_URL="https://api.github.com/repos/${GITHUB_OWNER}/${GITHUB_REPO}/releases/latest"
    
    if ! command -v curl >/dev/null 2>&1; then
        echo "Error: curl is required but not installed"
        exit 1
    fi
    
    VERSION=$(curl -sSfL "$API_URL" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/' | head -1)
    
    if [ -z "$VERSION" ]; then
        echo "Error: Failed to determine latest version"
        exit 1
    fi
    
    echo "$VERSION"
}

# Download and install the binary
install_binary() {
    VERSION="$1"
    ARCHIVE_NAME="${BINARY_NAME}-${PLATFORM}-${ARCH}.${ARCHIVE_EXT}"
    DOWNLOAD_URL="https://github.com/${GITHUB_OWNER}/${GITHUB_REPO}/releases/download/${VERSION}/${ARCHIVE_NAME}"
    
    echo "Installing ${BINARY_NAME} ${VERSION} for ${PLATFORM}-${ARCH}..."
    echo "Download URL: ${DOWNLOAD_URL}"
    
    # Create temporary directory
    TMPDIR=$(mktemp -d)
    trap "rm -rf $TMPDIR" EXIT
    
    # Download archive
    echo "Downloading ${ARCHIVE_NAME}..."
    if ! curl -sSfL -o "$TMPDIR/$ARCHIVE_NAME" "$DOWNLOAD_URL"; then
        echo "Error: Failed to download ${ARCHIVE_NAME}"
        exit 1
    fi
    
    # Extract archive
    echo "Extracting ${ARCHIVE_NAME}..."
    cd "$TMPDIR"
    
    case "$ARCHIVE_EXT" in
        tar.gz)
            if ! tar -xzf "$ARCHIVE_NAME"; then
                echo "Error: Failed to extract ${ARCHIVE_NAME}"
                exit 1
            fi
            ;;
        zip)
            if ! unzip -q "$ARCHIVE_NAME"; then
                echo "Error: Failed to extract ${ARCHIVE_NAME}"
                exit 1
            fi
            ;;
    esac
    
    # Create installation directory if it doesn't exist
    if [ ! -d "${INSTALL_DIR}" ]; then
        echo "Creating directory: ${INSTALL_DIR}"
        mkdir -p "${INSTALL_DIR}"
    fi

    # ${BINARY_NAME} is required. The optional companions only disable a
    # feature when missing (`bsed` backs the `sed` tool, `prior-egress` backs
    # network egress), so they are a warning rather than a failed install.
    for name in "$BINARY_NAME" ${OPTIONAL_BINARIES}; do
        if [ ! -f "$name" ]; then
            if [ "$name" = "$BINARY_NAME" ]; then
                echo "Error: Binary ${BINARY_NAME} not found in archive"
                exit 1
            fi
            echo "Warning: ${name} is not in this archive; skipping (this only disables the"
            echo "         matching agent tool)."
            continue
        fi
        chmod +x "$name"
        echo "Installing ${name} to ${INSTALL_DIR}..."
        if ! mv "$name" "${INSTALL_DIR}/${name}"; then
            echo "Error: Failed to install ${name} to ${INSTALL_DIR}"
            echo "You may need to run with sudo: sudo sh -s -- -b ${INSTALL_DIR}"
            exit 1
        fi
    done

    # Verify installation
    for name in "$BINARY_NAME" ${OPTIONAL_BINARIES}; do
        if [ ! -f "${INSTALL_DIR}/${name}" ]; then
            continue
        fi
        if command -v "$name" >/dev/null 2>&1; then
            INSTALLED_VERSION=$("${INSTALL_DIR}/${name}" --version 2>/dev/null | head -1)
            echo "Successfully installed ${name} to ${INSTALL_DIR}"
            [ -n "$INSTALLED_VERSION" ] && echo "Version: ${INSTALLED_VERSION}"
        else
            echo "Successfully installed ${name} to ${INSTALL_DIR}"
            echo ""
            echo "Warning: ${name} is not in your PATH."
            echo "To use it, add ${INSTALL_DIR} to your PATH:"
            echo ""
            echo "  export PATH=\"\$PATH:${INSTALL_DIR}\""
            echo ""
        fi
    done
}

# Main execution
main() {
    detect_platform
    VERSION=$(get_latest_version)
    install_binary "$VERSION"
}

main "$@"
