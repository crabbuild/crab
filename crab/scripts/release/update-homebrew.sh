#!/usr/bin/env bash
# update-homebrew.sh — Update the Homebrew tap formula after a release.
#
# Downloads the release tarballs (or uses local dist/), computes SHA256
# hashes, renders the formula template, and pushes to the tap repo.
#
# Prerequisites:
#   - `gh` CLI authenticated
#   - Write access to the tap repo (crabbuild/homebrew-tap)
#
# Usage:
#   ./scripts/release/update-homebrew.sh              # uses latest release tag
#   ./scripts/release/update-homebrew.sh v1.0.1       # explicit tag
#   ./scripts/release/update-homebrew.sh --local      # use local dist/ checksums
#   ./scripts/release/update-homebrew.sh --dry-run    # render formula, don't push

set -euo pipefail
export GIT_TERMINAL_PROMPT=0

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

TAP_REPO="${TAP_REPO:-crabbuild/homebrew-tap}"
RELEASE_REPO="${RELEASE_REPO:-crabbuild/crab-oss}"
FORMULA_NAME="${FORMULA_NAME:-crab}"
DIST_DIR="${DIST_DIR:-$CRAB_DIR/dist}"

DRY_RUN=false
USE_LOCAL=false
TAG=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        --local)
            USE_LOCAL=true
            shift
            ;;
        v*)
            TAG="$1"
            shift
            ;;
        *)
            echo "Usage: $0 [--dry-run] [--local] [vX.Y.Z]" >&2
            exit 1
            ;;
    esac
done

# Resolve version
if [[ -z "$TAG" ]]; then
    VERSION="$(grep -m1 '^version' "$CRAB_DIR/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')"
    TAG="v${VERSION}"
else
    VERSION="${TAG#v}"
fi

BOLD='\033[1m'
GREEN='\033[32m'
YELLOW='\033[33m'
RED='\033[31m'
RESET='\033[0m'

info() { printf "${GREEN}==>${RESET} %s\n" "$1"; }
warn() { printf "${YELLOW}warning:${RESET} %s\n" "$1"; }
error() { printf "${RED}error:${RESET} %s\n" "$1" >&2; exit 1; }

setup_gh_git_auth() {
    if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
        gh auth setup-git >/dev/null 2>&1 || true
    fi
}

info "Updating Homebrew formula for crab ${TAG}"

# --- Compute SHA256 hashes ---
# Using plain variables instead of associative arrays for bash 3.2 compat.

SHA_DARWIN_AARCH64=""
SHA_DARWIN_X86_64=""
SHA_LINUX_X86_64=""
SHA_LINUX_AARCH64=""

compute_sha() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        error "Neither shasum nor sha256sum is available."
    fi
}

read_checksums() {
    local checksums_file="$1"
    local hash filename

    while read -r hash filename; do
        case "$filename" in
            *darwin-aarch64*) SHA_DARWIN_AARCH64="$hash" ;;
            *darwin-x86_64*)  SHA_DARWIN_X86_64="$hash" ;;
            *linux-x86_64*)   SHA_LINUX_X86_64="$hash" ;;
            *linux-aarch64*)  SHA_LINUX_AARCH64="$hash" ;;
        esac
    done < "$checksums_file"
}

require_formula_shas() {
    local missing=()

    [[ -n "$SHA_DARWIN_AARCH64" ]] || missing+=("crab-darwin-aarch64.tar.gz")
    [[ -n "$SHA_DARWIN_X86_64" ]] || missing+=("crab-darwin-x86_64.tar.gz")
    [[ -n "$SHA_LINUX_AARCH64" ]] || missing+=("crab-linux-aarch64.tar.gz")
    [[ -n "$SHA_LINUX_X86_64" ]] || missing+=("crab-linux-x86_64.tar.gz")

    if [[ ${#missing[@]} -gt 0 ]]; then
        printf "${RED}error:${RESET} Missing SHA256 values required for the Homebrew formula:\n" >&2
        printf "  %s\n" "${missing[@]}" >&2
        exit 1
    fi
}

if [[ "$USE_LOCAL" == true ]]; then
    if [[ ! -d "$DIST_DIR" ]]; then
        error "dist/ not found. Run ./scripts/release/release.sh --dry-run first."
    fi

    if [[ -f "$DIST_DIR/SHA256SUMS.txt" ]]; then
        read_checksums "$DIST_DIR/SHA256SUMS.txt"
    else
        if [[ -f "$DIST_DIR/crab-darwin-aarch64.tar.gz" ]]; then
            SHA_DARWIN_AARCH64="$(compute_sha "$DIST_DIR/crab-darwin-aarch64.tar.gz")"
        fi

        if [[ -f "$DIST_DIR/crab-darwin-x86_64.tar.gz" ]]; then
            SHA_DARWIN_X86_64="$(compute_sha "$DIST_DIR/crab-darwin-x86_64.tar.gz")"
        fi

        if [[ -f "$DIST_DIR/crab-linux-x86_64.tar.gz" ]]; then
            SHA_LINUX_X86_64="$(compute_sha "$DIST_DIR/crab-linux-x86_64.tar.gz")"
        fi

        if [[ -f "$DIST_DIR/crab-linux-aarch64.tar.gz" ]]; then
            SHA_LINUX_AARCH64="$(compute_sha "$DIST_DIR/crab-linux-aarch64.tar.gz")"
        fi
    fi
else
    # Download checksums from the GitHub release
    if ! command -v gh &>/dev/null; then
        error "\`gh\` CLI not found."
    fi

    TMPDIR="$(mktemp -d)"
    trap 'rm -rf "$TMPDIR"' EXIT

    info "Downloading SHA256SUMS from ${RELEASE_REPO} ${TAG}..."
    if gh release download "$TAG" --repo "$RELEASE_REPO" --pattern "SHA256SUMS.txt" --dir "$TMPDIR" 2>/dev/null; then
        read_checksums "$TMPDIR/SHA256SUMS.txt"
    else
        # Fall back to downloading individual archives
        warn "SHA256SUMS.txt not found, downloading archives..."
        for target in darwin-aarch64 darwin-x86_64 linux-x86_64 linux-aarch64; do
            archive="crab-${target}.tar.gz"
            if gh release download "$TAG" --repo "$RELEASE_REPO" --pattern "$archive" --dir "$TMPDIR" 2>/dev/null; then
                sha="$(compute_sha "$TMPDIR/$archive")"
                case "$target" in
                    darwin-aarch64) SHA_DARWIN_AARCH64="$sha" ;;
                    darwin-x86_64)  SHA_DARWIN_X86_64="$sha" ;;
                    linux-x86_64)   SHA_LINUX_X86_64="$sha" ;;
                    linux-aarch64)  SHA_LINUX_AARCH64="$sha" ;;
                esac
            else
                warn "Missing $archive in release"
            fi
        done
    fi

    rm -rf "$TMPDIR"
    trap - EXIT
fi

require_formula_shas

# --- Render formula ---

OUTPUT_DIR="$CRAB_DIR/dist"
mkdir -p "$OUTPUT_DIR"
FORMULA_PATH="$OUTPUT_DIR/${FORMULA_NAME}.rb"

cat > "$FORMULA_PATH" << EOF
# typed: false
# frozen_string_literal: true

# This file is auto-generated by crab/scripts/release/update-homebrew.sh
# Do not edit manually.

class Crab < Formula
  desc "Serverless git remote helper — repositories in cloud object storage"
  homepage "https://crab.build"
  version "${VERSION}"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/${RELEASE_REPO}/releases/download/${TAG}/crab-darwin-aarch64.tar.gz"
      sha256 "${SHA_DARWIN_AARCH64}"
    end
    on_intel do
      url "https://github.com/${RELEASE_REPO}/releases/download/${TAG}/crab-darwin-x86_64.tar.gz"
      sha256 "${SHA_DARWIN_X86_64}"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/${RELEASE_REPO}/releases/download/${TAG}/crab-linux-aarch64.tar.gz"
      sha256 "${SHA_LINUX_AARCH64}"
    end
    on_intel do
      url "https://github.com/${RELEASE_REPO}/releases/download/${TAG}/crab-linux-x86_64.tar.gz"
      sha256 "${SHA_LINUX_X86_64}"
    end
  end

  def install
    bin.install "crab"
    bin.install "crab-fuse-mount"
    bin.install_symlink "crab" => "crab-nfs-mount"
    bin.install_symlink "crab" => "git-remote-crab"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/crab version")
  end
end
EOF

info "Formula written to $FORMULA_PATH"

if [[ "$DRY_RUN" == true ]]; then
    echo ""
    printf "${BOLD}--- Formula (dry run) ---${RESET}\n"
    cat "$FORMULA_PATH"
    exit 0
fi

# --- Push to tap repo ---

info "Pushing formula to ${TAP_REPO}..."
setup_gh_git_auth

TMPDIR_TAP="$(mktemp -d)"

gh repo clone "$TAP_REPO" "$TMPDIR_TAP" -- --depth 1 2>/dev/null || \
    error "Could not clone ${TAP_REPO}. Create it first: gh repo create ${TAP_REPO} --public"

mkdir -p "$TMPDIR_TAP/Formula"
cp "$FORMULA_PATH" "$TMPDIR_TAP/Formula/${FORMULA_NAME}.rb"

(
    cd "$TMPDIR_TAP"
    git config user.email "release@crabbuild.com"
    git config user.name "Crab Release Bot"
    git add Formula/${FORMULA_NAME}.rb
    if git diff --cached --quiet; then
        echo "==> Formula unchanged — nothing to push."
    else
        git commit -m "crab ${TAG}"
        git push origin HEAD
        echo "==> Pushed formula update for ${TAG}"
    fi
)

rm -rf "$TMPDIR_TAP"

echo ""
printf "${GREEN}${BOLD}Done!${RESET} Users can now run:\n"
echo "  brew install crabbuild/tap/crab"
