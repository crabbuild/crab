#!/bin/sh
# Crab CLI installer
#
# Usage:
#   curl -fsSL https://crab.build/install.sh | bash
#
# Environment variables:
#   CRAB_VERSION     - install a specific version (e.g. "v1.0.15"). Default: latest.
#   CRAB_INSTALL_DIR - installation directory. Default: ~/.crab/bin.
#
# Installs the `crab` binary, mount helpers, and `git-remote-crab` symlink.
# Unix archives include `crab-fuse-mount` and a `crab-nfs-mount` helper link.
# Adds the install directory to PATH via your shell profile.
#
# Windows users: use the PowerShell installer instead:
#   irm https://crab.build/install.ps1 | iex

set -eu
export LC_ALL=C
export LANG=C

REPO="crabbuild/crab-release"
INSTALL_DIR="${CRAB_INSTALL_DIR:-$HOME/.crab/bin}"
VERSION="${CRAB_VERSION:-latest}"
TMP_DIR=""

BOLD='\033[1m'
GREEN='\033[32m'
YELLOW='\033[33m'
RED='\033[31m'
RESET='\033[0m'

info() { printf "${GREEN}==>${RESET} %s\n" "$1" >&2; }
warn() { printf "${YELLOW}warning:${RESET} %s\n" "$1" >&2; }
error() { printf "${RED}error:${RESET} %s\n" "$1" >&2; exit 1; }

error_detail() {
    printf "${RED}error:${RESET} %s\n" "$1" >&2
    shift
    for line in "$@"; do
        printf "  %s\n" "$line" >&2
    done
    exit 1
}

cleanup() {
    if [ -n "$TMP_DIR" ]; then
        rm -rf "$TMP_DIR"
    fi
}

trap cleanup EXIT
trap 'cleanup; exit 1' HUP INT TERM

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        error "$1 is required to install Crab."
    fi
}

ensure_tools() {
    require_cmd curl
    require_cmd tar
    require_cmd mktemp
    require_cmd awk
    require_cmd sed
    require_cmd grep
}

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64) echo "darwin-aarch64" ;;
                x86_64) echo "darwin-x86_64" ;;
                *) error "Unsupported macOS architecture: $arch" ;;
            esac
            ;;
        Linux)
            case "$arch" in
                x86_64|amd64) echo "linux-x86_64" ;;
                aarch64|arm64) echo "linux-aarch64" ;;
                *) error "Unsupported Linux architecture: $arch" ;;
            esac
            ;;
        MINGW*|MSYS*|CYGWIN*)
            error_detail "Windows detected. Use PowerShell instead:" "irm https://crab.build/install.ps1 | iex"
            ;;
        *)
            error "Unsupported OS: $os (supported: macOS, Linux, Windows)"
            ;;
    esac
}

resolve_version() {
    if [ "$VERSION" = "latest" ]; then
        url="https://github.com/${REPO}/releases/latest"
        resolved="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$url" 2>/dev/null || true)"

        case "$resolved" in
            */releases/tag/*)
                VERSION="$(printf '%s' "$resolved" | sed 's|.*/releases/tag/||')"
                ;;
            *)
                api_url="https://api.github.com/repos/${REPO}/releases/latest"
                body="$(curl -fsSL -H 'Accept: application/vnd.github+json' "$api_url" 2>/dev/null || true)"
                VERSION="$(printf '%s' "$body" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | sed -n '1p')"
                ;;
        esac

        if [ -z "$VERSION" ]; then
            error_detail "Failed to resolve the latest Crab release." \
                "Checked: $url" \
                "Make sure ${REPO} is public and has a non-draft release."
        fi
    fi

    case "$VERSION" in
        v*) ;;
        *) VERSION="v${VERSION}" ;;
    esac
}

download_file() {
    url="$1"
    output="$2"
    label="$3"

    if ! curl -fL --retry 3 --retry-delay 1 --connect-timeout 20 -o "$output" "$url"; then
        error_detail "Failed to download $label." \
            "$url" \
            "Check that ${REPO} is public and ${VERSION} includes this release asset."
    fi
}

sha256_file() {
    file="$1"

    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$file" | awk '{print $NF}'
    else
        error "sha256sum, shasum, or openssl is required to verify the Crab download."
    fi
}

verify_checksum() {
    asset="$1"
    file="$2"
    checksums="$3"

    expected="$(
        awk -v asset="$asset" '
            $2 == asset || $2 == "*" asset {
                print $1
                found = 1
                exit
            }
            END {
                if (!found) exit 1
            }
        ' "$checksums" 2>/dev/null || true
    )"

    if [ -z "$expected" ]; then
        error_detail "Checksum file does not include $asset." \
            "Downloaded: https://github.com/${REPO}/releases/download/${VERSION}/SHA256SUMS.txt"
    fi

    actual="$(sha256_file "$file")"
    if [ "$actual" != "$expected" ]; then
        error_detail "Checksum verification failed for $asset." \
            "expected: $expected" \
            "actual:   $actual"
    fi
}

verify_tarball_layout() {
    tarball="$1"
    asset="$2"
    target="$3"

    entries="$(tar -tzf "$tarball" 2>/dev/null || true)"
    entry_count="$(printf '%s\n' "$entries" | sed '/^$/d' | wc -l | awk '{print $1}')"

    case "$target" in
        darwin-*)
            has_crab="$(printf '%s\n' "$entries" | grep -x "crab" || true)"
            has_fuse_mount="$(printf '%s\n' "$entries" | grep -x "crab-fuse-mount" || true)"
            has_nfs_mount="$(printf '%s\n' "$entries" | grep -x "crab-nfs-mount" || true)"
            if [ "$entry_count" != "3" ] || [ -z "$has_crab" ] || [ -z "$has_fuse_mount" ] || [ -z "$has_nfs_mount" ]; then
                error_detail "Unexpected archive layout in $asset." \
                    "Expected root-level crab, crab-fuse-mount, and crab-nfs-mount entries."
            fi
            ;;
        *)
            has_crab="$(printf '%s\n' "$entries" | grep -x "crab" || true)"
            has_fuse_mount="$(printf '%s\n' "$entries" | grep -x "crab-fuse-mount" || true)"
            has_nfs_mount="$(printf '%s\n' "$entries" | grep -x "crab-nfs-mount" || true)"
            if [ "$entry_count" != "3" ] || [ -z "$has_crab" ] || [ -z "$has_fuse_mount" ] || [ -z "$has_nfs_mount" ]; then
                error_detail "Unexpected archive layout in $asset." \
                    "Expected root-level crab, crab-fuse-mount, and crab-nfs-mount entries."
            fi
            ;;
    esac
}

download_tarball() {
    target="$1"
    asset="crab-${target}.tar.gz"
    tarball="$TMP_DIR/$asset"
    checksums="$TMP_DIR/SHA256SUMS.txt"
    url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
    checksum_url="https://github.com/${REPO}/releases/download/${VERSION}/SHA256SUMS.txt"

    info "Downloading crab ${VERSION} for ${target}"
    info "  $url"

    download_file "$url" "$tarball" "$asset"
    download_file "$checksum_url" "$checksums" "SHA256SUMS.txt"
    verify_checksum "$asset" "$tarball" "$checksums"
    verify_tarball_layout "$tarball" "$asset" "$target"

    echo "$tarball"
}

install_binary() {
    tarball="$1"
    extract_dir="$TMP_DIR/extract"
    staged_bin="$INSTALL_DIR/.crab.tmp.$$"

    info "Installing to $INSTALL_DIR"
    mkdir -p "$extract_dir"
    mkdir -p "$INSTALL_DIR"

    tar -xzf "$tarball" -C "$extract_dir"
    if [ ! -f "$extract_dir/crab" ]; then
        error "crab binary not found after extraction."
    fi

    cp "$extract_dir/crab" "$staged_bin"
    chmod 755 "$staged_bin"
    mv "$staged_bin" "$INSTALL_DIR/crab"

    if [ ! -e "$extract_dir/crab-nfs-mount" ] && [ ! -L "$extract_dir/crab-nfs-mount" ]; then
        error "crab-nfs-mount helper not found after extraction."
    fi
    if [ -d "$INSTALL_DIR/crab-nfs-mount" ] && [ ! -L "$INSTALL_DIR/crab-nfs-mount" ]; then
        error "refusing to replace directory: $INSTALL_DIR/crab-nfs-mount"
    fi
    ln -sf "crab" "$INSTALL_DIR/crab-nfs-mount"
    info "Created symlink: crab-nfs-mount -> crab"

    if [ -f "$extract_dir/crab-fuse-mount" ]; then
        staged_fuse_mount="$INSTALL_DIR/.crab-fuse-mount.tmp.$$"
        cp "$extract_dir/crab-fuse-mount" "$staged_fuse_mount"
        chmod 755 "$staged_fuse_mount"
        mv "$staged_fuse_mount" "$INSTALL_DIR/crab-fuse-mount"
        info "Installed crab-fuse-mount"
    fi

    ln -sf "$INSTALL_DIR/crab" "$INSTALL_DIR/git-remote-crab"
    info "Created symlink: git-remote-crab -> crab"
}

detect_shell_profile() {
    case "${SHELL:-}" in
        */zsh) echo "$HOME/.zshrc" ;;
        */bash)
            if [ -f "$HOME/.bash_profile" ]; then
                echo "$HOME/.bash_profile"
            else
                echo "$HOME/.bashrc"
            fi
            ;;
        */fish) echo "$HOME/.config/fish/config.fish" ;;
        *) echo "" ;;
    esac
}

update_path() {
    # Check if it's already in PATH
    case ":${PATH:-}:" in
        *":$INSTALL_DIR:"*)
            info "$INSTALL_DIR is already in PATH"
            return
            ;;
    esac

    profile="$(detect_shell_profile)"
    if [ -z "$profile" ]; then
        warn "Could not detect your shell profile. Add this to your shell config:"
        printf "  export PATH=\"%s:\$PATH\"\n" "$INSTALL_DIR"
        return
    fi

    export_line="export PATH=\"$INSTALL_DIR:\$PATH\""
    if [ "$profile" = "$HOME/.config/fish/config.fish" ]; then
        export_line="set -gx PATH \"$INSTALL_DIR\" \$PATH"
    fi

    if [ -f "$profile" ] && grep -Fq "$INSTALL_DIR" "$profile"; then
        info "PATH already configured in $profile"
        return
    fi

    info "Adding $INSTALL_DIR to PATH in $profile"
    mkdir -p "$(dirname "$profile")"
    {
        printf "\n# Crab CLI\n"
        printf "%s\n" "$export_line"
    } >> "$profile"
}

main() {
    printf "${BOLD}Installing Crab CLI${RESET}\n\n"

    ensure_tools
    TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/crab-install.XXXXXX")"
    target="$(detect_target)"
    info "Platform: $target"

    resolve_version
    tarball="$(download_tarball "$target")"
    install_binary "$tarball"
    update_path

    printf "\n${GREEN}${BOLD}Installed crab ${VERSION}${RESET}\n"
    printf "Run ${BOLD}exec \$SHELL${RESET} (or open a new terminal) to load the updated PATH.\n"
    printf "Then run ${BOLD}crab version${RESET} to verify the installation.\n\n"
    printf "Get started:\n"
    printf "  ${BOLD}crab init${RESET}       - initialize a new repository\n"
    printf "  ${BOLD}crab clone${RESET}      - clone an existing repository\n"
    printf "  ${BOLD}crab --help${RESET}     - see all commands\n"
}

main "$@"
