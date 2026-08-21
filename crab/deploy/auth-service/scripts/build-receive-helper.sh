#!/usr/bin/env bash
set -euo pipefail

CRAB_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
WORKSPACE_ROOT="$(cd "$CRAB_ROOT/.." && pwd)"
AUTH_DIR="$CRAB_ROOT/deploy/auth-service"
TARGET_DIR="${CARGO_TARGET_DIR:-$WORKSPACE_ROOT/target}"
MODE="${1:---host}"
DOCKER_CONTAINER=""

cleanup() {
    if [[ -n "$DOCKER_CONTAINER" ]]; then
        docker rm "$DOCKER_CONTAINER" >/dev/null 2>&1 || true
    fi
}

trap cleanup EXIT

usage() {
    cat <<'EOF'
Usage: scripts/build-receive-helper.sh [--host|--linux-amd64|--linux-arm64]

  --host         Build helpers for the current machine. Use for local Python testing.
  --linux-amd64 Build helpers in Docker for AWS Lambda x86_64 zip packaging.
  --linux-arm64 Build helpers in Docker for AWS Lambda arm64 zip packaging.

Docker and Cloud Run image builds do not need this script; their Dockerfiles
compile crab-auth helpers inside the Linux image.
EOF
}

copy_helpers() {
    local source_dir="$1"
    mkdir -p "$AUTH_DIR/bin"
    cp "$source_dir/crab-auth-receive" "$AUTH_DIR/bin/crab-auth-receive"
    cp "$source_dir/crab-auth-view" "$AUTH_DIR/bin/crab-auth-view"
    chmod 0755 "$AUTH_DIR/bin/crab-auth-receive"
    chmod 0755 "$AUTH_DIR/bin/crab-auth-view"
}

build_host() {
    cargo build \
        --manifest-path "$WORKSPACE_ROOT/Cargo.toml" \
        -p crab-auth-server \
        --release \
        --bin crab-auth-receive \
        --bin crab-auth-view \
        --no-default-features
    copy_helpers "$TARGET_DIR/release"
}

build_linux() {
    local platform="$1"
    local image="crab-auth-receive-builder:${platform//\//-}"

    docker build \
        --platform "$platform" \
        --target receive-helper \
        -f "$AUTH_DIR/Dockerfile" \
        -t "$image" \
        "$WORKSPACE_ROOT"

    DOCKER_CONTAINER="$(docker create "$image")"
    mkdir -p "$AUTH_DIR/bin"
    docker cp "$DOCKER_CONTAINER:/workspace/target/release/crab-auth-receive" "$AUTH_DIR/bin/crab-auth-receive"
    docker cp "$DOCKER_CONTAINER:/workspace/target/release/crab-auth-view" "$AUTH_DIR/bin/crab-auth-view"
    docker rm "$DOCKER_CONTAINER" >/dev/null
    DOCKER_CONTAINER=""
    chmod 0755 "$AUTH_DIR/bin/crab-auth-receive"
    chmod 0755 "$AUTH_DIR/bin/crab-auth-view"
}

case "$MODE" in
    --host|host)
        build_host
        ;;
    --linux-amd64|linux-amd64|--linux|linux)
        build_linux "linux/amd64"
        ;;
    --linux-arm64|linux-arm64)
        build_linux "linux/arm64"
        ;;
    -h|--help|help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac

mkdir -p "$AUTH_DIR/bin"
echo "Wrote $AUTH_DIR/bin/crab-auth-receive and $AUTH_DIR/bin/crab-auth-view"
