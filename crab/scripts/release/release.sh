#!/usr/bin/env bash
# release.sh - Build crab CLI release archives and optionally publish them.
#
# The release contract is six CLI artifacts:
#   - macOS:   x86_64, aarch64
#   - Linux:   x86_64, aarch64
#   - Windows: x86_64, aarch64
#
# macOS builds run on macOS with the native Rust toolchain. Linux builds run
# in Docker containers. Windows builds use cargo-xwin locally, or the hosted
# GitHub Actions workflow for publishing.
#
# Usage:
#   ./scripts/release/release.sh --dry-run
#   ./scripts/release/release.sh
#   ./scripts/release/release.sh --target linux-x86_64 --dry-run
#   ./scripts/release/release.sh --allow-partial --dry-run
#
# Publishing requires `gh` authenticated with access to crabbuild/crab-release.
# Publishing also requires retained enterprise replica evidence verified through
# `scripts/release/verify-replica-release-evidence.sh`.

set -euo pipefail
export LC_ALL=C
export LANG=C

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
WORKSPACE_DIR="$(cd "$CRAB_DIR/.." && pwd)"

RELEASE_REPO="${RELEASE_REPO:-crabbuild/crab-release}"
DIST_DIR="${DIST_DIR:-$CRAB_DIR/dist}"
TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_DIR/target}"
DOCKER="${DOCKER:-docker}"
CARGO="${CARGO:-cargo}"
REPLICA_RELEASE_EVIDENCE_DIR="${REPLICA_RELEASE_EVIDENCE_DIR:-}"
REPLICA_RELEASE_EVIDENCE_OUTPUT="${REPLICA_RELEASE_EVIDENCE_OUTPUT:-replica-release-evidence-verify.json}"
REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID="${REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID:-}"
CRAB_CLI_FEATURES_NO_FUSE="simd-accel,tier,replication-s3-control-plane,replication-gcs-control-plane,replication-azure-control-plane,coordinator-dynamodb,coordinator-spanner,coordinator-cosmosdb,watch,nfs,gix-pathmatch"
CRAB_CLI_FEATURES_WITH_FUSE="${CRAB_CLI_FEATURES_NO_FUSE},fuse"

DRY_RUN=false
ALLOW_PARTIAL=false
LIST_TARGETS=false
REQUESTED_TARGETS=()

BOLD='\033[1m'
GREEN='\033[32m'
YELLOW='\033[33m'
RED='\033[31m'
RESET='\033[0m'

info() { printf "${GREEN}==>${RESET} %s\n" "$1"; }
warn() { printf "${YELLOW}warning:${RESET} %s\n" "$1" >&2; }
die() { printf "${RED}error:${RESET} %s\n" "$1" >&2; exit 1; }

usage() {
    cat <<USAGE
Usage: $0 [options]

Options:
  --dry-run            Build archives and checksums without publishing.
  --target NAME        Build one target. May be repeated.
  --allow-partial      Build only targets available on this machine.
  --list-targets       Print the release target matrix.
  --all                Accepted for compatibility; all targets are the default.
  -h, --help           Show this help.

Targets:
  darwin-aarch64, darwin-x86_64, linux-aarch64, linux-x86_64,
  windows-aarch64, windows-x86_64

Environment for publishing:
  REPLICA_RELEASE_EVIDENCE_DIR              Retained live evidence directory.
  REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID  Exact replica-live-<run>-<attempt> ID.
  REPLICA_RELEASE_EVIDENCE_OUTPUT           Verification JSON output path.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        --target)
            [[ $# -ge 2 ]] || die "--target requires a target name"
            REQUESTED_TARGETS+=("$2")
            shift 2
            ;;
        --allow-partial)
            ALLOW_PARTIAL=true
            shift
            ;;
        --list-targets)
            LIST_TARGETS=true
            shift
            ;;
        --all)
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "Unknown option: $1. Run $0 --help."
            ;;
    esac
done

# Matrix fields are parallel arrays for macOS' Bash 3.2.
TARGET_NAMES=(
    darwin-aarch64
    darwin-x86_64
    linux-aarch64
    linux-x86_64
    windows-aarch64
    windows-x86_64
)
TARGET_TRIPLES=(
    aarch64-apple-darwin
    x86_64-apple-darwin
    aarch64-unknown-linux-gnu
    x86_64-unknown-linux-gnu
    aarch64-pc-windows-msvc
    x86_64-pc-windows-msvc
)
TARGET_BUILDERS=(
    cargo-darwin
    cargo-darwin
    docker-linux
    docker-linux
    xwin-windows
    xwin-windows
)
TARGET_EXTS=(
    tar.gz
    tar.gz
    tar.gz
    tar.gz
    zip
    zip
)

target_index() {
    local name="$1"
    local i
    for i in "${!TARGET_NAMES[@]}"; do
        if [[ "${TARGET_NAMES[$i]}" == "$name" ]]; then
            echo "$i"
            return 0
        fi
    done
    return 1
}

print_targets() {
    local i
    for i in "${!TARGET_NAMES[@]}"; do
        printf "%-18s %-30s %s\n" \
            "${TARGET_NAMES[$i]}" "${TARGET_TRIPLES[$i]}" "${TARGET_BUILDERS[$i]}"
    done
}

if [[ "$LIST_TARGETS" == true ]]; then
    print_targets
    exit 0
fi

VERSION="$(grep -m1 '^version' "$CRAB_DIR/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')"
TAG="v${VERSION}"
GIT_SHA="$(git -C "$WORKSPACE_DIR" rev-parse --short HEAD 2>/dev/null || true)"
export CRAB_BUILD_VERSION="$VERSION"
export GIT_SHA

HOST_OS="$(uname -s)"
HOST_ARCH="$(uname -m)"
HOST_RUST_TARGET="$(rustc -vV 2>/dev/null | awk '/^host: / { print $2 }' || true)"

HAS_DOCKER=false
if command -v "$DOCKER" >/dev/null 2>&1 && "$DOCKER" info >/dev/null 2>&1; then
    HAS_DOCKER=true
fi

HAS_CARGO_XWIN=false
if "$CARGO" xwin --version >/dev/null 2>&1; then
    HAS_CARGO_XWIN=true
fi

HAS_GH=false
if command -v gh >/dev/null 2>&1; then
    HAS_GH=true
fi

can_build_index() {
    local i="$1"
    local builder="${TARGET_BUILDERS[$i]}"
    local triple="${TARGET_TRIPLES[$i]}"

    case "$builder" in
        cargo-darwin)
            [[ "$HOST_OS" == "Darwin" ]] || return 1
            rustup target list --installed | grep -qx "$triple" || return 1
            [[ "$triple" == "$HOST_RUST_TARGET" ]] || darwin_cross_fuse_available
            ;;
        docker-linux)
            [[ "$HAS_DOCKER" == true ]] || return 1
            ;;
        xwin-windows)
            [[ "$HAS_CARGO_XWIN" == true ]] || return 1
            command -v zip >/dev/null 2>&1 || return 1
            rustup target list --installed | grep -qx "$triple" || return 1
            PATH="$(xwin_path)" command -v llvm-lib >/dev/null 2>&1 || return 1
            if [[ "$triple" == "aarch64-pc-windows-msvc" ]]; then
                xwin_clang_cl_path >/dev/null || return 1
            fi
            ;;
        *)
            return 1
            ;;
    esac
}

unavailable_reason() {
    local i="$1"
    local builder="${TARGET_BUILDERS[$i]}"
    local triple="${TARGET_TRIPLES[$i]}"

    case "$builder" in
        cargo-darwin)
            if [[ "$HOST_OS" != "Darwin" ]]; then
                echo "requires macOS host"
            elif ! rustup target list --installed | grep -qx "$triple"; then
                echo "requires: rustup target add $triple"
            elif [[ "$triple" != "$HOST_RUST_TARGET" ]] && ! darwin_cross_fuse_available; then
                echo "requires universal macFUSE pkg-config files in /usr/local/lib/pkgconfig, or hosted workflow"
            else
                echo "unknown darwin build prerequisite missing"
            fi
            ;;
        docker-linux)
            if [[ "$HAS_DOCKER" != true ]]; then
                echo "requires Docker daemon"
            else
                echo "requires Docker daemon"
            fi
            ;;
        xwin-windows)
            if [[ "$HAS_CARGO_XWIN" != true ]]; then
                echo "requires: cargo install cargo-xwin --locked"
            elif ! command -v zip >/dev/null 2>&1; then
                echo "requires: zip"
            elif ! rustup target list --installed | grep -qx "$triple"; then
                echo "requires: rustup target add $triple"
            elif ! PATH="$(xwin_path)" command -v llvm-lib >/dev/null 2>&1; then
                echo "requires LLVM tools in PATH, e.g. brew install llvm"
            elif [[ "$triple" == "aarch64-pc-windows-msvc" ]] && ! xwin_clang_cl_path >/dev/null; then
                echo "requires clang-cl for Windows ARM64, e.g. brew install llvm"
            else
                echo "requires cargo-xwin-compatible MSVC cross toolchain"
            fi
            ;;
        *)
            echo "unknown builder: $builder"
            ;;
    esac
}

select_targets() {
    SELECTED_TARGETS=()
    local name i

    if [[ ${#REQUESTED_TARGETS[@]} -gt 0 ]]; then
        for name in "${REQUESTED_TARGETS[@]}"; do
            i="$(target_index "$name")" || die "Unknown target: $name. Run $0 --list-targets."
            SELECTED_TARGETS+=("$i")
        done
    else
        for i in "${!TARGET_NAMES[@]}"; do
            SELECTED_TARGETS+=("$i")
        done
    fi
}

ensure_buildable_targets() {
    BUILD_TARGETS=()
    local missing=()
    local i

    for i in "${SELECTED_TARGETS[@]}"; do
        if can_build_index "$i"; then
            BUILD_TARGETS+=("$i")
        else
            missing+=("$i")
        fi
    done

    if [[ ${#missing[@]} -gt 0 && "$ALLOW_PARTIAL" != true ]]; then
        printf "${RED}error:${RESET} Cannot build the full requested release matrix on this machine:\n" >&2
        for i in "${missing[@]}"; do
            printf "  %-18s %s\n" "${TARGET_NAMES[$i]}" "$(unavailable_reason "$i")" >&2
        done
        printf "\nUse --allow-partial for local smoke builds, or CI for unavailable host dependencies.\n" >&2
        exit 1
    fi

    if [[ ${#BUILD_TARGETS[@]} -eq 0 ]]; then
        die "No selected targets are buildable on this machine."
    fi

    if [[ ${#missing[@]} -gt 0 ]]; then
        for i in "${missing[@]}"; do
            warn "Skipping ${TARGET_NAMES[$i]} - $(unavailable_reason "$i")"
        done
    fi
}

package_unix_binaries() {
    local bin_path="$1"
    local fuse_path="$2"
    local nfs_path="$3"
    local archive="$4"
    tar -czf "$archive" -C "$(dirname "$bin_path")" "$(basename "$bin_path")" "$(basename "$fuse_path")" "$(basename "$nfs_path")"
}

package_darwin_binaries() {
    local bin_path="$1"
    local fuse_path="$2"
    local nfs_path="$3"
    local archive="$4"
    tar -czf "$archive" -C "$(dirname "$bin_path")" "$(basename "$bin_path")" "$(basename "$fuse_path")" "$(basename "$nfs_path")"
}

darwin_pkg_config_path() {
    if [[ -n "${PKG_CONFIG_PATH:-}" ]]; then
        echo "/usr/local/lib/pkgconfig:$PKG_CONFIG_PATH"
    else
        echo "/usr/local/lib/pkgconfig"
    fi
}

darwin_cross_fuse_available() {
    [[ "$HOST_OS" == "Darwin" ]] || return 1
    [[ -d /usr/local/lib/pkgconfig ]] || return 1
    PKG_CONFIG_ALLOW_CROSS=1 \
        PKG_CONFIG_PATH="$(darwin_pkg_config_path)" \
        pkg-config --exists fuse3 || \
        PKG_CONFIG_ALLOW_CROSS=1 \
            PKG_CONFIG_PATH="$(darwin_pkg_config_path)" \
            pkg-config --exists fuse
}

package_windows_binary() {
    local bin_path="$1"
    local nfs_path="$2"
    local archive="$3"
    (cd "$(dirname "$bin_path")" && zip -q "$archive" "$(basename "$bin_path")" "$(basename "$nfs_path")")
}

xwin_path() {
    local clang_cl
    if clang_cl="$(xwin_clang_cl_path 2>/dev/null)"; then
        echo "$(dirname "$clang_cl"):$PATH"
        return 0
    fi
    echo "$PATH"
}

xwin_clang_cl_path() {
    local dir
    for dir in /opt/homebrew/opt/llvm/bin /usr/local/opt/llvm/bin; do
        if [[ -x "$dir/clang-cl" ]]; then
            echo "$dir/clang-cl"
            return 0
        fi
    done
    command -v clang-cl
}

xwin_arm64_clang_wrapper_path() {
    local wrapper_dir="$1"
    local clang_cl
    clang_cl="$(xwin_clang_cl_path)" || return 1
    mkdir -p "$wrapper_dir"
    {
        printf '#!/usr/bin/env bash\n'
        printf 'exec "%s" "$@"\n' "$clang_cl"
    } > "$wrapper_dir/clang"
    chmod +x "$wrapper_dir/clang"
}

build_darwin() {
    local name="$1"
    local triple="$2"
    local archive="$DIST_DIR/crab-${name}.tar.gz"

    if [[ "$triple" != "$HOST_RUST_TARGET" ]]; then
        PKG_CONFIG_ALLOW_CROSS=1 \
            PKG_CONFIG_PATH="$(darwin_pkg_config_path)" \
            "$CARGO" build --release --locked --target "$triple" \
            --manifest-path "$CRAB_DIR/Cargo.toml" \
            -p crab --bin crab \
            --no-default-features --features "$CRAB_CLI_FEATURES_WITH_FUSE"
    else
        "$CARGO" build --release --locked --target "$triple" \
            --manifest-path "$CRAB_DIR/Cargo.toml" \
            -p crab --bin crab \
            --no-default-features --features "$CRAB_CLI_FEATURES_WITH_FUSE"
    fi

    local bin_path="$TARGET_DIR/$triple/release/crab"
    local fuse_path="$TARGET_DIR/$triple/release/crab-fuse-mount"
    local nfs_path="$TARGET_DIR/$triple/release/crab-nfs-mount"
    [[ -f "$bin_path" ]] || die "Binary not found: $bin_path"
    cp "$bin_path" "$fuse_path"

    "$CARGO" build --release --locked --target "$triple" \
        --manifest-path "$CRAB_DIR/Cargo.toml" \
        -p crab --bin crab \
        --no-default-features --features "$CRAB_CLI_FEATURES_NO_FUSE"

    [[ -f "$bin_path" ]] || die "Binary not found: $bin_path"
    ln -sf "$(basename "$bin_path")" "$nfs_path"
    [[ -f "$fuse_path" ]] || die "FUSE mount binary not found: $fuse_path"
    [[ -L "$nfs_path" ]] || die "NFS mount helper symlink not found: $nfs_path"
    package_darwin_binaries "$bin_path" "$fuse_path" "$nfs_path" "$archive"
}

build_linux() {
    local name="$1"
    local image_tag="crab-build-${name}:${VERSION}"
    local archive="$DIST_DIR/crab-${name}.tar.gz"
    local platform
    local docker_job_args=()
    local docker_cross_env=()
    local target_triple=""

    case "$name" in
        linux-aarch64) platform="linux/arm64" ;;
        linux-x86_64) platform="linux/amd64" ;;
        *) die "Unknown linux target: $name" ;;
    esac

    if [[ "$HOST_OS" == "Darwin" && "$HOST_ARCH" == "arm64" && "$name" == "linux-x86_64" ]]; then
        platform="linux/arm64"
        target_triple="x86_64-unknown-linux-gnu"
        docker_job_args=(-e "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-12}")
        docker_cross_env=(
            -e "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=gcc"
            -e "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc"
            -e "CC_aarch64_unknown_linux_gnu=gcc"
            -e "CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc"
            -e "AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar"
            -e "PKG_CONFIG_ALLOW_CROSS=1"
            -e "PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig"
        )
    fi

    "$DOCKER" run --rm \
        --platform "$platform" \
        ${docker_job_args[@]+"${docker_job_args[@]}"} \
        ${docker_cross_env[@]+"${docker_cross_env[@]}"} \
        -e "CRAB_BUILD_VERSION=$VERSION" \
        -e "GIT_SHA=$GIT_SHA" \
        -e "TARGET_TRIPLE=$target_triple" \
        -e "CRAB_CLI_FEATURES_NO_FUSE=$CRAB_CLI_FEATURES_NO_FUSE" \
        -e "CRAB_CLI_FEATURES_WITH_FUSE=$CRAB_CLI_FEATURES_WITH_FUSE" \
        -e "CARGO_TARGET_DIR=/tmp/crab-target" \
        -e "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=cc" \
        -e "CC_aarch64_unknown_linux_gnu=cc" \
        --mount "type=bind,source=$WORKSPACE_DIR,target=/workspace,readonly" \
        --mount "type=bind,source=$DIST_DIR,target=/dist" \
        --mount "type=volume,source=${image_tag//[:\/]/-}-registry,target=/usr/local/cargo/registry" \
        --mount "type=volume,source=${image_tag//[:\/]/-}-git,target=/usr/local/cargo/git" \
        -w /workspace \
        rust:1.93-bookworm \
        bash -c '
            set -euo pipefail
            cargo_target_args=()
            binary_dir="$CARGO_TARGET_DIR/release"
            if [[ -n "$TARGET_TRIPLE" ]]; then
                dpkg --add-architecture amd64
                apt-get update
                DEBIAN_FRONTEND=noninteractive apt-get install -y gcc gcc-x86-64-linux-gnu pkg-config cmake libssl-dev:amd64 libfuse3-dev:amd64
                rustup target add "$TARGET_TRIPLE"
                cargo_target_args=(--target "$TARGET_TRIPLE")
                binary_dir="$CARGO_TARGET_DIR/$TARGET_TRIPLE/release"
            else
                apt-get update
                apt-get install -y pkg-config libssl-dev cmake libfuse3-dev
            fi
            cargo build --release --locked "${cargo_target_args[@]}" --manifest-path crab/Cargo.toml -p crab --bin crab --no-default-features --features "$CRAB_CLI_FEATURES_WITH_FUSE"
            cp "$binary_dir/crab" /dist/crab-fuse-mount
            cargo build --release --locked "${cargo_target_args[@]}" --manifest-path crab/Cargo.toml -p crab --bin crab --no-default-features --features "$CRAB_CLI_FEATURES_NO_FUSE"
            cp "$binary_dir/crab" /dist/crab
        '

    ln -sf crab "$DIST_DIR/crab-nfs-mount"
    package_unix_binaries "$DIST_DIR/crab" "$DIST_DIR/crab-fuse-mount" "$DIST_DIR/crab-nfs-mount" "$archive"
    rm -f "$DIST_DIR/crab" "$DIST_DIR/crab-fuse-mount" "$DIST_DIR/crab-nfs-mount"
}

build_windows() {
    local name="$1"
    local triple="$2"
    local archive="$DIST_DIR/crab-${name}.zip"
    local xwin_env_path
    local wrapper_dir=""

    xwin_env_path="$(xwin_path)"
    if [[ "$triple" == "aarch64-pc-windows-msvc" ]]; then
        wrapper_dir="$(mktemp -d)"
        xwin_arm64_clang_wrapper_path "$wrapper_dir" || die "Unable to create Windows ARM64 clang-cl wrapper"
        xwin_env_path="$wrapper_dir:$xwin_env_path"
        trap '[[ -n "${wrapper_dir:-}" ]] && rm -rf "$wrapper_dir"' RETURN
    fi

    PATH="$xwin_env_path" "$CARGO" xwin build --release --locked --target "$triple" \
        --manifest-path "$CRAB_DIR/Cargo.toml" \
        -p crab --bin crab \
        --no-default-features --features simd-accel,tier,watch,nfs,gix-pathmatch

    PATH="$xwin_env_path" "$CARGO" xwin build --release --locked --target "$triple" \
        --manifest-path "$CRAB_DIR/Cargo.toml" \
        -p crab --bin crab-nfs-mount \
        --no-default-features

    if [[ -n "$wrapper_dir" ]]; then
        rm -rf "$wrapper_dir"
        trap - RETURN
    fi

    local bin_path="$TARGET_DIR/$triple/release/crab.exe"
    local nfs_path="$TARGET_DIR/$triple/release/crab-nfs-mount.exe"
    [[ -f "$bin_path" ]] || die "Binary not found: $bin_path"
    [[ -f "$nfs_path" ]] || die "NFS mount binary not found: $nfs_path"
    package_windows_binary "$bin_path" "$nfs_path" "$archive"
}

build_index() {
    local i="$1"
    local name="${TARGET_NAMES[$i]}"
    local triple="${TARGET_TRIPLES[$i]}"
    local builder="${TARGET_BUILDERS[$i]}"

    info "Building $name ($triple) via $builder"

    case "$builder" in
        cargo-darwin) build_darwin "$name" "$triple" ;;
        docker-linux) build_linux "$name" ;;
        xwin-windows) build_windows "$name" "$triple" ;;
        *) die "Unknown builder: $builder" ;;
    esac

    local archive="$DIST_DIR/crab-${name}.${TARGET_EXTS[$i]}"
    local size
    size="$(du -h "$archive" | cut -f1)"
    info "Created $(basename "$archive") ($size)"
}

verify_replica_release_evidence() {
    if [[ -z "$REPLICA_RELEASE_EVIDENCE_DIR" ]]; then
        die "REPLICA_RELEASE_EVIDENCE_DIR is required before publishing an enterprise replica release."
    fi
    if [[ -z "$REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID" ]]; then
        die "REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID is required before publishing; use replica-live-<github-run-id>-<attempt>."
    fi

    info "Verifying retained enterprise replica evidence"
    "$SCRIPT_DIR/verify-replica-release-evidence.sh" \
        "$REPLICA_RELEASE_EVIDENCE_DIR" \
        "$REPLICA_RELEASE_EVIDENCE_OUTPUT" \
        "$REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID"
}

if [[ "$DRY_RUN" != true ]]; then
    verify_replica_release_evidence
fi

SELECTED_TARGETS=()
BUILD_TARGETS=()
select_targets
ensure_buildable_targets

rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

echo ""
printf "${BOLD}Crab %s - Release Build${RESET}\n" "$TAG"
echo ""
info "Release repo: $RELEASE_REPO"
info "Targets:"
for i in "${BUILD_TARGETS[@]}"; do
    printf "  %-18s %-30s %s\n" \
        "${TARGET_NAMES[$i]}" "${TARGET_TRIPLES[$i]}" "${TARGET_BUILDERS[$i]}"
done
echo ""

BUILT_ARCHIVES=()
for i in "${BUILD_TARGETS[@]}"; do
    build_index "$i"
    BUILT_ARCHIVES+=("$DIST_DIR/crab-${TARGET_NAMES[$i]}.${TARGET_EXTS[$i]}")
    echo ""
done

info "Generating checksums"
(
    cd "$DIST_DIR"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 crab-* > SHA256SUMS.txt
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum crab-* > SHA256SUMS.txt
    else
        die "Neither shasum nor sha256sum is available."
    fi
)
BUILT_ARCHIVES+=("$DIST_DIR/SHA256SUMS.txt")

info "Built ${#BUILD_TARGETS[@]} target artifacts:"
for a in "${BUILT_ARCHIVES[@]}"; do
    echo "  $(basename "$a")"
done
echo ""

if [[ "$DRY_RUN" == true ]]; then
    info "Dry run complete - artifacts in $DIST_DIR"
    cat "$DIST_DIR/SHA256SUMS.txt"
    exit 0
fi

if [[ "$ALLOW_PARTIAL" == true ]]; then
    die "Refusing to publish a partial release. Re-run without --allow-partial."
fi

if [[ "$HAS_GH" != true ]]; then
    die "\`gh\` CLI not found. Install: https://cli.github.com/"
fi

if ! gh auth status >/dev/null 2>&1; then
    die "\`gh\` is not authenticated. Run: gh auth login"
fi

info "Publishing $TAG to $RELEASE_REPO"
if gh release view "$TAG" --repo "$RELEASE_REPO" >/dev/null 2>&1; then
    warn "Release $TAG exists - uploading assets with --clobber."
    gh release upload "$TAG" "${BUILT_ARCHIVES[@]}" \
        --repo "$RELEASE_REPO" \
        --clobber
else
    gh release create "$TAG" "${BUILT_ARCHIVES[@]}" \
        --repo "$RELEASE_REPO" \
        --title "Crab CLI ${TAG}" \
        --generate-notes
fi

echo ""
printf "${GREEN}${BOLD}Released crab %s${RESET}\n" "$TAG"
printf "  https://github.com/%s/releases/tag/%s\n" "$RELEASE_REPO" "$TAG"
