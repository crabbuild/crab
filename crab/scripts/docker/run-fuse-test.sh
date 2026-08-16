#!/usr/bin/env bash
#
# run-fuse-test.sh — Build and run the FUSE integration test container.
#
# Run from the workspace root:
#   ./crab/scripts/docker/run-fuse-test.sh
#
# Or with custom docker build args:
#   ./crab/scripts/docker/run-fuse-test.sh --no-cache

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
IMAGE_NAME="crab-fuse-test"

echo "Building $IMAGE_NAME..."
docker build \
    -f "$SCRIPT_DIR/Dockerfile.fuse-test" \
    -t "$IMAGE_NAME" \
    "$@" \
    "$WORKSPACE_ROOT"

echo ""
echo "Running FUSE smoke test..."
docker run --rm \
    --cap-add SYS_ADMIN \
    --device /dev/fuse \
    "$IMAGE_NAME"
