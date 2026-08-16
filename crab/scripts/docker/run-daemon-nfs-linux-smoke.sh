#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
DOCKER="${DOCKER:-docker}"
AWS="${AWS:-aws}"
RUN_ID="${CRAB_DAEMON_NFS_SMOKE_RUN_ID:-daemon-nfs-linux-$(date -u +%Y%m%d-%H%M%S)}"
ARTIFACT_ROOT="${CRAB_DAEMON_NFS_SMOKE_ROOT:-/tmp/crab-daemon-nfs-linux-smoke}"
CACHE_ROOT="${CRAB_DAEMON_NFS_SMOKE_CACHE_ROOT:-$ARTIFACT_ROOT/cache}"
RUN_ROOT="$ARTIFACT_ROOT/$RUN_ID"
HOST_TARGET="${CRAB_DAEMON_NFS_SMOKE_TARGET_CACHE:-$CACHE_ROOT/target}"
HOST_CARGO="${CRAB_DAEMON_NFS_SMOKE_CARGO_CACHE:-$CACHE_ROOT/cargo}"
RUST_IMAGE="${CRAB_DAEMON_NFS_SMOKE_RUST_IMAGE:-rust:1.91-bookworm}"
RUSTFS_IMAGE="${CRAB_DAEMON_NFS_SMOKE_RUSTFS_IMAGE:-rustfs/rustfs:1.0.0-beta.8-glibc}"
BUCKET="${CRAB_DAEMON_NFS_SMOKE_BUCKET:-crab}"
REGION="${AWS_REGION:-us-east-1}"
EXTERNAL_ENDPOINT="${CRAB_DAEMON_NFS_EXTERNAL_ENDPOINT:-}"
EXTERNAL_HOST_ENDPOINT="${CRAB_DAEMON_NFS_EXTERNAL_HOST_ENDPOINT:-${AWS_ENDPOINT_URL:-}}"
S3_ACCESS_KEY="${AWS_ACCESS_KEY_ID:-crab}"
S3_SECRET_KEY="${AWS_SECRET_ACCESS_KEY:-crab}"
NETWORK="net-$RUN_ID"
RUSTFS="rustfs-$RUN_ID"
RUNNER="crab-daemon-nfs-smoke-$RUN_ID"

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    "$DOCKER" rm -f "$RUNNER" >/dev/null 2>&1 || true
    "$DOCKER" rm -f "$RUSTFS" >/dev/null 2>&1 || true
    "$DOCKER" network rm "$NETWORK" >/dev/null 2>&1 || true
}
trap cleanup EXIT

command -v "$DOCKER" >/dev/null 2>&1 || die "docker is required"
command -v "$AWS" >/dev/null 2>&1 || die "aws CLI is required"
"$DOCKER" info >/dev/null 2>&1 || die "Docker daemon is not running"
mkdir -p "$RUN_ROOT" "$HOST_TARGET" "$HOST_CARGO"

"$DOCKER" network create "$NETWORK" >/dev/null
if [ -n "$EXTERNAL_ENDPOINT" ]; then
    [ -n "$EXTERNAL_HOST_ENDPOINT" ] || die "external RustFS requires CRAB_DAEMON_NFS_EXTERNAL_HOST_ENDPOINT"
    RUNNER_ENDPOINT="$EXTERNAL_ENDPOINT"
    HOST_ENDPOINT="$EXTERNAL_HOST_ENDPOINT"
else
    "$DOCKER" run -d \
        --name "$RUSTFS" \
        --network "$NETWORK" \
        --network-alias rustfs \
        -p 127.0.0.1::9000 \
        -e RUSTFS_ACCESS_KEY=crab \
        -e RUSTFS_SECRET_KEY=crab \
        "$RUSTFS_IMAGE" >/dev/null

    HOST_PORT=
    for _ in $(seq 1 60); do
        HOST_PORT="$("$DOCKER" port "$RUSTFS" 9000/tcp 2>/dev/null | sed -E 's/.*:([0-9]+)$/\1/' | head -n1 || true)"
        if [ -n "$HOST_PORT" ] && \
            AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab AWS_DEFAULT_REGION="$REGION" \
            AWS_EC2_METADATA_DISABLED=true "$AWS" \
            --endpoint-url "http://127.0.0.1:$HOST_PORT" s3api list-buckets >/dev/null 2>&1; then
            break
        fi
        sleep 1
    done
    [ -n "$HOST_PORT" ] || die "RustFS did not publish a host port"
    HOST_ENDPOINT="http://127.0.0.1:$HOST_PORT"
    RUNNER_ENDPOINT=http://rustfs:9000
fi

AWS_ACCESS_KEY_ID="$S3_ACCESS_KEY" AWS_SECRET_ACCESS_KEY="$S3_SECRET_KEY" AWS_DEFAULT_REGION="$REGION" \
    AWS_EC2_METADATA_DISABLED=true "$AWS" \
    --endpoint-url "$HOST_ENDPOINT" s3api create-bucket \
    --bucket "$BUCKET" >/dev/null 2>&1 || true
AWS_ACCESS_KEY_ID="$S3_ACCESS_KEY" AWS_SECRET_ACCESS_KEY="$S3_SECRET_KEY" AWS_DEFAULT_REGION="$REGION" \
    AWS_EC2_METADATA_DISABLED=true "$AWS" \
    --endpoint-url "$HOST_ENDPOINT" s3api head-bucket \
    --bucket "$BUCKET" >/dev/null

printf 'run_id=%s\n' "$RUN_ID"
printf 'artifact_root=%s\n' "$RUN_ROOT"

"$DOCKER" run --rm -i \
    --name "$RUNNER" \
    --network "$NETWORK" \
    --cap-add SYS_ADMIN \
    --security-opt apparmor:unconfined \
    -v "$REPO_ROOT:/src" \
    -v "$HOST_TARGET:/src/target" \
    -v "$HOST_CARGO:/cargo" \
    -v "$RUN_ROOT:/e2e" \
    -e CARGO_HOME=/cargo \
    -e AWS_ACCESS_KEY_ID="$S3_ACCESS_KEY" \
    -e AWS_SECRET_ACCESS_KEY="$S3_SECRET_KEY" \
    -e AWS_DEFAULT_REGION="$REGION" \
    -e AWS_REGION="$REGION" \
    -e AWS_ENDPOINT_URL="$RUNNER_ENDPOINT" \
    -e AWS_ENDPOINT_URL_S3="$RUNNER_ENDPOINT" \
    -e AWS_ALLOW_HTTP=true \
    -e AWS_EC2_METADATA_DISABLED=true \
    -e AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false \
    -e VIRTUAL_HOSTED_STYLE_REQUEST=false \
    -e CRAB_DAEMON_NFS_SMOKE_RUN_ID="$RUN_ID" \
    -e CRAB_DAEMON_NFS_SMOKE_BUCKET="$BUCKET" \
    "$RUST_IMAGE" bash -s <<'INNER'
set -euo pipefail

export HOME=/e2e/home
export GIT_TERMINAL_PROMPT=0
export PATH=/src/target/debug:$PATH

HOST_TRIPLE="$(rustc -vV | awk '/^host:/ { print $2 }')"
case "$HOST_TRIPLE" in
    aarch64-unknown-linux-gnu)
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=cc
        ;;
    x86_64-unknown-linux-gnu)
        export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=cc
        ;;
esac

DAEMON_ROOT=/e2e/daemon
MOUNT_ROOT=/e2e/mounts
SOURCE=/e2e/source
REMOTE_URL="crab://$CRAB_DAEMON_NFS_SMOKE_BUCKET/daemon-nfs/$CRAB_DAEMON_NFS_SMOKE_RUN_ID"
LOG_ROOT=/e2e/logs
DAEMON_PID=

is_mounted() {
    mountpoint -q "$1"
}

wait_for_mount() {
    local mountpoint="$1"
    for _ in $(seq 1 60); do
        if is_mounted "$mountpoint" && [ -f "$mountpoint/hello.txt" ]; then
            return
        fi
        sleep 1
    done
    cat /proc/mounts >"$LOG_ROOT/mounts-on-timeout.txt"
    printf 'timed out waiting for %s\n' "$mountpoint" >&2
    exit 1
}

wait_for_unmount() {
    local mountpoint="$1"
    for _ in $(seq 1 60); do
        if ! is_mounted "$mountpoint"; then
            return
        fi
        sleep 1
    done
    cat /proc/mounts >"$LOG_ROOT/mounts-after-stop.txt"
    printf 'mount remained active after daemon shutdown: %s\n' "$mountpoint" >&2
    exit 1
}

wait_for_text() {
    local path="$1"
    local expected="$2"
    for _ in $(seq 1 60); do
        if [ -f "$path" ] && [ "$(cat "$path")" = "$expected" ]; then
            return
        fi
        sleep 1
    done
    printf 'timed out waiting for refreshed contents at %s\n' "$path" >&2
    exit 1
}

cleanup_inner() {
    set +e
    if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID"
        wait "$DAEMON_PID"
    fi
    for mountpoint in "$MOUNT_ROOT/repo-a" "$MOUNT_ROOT/repo-b"; do
        umount "$mountpoint" >/dev/null 2>&1 || true
        umount -l "$mountpoint" >/dev/null 2>&1 || true
    done
}
trap cleanup_inner EXIT

export DEBIAN_FRONTEND=noninteractive
mkdir -p "$HOME" "$DAEMON_ROOT" "$MOUNT_ROOT" "$SOURCE" "$LOG_ROOT"
apt-get update >"$LOG_ROOT/apt-update.log"
apt-get install -y --no-install-recommends \
    ca-certificates git nfs-common pkg-config procps python3 util-linux \
    >"$LOG_ROOT/apt-install.log"

cd /src/crab
cargo build -p crab --bin crab --no-default-features --features nfs \
    >"$LOG_ROOT/cargo-build-nfs.log" 2>&1
ln -sf crab /src/target/debug/crab-nfs-mount
ln -sf crab /src/target/debug/git-remote-crab

git config --global user.email daemon-nfs-smoke@crab.local
git config --global user.name 'Crab Daemon NFS Smoke'
git -C "$SOURCE" init -b main >"$LOG_ROOT/git-init.log"
cd "$SOURCE"
crab init "$REMOTE_URL" >"$LOG_ROOT/crab-init.log" 2>&1
printf 'hello\n' >"$SOURCE/hello.txt"
git -C "$SOURCE" add hello.txt
git -C "$SOURCE" commit -m seed >"$LOG_ROOT/git-commit-seed.log"
crab push --json origin HEAD:refs/heads/main >"$LOG_ROOT/crab-push-seed.json"

for name in repo-a repo-b; do
    mkdir -p "$MOUNT_ROOT/$name"
    crab mount doctor --backend nfs --mountpoint "$MOUNT_ROOT/$name" --json \
        >"/e2e/doctor-$name.json"
    crab daemon --root "$DAEMON_ROOT" add-repo \
        --name "$name" \
        --remote "$REMOTE_URL" \
        --branch main \
        --mount-root "$MOUNT_ROOT" \
        --backend nfs
done
crab daemon --root "$DAEMON_ROOT" set-refresh --name repo-a --interval 1
crab daemon --root "$DAEMON_ROOT" set-refresh --name repo-b --interval 1

crab daemon --root "$DAEMON_ROOT" >"$LOG_ROOT/daemon.log" 2>&1 &
DAEMON_PID="$!"
wait_for_mount "$MOUNT_ROOT/repo-a"
wait_for_mount "$MOUNT_ROOT/repo-b"

[ "$(cat "$MOUNT_ROOT/repo-a/hello.txt")" = hello ]
[ "$(cat "$MOUNT_ROOT/repo-b/hello.txt")" = hello ]
python3 - "$MOUNT_ROOT/repo-a/hello.txt" "$MOUNT_ROOT/repo-b/hello.txt" <<'PY'
import sys

for path in sys.argv[1:]:
    data = open(path, "rb").read()
    assert data == b"hello\n", (path, len(data), data[:32])
PY
grep -F "127.0.0.1:/crab $MOUNT_ROOT/repo-a nfs" /proc/mounts \
    >"$LOG_ROOT/repo-a-mount.txt"
grep -F "127.0.0.1:/crab $MOUNT_ROOT/repo-b nfs" /proc/mounts \
    >"$LOG_ROOT/repo-b-mount.txt"

printf 'changed through Linux NFS\n' >"$MOUNT_ROOT/repo-a/hello.txt"
printf 'new through Linux NFS\n' >"$MOUNT_ROOT/repo-a/new.txt"
crab daemon --root "$DAEMON_ROOT" status --name repo-a --json \
    >"/e2e/status-dirty.json"
kill -TERM "$DAEMON_PID"
wait "$DAEMON_PID"
DAEMON_PID=
wait_for_unmount "$MOUNT_ROOT/repo-a"
wait_for_unmount "$MOUNT_ROOT/repo-b"

crab daemon --root "$DAEMON_ROOT" >"$LOG_ROOT/daemon-restart.log" 2>&1 &
DAEMON_PID="$!"
wait_for_mount "$MOUNT_ROOT/repo-a"
wait_for_mount "$MOUNT_ROOT/repo-b"
crab daemon --root "$DAEMON_ROOT" status --name repo-a --json \
    >"/e2e/status-dirty-after-restart.json"
[ "$(cat "$MOUNT_ROOT/repo-a/hello.txt")" = 'changed through Linux NFS' ]
[ "$(cat "$MOUNT_ROOT/repo-a/new.txt")" = 'new through Linux NFS' ]

crab daemon --root "$DAEMON_ROOT" commit --name repo-a \
    -m 'commit Linux NFS overlay' --json >"/e2e/commit-local.json"
sleep 3
[ "$(cat "$MOUNT_ROOT/repo-a/hello.txt")" = 'changed through Linux NFS' ]
[ "$(cat "$MOUNT_ROOT/repo-a/new.txt")" = 'new through Linux NFS' ]
python3 - "$MOUNT_ROOT/repo-a/hello.txt" "$MOUNT_ROOT/repo-a/new.txt" <<'PY'
import sys

expected = [b"changed through Linux NFS\n", b"new through Linux NFS\n"]
for path, content in zip(sys.argv[1:], expected):
    data = open(path, "rb").read()
    assert data == content, (path, len(data), data[:64])
PY
crab daemon --root "$DAEMON_ROOT" commit --name repo-a \
    -m 'push existing Linux NFS commit' --push --json >"/e2e/commit-push.json"

git -C "$SOURCE" fetch origin main >"$LOG_ROOT/git-fetch-after-daemon-commit.log" 2>&1
git -C "$SOURCE" reset --hard origin/main >"$LOG_ROOT/git-reset-after-daemon-commit.log"
printf 'refreshed from RustFS\n' >"$SOURCE/hello.txt"
git -C "$SOURCE" add hello.txt
git -C "$SOURCE" commit -m refresh >"$LOG_ROOT/git-commit-refresh.log"
cd "$SOURCE"
crab push --json origin HEAD:refs/heads/main >"$LOG_ROOT/crab-push-refresh.json"
wait_for_text "$MOUNT_ROOT/repo-b/hello.txt" 'refreshed from RustFS'

crab daemon --root "$DAEMON_ROOT" list --json >"/e2e/list.json"
crab daemon --root "$DAEMON_ROOT" status --name repo-a --json \
    >"/e2e/status-repo-a.json"
python3 - /e2e/list.json /e2e/status-repo-a.json /e2e/status-dirty.json \
    /e2e/status-dirty-after-restart.json /e2e/commit-local.json \
    /e2e/commit-push.json <<'PY'
import json
import sys

listed = json.load(open(sys.argv[1], encoding="utf-8"))["data"]["repos"]
status = json.load(open(sys.argv[2], encoding="utf-8"))["data"]
dirty = json.load(open(sys.argv[3], encoding="utf-8"))["data"]
dirty_after_restart = json.load(open(sys.argv[4], encoding="utf-8"))["data"]
local_commit = json.load(open(sys.argv[5], encoding="utf-8"))["data"]
pushed_commit = json.load(open(sys.argv[6], encoding="utf-8"))["data"]

assert len(listed) == 2, listed
assert all(repo["backend"] == "nfs" for repo in listed), listed
assert all(repo["state"] == "running" for repo in listed), listed
assert status["backend"] == "nfs", status
assert status["state"] == "running", status
assert status["is_live"], status
assert status["dirty_count"] == 0, status
assert status["dirty_paths"] == [], status
assert dirty["is_live"], dirty
assert dirty["dirty_count"] == 2, dirty
assert dirty["dirty_paths"] == ["hello.txt", "new.txt"], dirty
assert dirty_after_restart["is_live"], dirty_after_restart
assert dirty_after_restart["dirty_count"] == 2, dirty_after_restart
assert dirty_after_restart["dirty_paths"] == ["hello.txt", "new.txt"], dirty_after_restart
assert local_commit["result"]["commit_oid"], local_commit
assert not local_commit["result"]["pushed"], local_commit
assert local_commit["result"]["overlay_cleaned"], local_commit
assert pushed_commit["result"]["commit_oid"] == local_commit["result"]["commit_oid"], pushed_commit
assert pushed_commit["result"]["pushed"], pushed_commit
PY

kill -TERM "$DAEMON_PID"
wait "$DAEMON_PID"
DAEMON_PID=
wait_for_unmount "$MOUNT_ROOT/repo-a"
wait_for_unmount "$MOUNT_ROOT/repo-b"
if find "$DAEMON_ROOT/repos" -name nfs-control-endpoint -print -quit | grep -q .; then
    printf 'daemon retained an NFS control endpoint after shutdown\n' >&2
    exit 1
fi

crab clone --eager "$REMOTE_URL" /e2e/fresh >"$LOG_ROOT/crab-clone-fresh.log" 2>&1
[ "$(cat /e2e/fresh/hello.txt)" = 'refreshed from RustFS' ]
[ "$(cat /e2e/fresh/new.txt)" = 'new through Linux NFS' ]

python3 - /e2e/report.json <<'PY'
import json
import platform
import sys

json.dump(
    {
        "backend": "nfs",
        "daemon_repos": 2,
        "exact_file_sizes": True,
        "kernel": platform.release(),
        "native_mount": True,
        "commit": True,
        "clean_overlay_push": True,
        "object_store": "rustfs",
        "overlay_restart_persistence": True,
        "push": True,
        "refresh": True,
        "unpushed_commit_preserved": True,
        "fresh_clone": True,
        "clean_shutdown": True,
    },
    open(sys.argv[1], "w", encoding="utf-8"),
    indent=2,
)
PY

printf 'Linux daemon NFS smoke passed\n'
INNER

printf 'artifacts=%s\n' "$RUN_ROOT"
