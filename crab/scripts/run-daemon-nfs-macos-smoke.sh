#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$CRAB_DIR/.." && pwd)"
RUN_ID="${CRAB_DAEMON_NFS_SMOKE_RUN_ID:-daemon-nfs-macos-$(date -u +%Y%m%d-%H%M%S)}"
RUN_ROOT="${CRAB_DAEMON_NFS_SMOKE_ROOT:-/tmp/crab-daemon-nfs-macos-smoke}/$RUN_ID"
HOME_DIR="$RUN_ROOT/home"
DAEMON_ROOT="$RUN_ROOT/daemon"
MOUNT_ROOT="$RUN_ROOT/mounts"
SOURCE="$RUN_ROOT/source"
REMOTE="$RUN_ROOT/remote.git"
LOG_DIR="$RUN_ROOT/logs"
CRAB_EXE="$REPO_ROOT/target/debug/crab"
DAEMON_PID=""
HOST_HOME="${HOME:-}"
HOST_CARGO_HOME="${CARGO_HOME:-${HOST_HOME:+$HOST_HOME/.cargo}}"
HOST_RUSTUP_HOME="${RUSTUP_HOME:-${HOST_HOME:+$HOST_HOME/.rustup}}"

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

is_mounted() {
    local mountpoint="$1"
    mount | grep -F " on $mountpoint (" | grep -F '(nfs,' >/dev/null 2>&1
}

wait_for_mount() {
    local mountpoint="$1"
    for _ in $(seq 1 60); do
        if is_mounted "$mountpoint" && [ -f "$mountpoint/hello.txt" ]; then
            return
        fi
        sleep 1
    done
    mount >"$LOG_DIR/mounts-on-timeout.txt" 2>&1 || true
    die "timed out waiting for NFS mount at $mountpoint"
}

wait_for_unmount() {
    local mountpoint="$1"
    for _ in $(seq 1 60); do
        if ! is_mounted "$mountpoint"; then
            return
        fi
        sleep 1
    done
    mount >"$LOG_DIR/mounts-after-stop.txt" 2>&1 || true
    die "NFS mount remained active after daemon shutdown: $mountpoint"
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
    die "timed out waiting for refreshed contents at $path"
}

cleanup() {
    set +e
    if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -TERM "$DAEMON_PID" 2>/dev/null
        wait "$DAEMON_PID" 2>/dev/null
    fi
    for mountpoint in "$MOUNT_ROOT/repo-a" "$MOUNT_ROOT/repo-b"; do
        if is_mounted "$mountpoint"; then
            /sbin/umount "$mountpoint" >/dev/null 2>&1
        fi
        if is_mounted "$mountpoint"; then
            /sbin/umount -f "$mountpoint" >/dev/null 2>&1
        fi
    done
}

[ "$(uname -s)" = Darwin ] || die "this smoke test requires macOS"
command -v cargo >/dev/null 2>&1 || die "cargo is required"
command -v git >/dev/null 2>&1 || die "git is required"
command -v mount_nfs >/dev/null 2>&1 || die "mount_nfs is required"

mkdir -p "$HOME_DIR" "$DAEMON_ROOT" "$MOUNT_ROOT" "$SOURCE" "$LOG_DIR"
RUN_ROOT="$(cd "$RUN_ROOT" && pwd -P)"
HOME_DIR="$RUN_ROOT/home"
DAEMON_ROOT="$RUN_ROOT/daemon"
MOUNT_ROOT="$RUN_ROOT/mounts"
SOURCE="$RUN_ROOT/source"
REMOTE="$RUN_ROOT/remote.git"
LOG_DIR="$RUN_ROOT/logs"
trap cleanup EXIT

export HOME="$HOME_DIR"
export GIT_TERMINAL_PROMPT=0
if [ -n "$HOST_CARGO_HOME" ]; then
    export CARGO_HOME="$HOST_CARGO_HOME"
fi
if [ -n "$HOST_RUSTUP_HOME" ]; then
    export RUSTUP_HOME="$HOST_RUSTUP_HOME"
fi
git config --global user.email daemon-nfs-smoke@crab.local
git config --global user.name 'Crab Daemon NFS Smoke'

cargo build -p crab --bin crab >"$LOG_DIR/cargo-build.log" 2>&1

git -C "$SOURCE" init -b main >"$LOG_DIR/git-init.log" 2>&1
git -C "$SOURCE" config user.email daemon-nfs-smoke@crab.local
git -C "$SOURCE" config user.name 'Crab Daemon NFS Smoke'
printf 'hello\n' >"$SOURCE/hello.txt"
git -C "$SOURCE" add hello.txt
git -C "$SOURCE" commit -m seed >"$LOG_DIR/git-commit-seed.log" 2>&1
git clone --bare "$SOURCE" "$REMOTE" >"$LOG_DIR/git-clone-bare.log" 2>&1

for name in repo-a repo-b; do
    mkdir -p "$MOUNT_ROOT/$name"
    "$CRAB_EXE" mount doctor \
        --backend nfs \
        --mountpoint "$MOUNT_ROOT/$name" \
        --json >"$RUN_ROOT/doctor-$name.json"
    "$CRAB_EXE" daemon \
        --root "$DAEMON_ROOT" \
        add-repo \
        --name "$name" \
        --remote "file://$REMOTE" \
        --branch main \
        --mount-root "$MOUNT_ROOT" \
        --backend nfs
done

"$CRAB_EXE" daemon --root "$DAEMON_ROOT" set-refresh --name repo-b --interval 1

"$CRAB_EXE" daemon --root "$DAEMON_ROOT" >"$LOG_DIR/daemon.log" 2>&1 &
DAEMON_PID="$!"

wait_for_mount "$MOUNT_ROOT/repo-a"
wait_for_mount "$MOUNT_ROOT/repo-b"

[ "$(cat "$MOUNT_ROOT/repo-a/hello.txt")" = hello ] || die "repo-a read mismatch"
[ "$(cat "$MOUNT_ROOT/repo-b/hello.txt")" = hello ] || die "repo-b read mismatch"

printf 'changed through NFS\n' >"$MOUNT_ROOT/repo-a/hello.txt"
printf 'new through NFS\n' >"$MOUNT_ROOT/repo-a/new.txt"
"$CRAB_EXE" daemon \
    --root "$DAEMON_ROOT" \
    commit \
    --name repo-a \
    -m 'commit NFS overlay' \
    --json >"$RUN_ROOT/commit.json"

[ "$(cat "$MOUNT_ROOT/repo-a/hello.txt")" = 'changed through NFS' ] \
    || die "repo-a live view did not adopt committed snapshot"
[ "$(cat "$MOUNT_ROOT/repo-a/new.txt")" = 'new through NFS' ] \
    || die "repo-a committed file is unreadable"

printf 'refreshed from remote\n' >"$SOURCE/hello.txt"
git -C "$SOURCE" add hello.txt
git -C "$SOURCE" commit -m refresh >"$LOG_DIR/git-commit-refresh.log" 2>&1
git -C "$SOURCE" push "file://$REMOTE" main >"$LOG_DIR/git-push-refresh.log" 2>&1
wait_for_text "$MOUNT_ROOT/repo-b/hello.txt" 'refreshed from remote'

"$CRAB_EXE" daemon --root "$DAEMON_ROOT" list --json >"$RUN_ROOT/list.json"
"$CRAB_EXE" daemon --root "$DAEMON_ROOT" status --name repo-a --json \
    >"$RUN_ROOT/status-repo-a.json"

python3 - "$RUN_ROOT/list.json" "$RUN_ROOT/status-repo-a.json" "$RUN_ROOT/commit.json" <<'PY'
import json
import sys

listed = json.load(open(sys.argv[1], encoding="utf-8"))["data"]["repos"]
status = json.load(open(sys.argv[2], encoding="utf-8"))["data"]
commit = json.load(open(sys.argv[3], encoding="utf-8"))["data"]

assert len(listed) == 2, listed
assert all(repo["backend"] == "nfs" for repo in listed), listed
assert all(repo["state"] == "running" for repo in listed), listed
assert status["backend"] == "nfs", status
assert status["state"] == "running", status
assert status["dirty_count"] == 0, status
assert commit["result"]["commit_oid"], commit
assert commit["result"]["overlay_cleaned"], commit
PY

kill -TERM "$DAEMON_PID"
wait "$DAEMON_PID"
DAEMON_PID=""

wait_for_unmount "$MOUNT_ROOT/repo-a"
wait_for_unmount "$MOUNT_ROOT/repo-b"

printf 'daemon NFS smoke passed\n'
printf 'artifacts=%s\n' "$RUN_ROOT"
