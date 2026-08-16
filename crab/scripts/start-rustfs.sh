#!/usr/bin/env bash
# Start a persistent local RustFS container for Crab E2E and development.
# Usage: crab/scripts/start-rustfs.sh [self-test]

set -euo pipefail

IMAGE="rustfs/rustfs:latest"
CONTAINER="rustfs"
DATA_DIR="/Volumes/Workspace/CrabData"
ENDPOINT_URL="http://127.0.0.1:9000"
BUCKET="crab"

need_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: missing required command: $1" >&2
        exit 1
    fi
}

self_test() {
    local script_dir
    script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
    local script_path="$script_dir/$(basename -- "${BASH_SOURCE[0]}")"
    local test_root
    test_root="$(mktemp -d)"
    local fake_bin="$test_root/bin"
    local call_log="$test_root/calls.log"
    local bucket_state="$test_root/bucket-created"

    self_test_cleanup() {
        local status="$?"
        trap - EXIT
        if ((status != 0)); then
            local file
            for file in "$call_log" "$test_root/stdout.log" "$test_root/stderr.log"; do
                [[ ! -f "$file" ]] || sed -n '1,120p' "$file" >&2
            done
        fi
        rm -rf "$test_root"
        exit "$status"
    }
    trap self_test_cleanup EXIT

    mkdir -p "$fake_bin"

    cat >"$fake_bin/docker" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'docker' >>"$CALL_LOG"
printf ' %s' "$@" >>"$CALL_LOG"
printf '\n' >>"$CALL_LOG"

case "${1:-}" in
    info|pull|rm)
        exit 0
        ;;
    container)
        [[ "${2:-}" == inspect && "${3:-}" == rustfs ]]
        ;;
    run)
        printf 'rustfs-container-id\n'
        ;;
    logs)
        printf 'fake rustfs logs\n'
        ;;
esac
EOF

    cat >"$fake_bin/aws" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'aws credentials=%s/%s region=%s' \
    "${AWS_ACCESS_KEY_ID:-}" \
    "${AWS_SECRET_ACCESS_KEY:-}" \
    "${AWS_DEFAULT_REGION:-}" >>"$CALL_LOG"
printf ' %s' "$@" >>"$CALL_LOG"
printf '\n' >>"$CALL_LOG"

case " $* " in
    *' s3api head-bucket '* )
        [[ -f "$BUCKET_STATE" ]]
        ;;
    *' s3api create-bucket '* )
        touch "$BUCKET_STATE"
        ;;
esac
EOF

    chmod +x "$fake_bin/docker" "$fake_bin/aws"

    PATH="$fake_bin:$PATH" \
    CALL_LOG="$call_log" \
    BUCKET_STATE="$bucket_state" \
        "$script_path" >"$test_root/stdout.log" 2>"$test_root/stderr.log"

    grep -Fxq 'docker pull rustfs/rustfs:latest' "$call_log"
    grep -Fxq 'docker rm -f rustfs' "$call_log"
    grep -Fxq \
        'docker run -d --name rustfs -p 9000:9000 -p 9001:9001 -e RUSTFS_ACCESS_KEY=crab -e RUSTFS_SECRET_KEY=crab -e RUSTFS_CONSOLE_ENABLE=true -v /Volumes/Workspace/CrabData:/data rustfs/rustfs:latest /data' \
        "$call_log"
    grep -Fxq \
        'aws credentials=crab/crab region=us-east-1 --endpoint-url http://127.0.0.1:9000 s3api list-buckets' \
        "$call_log"
    grep -Fxq \
        'aws credentials=crab/crab region=us-east-1 --endpoint-url http://127.0.0.1:9000 s3api create-bucket --bucket crab' \
        "$call_log"
    grep -Fxq \
        'aws credentials=crab/crab region=us-east-1 --endpoint-url http://127.0.0.1:9000 s3api head-bucket --bucket crab' \
        "$call_log"
    grep -Fxq 'RustFS is ready at http://127.0.0.1:9000 with bucket crab.' "$test_root/stdout.log"

    trap - EXIT
    rm -rf "$test_root"
    echo "ok: RustFS launcher self-test passed"
}

if [[ "${1:-}" == self-test ]]; then
    self_test
    exit 0
fi

need_command aws
need_command docker

if ! docker info >/dev/null 2>&1; then
    echo "error: Docker is not running or is not reachable." >&2
    exit 1
fi

mkdir -p "$DATA_DIR"

echo "Pulling $IMAGE..."
docker pull "$IMAGE"

if docker container inspect "$CONTAINER" >/dev/null 2>&1; then
    echo "Replacing existing $CONTAINER container; data remains in $DATA_DIR."
    docker rm -f "$CONTAINER" >/dev/null
fi

echo "Starting $CONTAINER with data from $DATA_DIR..."
docker run -d \
    --name "$CONTAINER" \
    -p 9000:9000 \
    -p 9001:9001 \
    -e RUSTFS_ACCESS_KEY=crab \
    -e RUSTFS_SECRET_KEY=crab \
    -e RUSTFS_CONSOLE_ENABLE=true \
    -v "$DATA_DIR:/data" \
    "$IMAGE" \
    /data >/dev/null

export AWS_ACCESS_KEY_ID=crab
export AWS_SECRET_ACCESS_KEY=crab
export AWS_REGION=us-east-1
export AWS_DEFAULT_REGION=us-east-1
export AWS_EC2_METADATA_DISABLED=true

ready=false
for _ in {1..60}; do
    if aws --endpoint-url "$ENDPOINT_URL" s3api list-buckets >/dev/null 2>&1; then
        ready=true
        break
    fi
    sleep 1
done

if [[ "$ready" != true ]]; then
    docker logs "$CONTAINER" >&2 || true
    echo "error: RustFS did not become ready at $ENDPOINT_URL." >&2
    exit 1
fi

if ! aws --endpoint-url "$ENDPOINT_URL" s3api head-bucket --bucket "$BUCKET" >/dev/null 2>&1; then
    aws --endpoint-url "$ENDPOINT_URL" s3api create-bucket --bucket "$BUCKET" >/dev/null
fi
aws --endpoint-url "$ENDPOINT_URL" s3api head-bucket --bucket "$BUCKET" >/dev/null

echo "RustFS is ready at $ENDPOINT_URL with bucket $BUCKET."
echo "Console: http://127.0.0.1:9001 (credentials: crab/crab)"
