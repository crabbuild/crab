#!/usr/bin/env bash
# One-command local Crab Auth E2E against RustFS.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
AUTH_ROOT="$WORKSPACE_ROOT/crab/deploy/auth-service"

RUSTFS_IMAGE="${CRAB_AUTH_RUSTFS_IMAGE:-rustfs/rustfs:1.0.0-beta.8-glibc}"
CONTAINER="${CRAB_AUTH_RUSTFS_CONTAINER:-crab-auth-rustfs-e2e}"
PORT="${CRAB_AUTH_RUSTFS_PORT:-19000}"
BUCKET="${CRAB_AUTH_RUSTFS_BUCKET:-crab}"
REGION="${CRAB_AUTH_S3_REGION:-us-east-1}"
ACCESS_KEY="${CRAB_AUTH_S3_ACCESS_KEY_ID:-crab}"
SECRET_KEY="${CRAB_AUTH_S3_SECRET_ACCESS_KEY:-crab}"
DATA_DIR="${CRAB_AUTH_RUSTFS_DATA_DIR:-$WORKSPACE_ROOT/target/crab-auth-rustfs-data-$PORT}"
VENV_DIR="${CRAB_AUTH_E2E_VENV:-$WORKSPACE_ROOT/target/crab-auth-e2e-venv}"
PYTHON_BIN="${CRAB_AUTH_E2E_PYTHON:-}"

KEEP_CONTAINER=0
SKIP_BUILD=0
SKIP_DEPS=0

usage() {
    cat <<EOF
Usage: crab/deploy/auth-service/scripts/e2e-rustfs-docker.sh [options]

Runs a local end-to-end Crab Auth verification:
  Docker RustFS -> local JWKS -> local Crab Auth -> crab CLI push/clone/hydrate.

Options:
  --keep-container   Leave the RustFS container running after the test.
  --skip-build       Do not build crab and crab-auth helper binaries first.
  --skip-deps        Do not install Python dependencies into the local venv.
  --port PORT        Host port for RustFS. Default: $PORT.
  --help             Show this help.

Environment overrides:
  CRAB_AUTH_RUSTFS_IMAGE
  CRAB_AUTH_RUSTFS_CONTAINER
  CRAB_AUTH_RUSTFS_BUCKET
  CRAB_AUTH_RUSTFS_DATA_DIR
  CRAB_AUTH_E2E_VENV
  CRAB_AUTH_E2E_PYTHON
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --keep-container)
            KEEP_CONTAINER=1
            ;;
        --skip-build)
            SKIP_BUILD=1
            ;;
        --skip-deps)
            SKIP_DEPS=1
            ;;
        --port)
            if [ "$#" -lt 2 ]; then
                echo "missing value for --port" >&2
                exit 2
            fi
            PORT="$2"
            DATA_DIR="${CRAB_AUTH_RUSTFS_DATA_DIR:-$WORKSPACE_ROOT/target/crab-auth-rustfs-data-$PORT}"
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

need_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "missing required command: $1" >&2
        exit 1
    fi
}

cleanup() {
    if [ "$KEEP_CONTAINER" -eq 0 ]; then
        docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

need_command cargo
need_command curl
need_command docker
need_command git

python_supported() {
    "$1" - <<'PY'
import sys
version = sys.version_info[:2]
raise SystemExit(0 if (3, 11) <= version <= (3, 13) else 1)
PY
}

select_python() {
    if [ -n "$PYTHON_BIN" ]; then
        if ! command -v "$PYTHON_BIN" >/dev/null 2>&1; then
            echo "configured CRAB_AUTH_E2E_PYTHON is not executable: $PYTHON_BIN" >&2
            exit 1
        fi
        if ! python_supported "$PYTHON_BIN"; then
            echo "CRAB_AUTH_E2E_PYTHON must be Python 3.11, 3.12, or 3.13." >&2
            exit 1
        fi
        return
    fi

    for candidate in python3.12 python3.11 python3.13 python3; do
        if command -v "$candidate" >/dev/null 2>&1 && python_supported "$candidate"; then
            PYTHON_BIN="$candidate"
            return
        fi
    done

    echo "missing supported Python. Install Python 3.11, 3.12, or 3.13, or set CRAB_AUTH_E2E_PYTHON." >&2
    exit 1
}

select_python

if ! docker info >/dev/null 2>&1; then
    echo "Docker is not running or is not reachable." >&2
    exit 1
fi

if [ "$SKIP_BUILD" -eq 0 ]; then
    echo "Building crab and crab-auth helper binaries..."
    cargo build \
        --manifest-path "$WORKSPACE_ROOT/Cargo.toml" \
        -p crab \
        -p crab-auth-server \
        --bins \
        --no-default-features
fi

if [ -x "$VENV_DIR/bin/python" ] && ! python_supported "$VENV_DIR/bin/python"; then
    echo "Recreating Python venv because it uses an unsupported interpreter..."
    rm -rf "$VENV_DIR"
fi

if [ ! -x "$VENV_DIR/bin/python" ]; then
    echo "Creating Python venv for Crab Auth E2E..."
    "$PYTHON_BIN" -m venv "$VENV_DIR"
fi

if [ "$SKIP_DEPS" -eq 0 ]; then
    echo "Installing Crab Auth Python dependencies..."
    "$VENV_DIR/bin/python" -m pip install --upgrade pip >/dev/null
    "$VENV_DIR/bin/python" -m pip install -r "$AUTH_ROOT/requirements.txt" >/dev/null
fi

echo "Starting RustFS on http://127.0.0.1:$PORT..."
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
mkdir -p "$DATA_DIR"
docker run -d \
    --name "$CONTAINER" \
    -p "127.0.0.1:$PORT:9000" \
    -e RUSTFS_ACCESS_KEY="$ACCESS_KEY" \
    -e RUSTFS_SECRET_KEY="$SECRET_KEY" \
    -v "$DATA_DIR:/data" \
    "$RUSTFS_IMAGE" >/dev/null

deadline=$((SECONDS + 45))
while [ "$SECONDS" -lt "$deadline" ]; do
    status="$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$PORT" || true)"
    if [ "$status" = "200" ] || [ "$status" = "403" ]; then
        break
    fi
    sleep 1
done

if [ "${status:-000}" != "200" ] && [ "${status:-000}" != "403" ]; then
    echo "RustFS did not become ready on port $PORT." >&2
    docker logs "$CONTAINER" >&2 || true
    exit 1
fi

echo "Running Crab Auth RustFS path-ACL E2E..."
CRAB_AUTH_RUSTFS_ENDPOINT="http://127.0.0.1:$PORT" \
CRAB_AUTH_RUSTFS_BUCKET="$BUCKET" \
CRAB_AUTH_S3_ACCESS_KEY_ID="$ACCESS_KEY" \
CRAB_AUTH_S3_SECRET_ACCESS_KEY="$SECRET_KEY" \
CRAB_AUTH_S3_REGION="$REGION" \
    "$VENV_DIR/bin/python" "$AUTH_ROOT/scripts/e2e-path-acl-rustfs.py"
