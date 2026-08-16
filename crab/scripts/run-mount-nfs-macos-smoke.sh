#!/usr/bin/env bash
#
# Run a native macOS NFS smoke for `crab mount --backend nfs`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$CRAB_DIR/.." && pwd)"

RUN_ID="${CRAB_NFS_SMOKE_RUN_ID:-mount-nfs-macos-$(date -u +%Y%m%d-%H%M%S)}"
ARTIFACT_ROOT="${CRAB_NFS_SMOKE_ROOT:-/tmp/crab-mount-nfs-macos-smoke}"
RUN_ROOT="$ARTIFACT_ROOT/$RUN_ID"
LOG_DIR="$RUN_ROOT/logs"
TEST_HOME="$RUN_ROOT/home"
SOURCE="$RUN_ROOT/source"
MNT="$RUN_ROOT/Crab Mount"
DEBUG_DIR="$REPO_ROOT/target/debug"
CRAB_EXE="$DEBUG_DIR/crab"
HELPER_EXE="$DEBUG_DIR/crab-nfs-mount"
HOST_HOME="${HOME:-}"
HOST_CARGO_HOME="${CARGO_HOME:-${HOST_HOME:+$HOST_HOME/.cargo}}"
HOST_RUSTUP_HOME="${RUSTUP_HOME:-${HOST_HOME:+$HOST_HOME/.rustup}}"

die() {
    printf "error: %s\n" "$*" >&2
    exit 1
}

is_mounted() {
    mount | grep -F " on $MNT (" | grep -F "(nfs," >/dev/null 2>&1
}

run_with_timeout() {
    local seconds="$1"
    shift

    "$@" &
    local pid="$!"
    local elapsed=0
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$elapsed" -ge "$seconds" ]; then
            kill -TERM "$pid" 2>/dev/null || true
            sleep 1
            kill -KILL "$pid" 2>/dev/null || true
            return 124
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done

    wait "$pid"
}

mount_is_readable() {
    local rc

    rc=0
    run_with_timeout 5 test -f "$MNT/hello.txt" || rc="$?"
    [ "$rc" -eq 124 ] && return 124
    [ "$rc" -eq 0 ] || return 1

    rc=0
    run_with_timeout 5 test -f "$MNT/.git" || rc="$?"
    [ "$rc" -eq 124 ] && return 124
    [ "$rc" -eq 0 ] || return 1

    return 0
}

cleanup_mounts() {
    set +e
    if is_mounted && [ -x "$CRAB_EXE" ]; then
        run_with_timeout 10 "$CRAB_EXE" unmount --mountpoint "$MNT" >"$LOG_DIR/unmount-cleanup.log" 2>&1
    fi
    if is_mounted; then
        /sbin/umount "$MNT" >/dev/null 2>&1
    fi
    if is_mounted; then
        /sbin/umount -f "$MNT" >/dev/null 2>&1
    fi
}

redact_retained_control_endpoint_filter() {
    python3 -c '
import json
import sys

def redact_control_endpoint(endpoint):
    if isinstance(endpoint, str) and endpoint.startswith("tcp:") and "?token=" in endpoint:
        return endpoint.split("?token=", 1)[0] + "?token=<redacted>"
    return endpoint

def sanitize(value):
    if isinstance(value, dict):
        return {
            key: redact_control_endpoint(child) if key == "control_endpoint" else sanitize(child)
            for key, child in value.items()
        }
    if isinstance(value, list):
        return [sanitize(child) for child in value]
    return value

json.dump(sanitize(json.load(sys.stdin)), sys.stdout, indent=2)
sys.stdout.write("\n")
'
}

wait_for_mount() {
    for _ in $(seq 1 60); do
        if is_mounted; then
            local mount_rc=0
            mount_is_readable || mount_rc="$?"
            case "$mount_rc" in
                0) return ;;
                124)
                    mount >"$LOG_DIR/mounts-on-timeout.txt" 2>&1 || true
                    die "NFS mount is present but reads timed out at $MNT"
                    ;;
            esac
        fi
        sleep 1
    done

    mount >"$LOG_DIR/mounts-on-timeout.txt" 2>&1 || true
    die "timed out waiting for NFS mount at $MNT"
}

wait_for_unmount() {
    for _ in $(seq 1 60); do
        if ! is_mounted; then
            return
        fi
        sleep 1
    done

    mount >"$LOG_DIR/mounts-after-unmount-timeout.txt" 2>&1 || true
    die "NFS mount is still mounted at $MNT"
}

assert_file_text() {
    local path="$1"
    local expected="$2"
    [ -f "$path" ] || die "missing file: $path"
    local actual
    actual="$(cat "$path")"
    [ "$actual" = "$expected" ] || die "unexpected file contents for $path"
}

assert_gitdir_file() {
    case "$(cat "$MNT/.git")" in
        gitdir:*) ;;
        *) die "synthetic .git did not render gitdir file" ;;
    esac
}

[ "$(uname -s)" = "Darwin" ] || die "native macOS NFS smoke must run on macOS"
command -v cargo >/dev/null 2>&1 || die "cargo is required"
command -v git >/dev/null 2>&1 || die "git is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required"
command -v mount_nfs >/dev/null 2>&1 || die "mount_nfs is required"
command -v umount >/dev/null 2>&1 || die "umount is required"

GIT_COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD)"

mkdir -p "$RUN_ROOT"
RUN_ROOT="$(cd "$RUN_ROOT" && pwd -P)"
LOG_DIR="$RUN_ROOT/logs"
TEST_HOME="$RUN_ROOT/home"
SOURCE="$RUN_ROOT/source"
MNT="$RUN_ROOT/Crab Mount"
mkdir -p "$LOG_DIR" "$TEST_HOME" "$SOURCE" "$MNT"

printf "run_id=%s\n" "$RUN_ID"
printf "artifact_root=%s\n" "$RUN_ROOT"

export HOME="$TEST_HOME"
export CRAB_CACHE_DIR="$RUN_ROOT/crab-cache"
export GIT_TERMINAL_PROMPT=0
export PATH="$DEBUG_DIR:$PATH"
if [ -n "$HOST_CARGO_HOME" ]; then
    export CARGO_HOME="$HOST_CARGO_HOME"
fi
if [ -n "$HOST_RUSTUP_HOME" ]; then
    export RUSTUP_HOME="$HOST_RUSTUP_HOME"
fi
mkdir -p "$CRAB_CACHE_DIR"

trap cleanup_mounts EXIT

cd "$CRAB_DIR"
cargo build -p crab --bin crab --no-default-features --features nfs,gix-all \
    >"$LOG_DIR/cargo-build-nfs.log" 2>&1
cp "$CRAB_EXE" "$HELPER_EXE"

"$CRAB_EXE" --version | tee "$RUN_ROOT/crab-version.txt"
"$HELPER_EXE" --version | tee "$RUN_ROOT/crab-nfs-mount-version.txt"
command -v mount_nfs >"$RUN_ROOT/mount-nfs-path.txt"

git -C "$SOURCE" init -b main >"$LOG_DIR/git-init.log" 2>&1
git -C "$SOURCE" config user.email nfs-smoke@crab.local
git -C "$SOURCE" config user.name "Crab NFS Smoke"
printf "hello" >"$SOURCE/hello.txt"
mkdir -p "$SOURCE/dir"
printf "nested" >"$SOURCE/dir/nested.txt"
ln -s hello.txt "$SOURCE/link-to-hello"
python3 - "$SOURCE/native-read.bin" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
size = 4 * 1024 * 1024
pattern = bytes(index % 251 for index in range(256 * 1024))
remaining = size
with path.open("wb") as handle:
    while remaining:
        chunk = pattern[: min(len(pattern), remaining)]
        handle.write(chunk)
        remaining -= len(chunk)
PY
git -C "$SOURCE" add .
git -C "$SOURCE" commit -m seed >"$LOG_DIR/git-commit.log" 2>&1

"$CRAB_EXE" mount doctor --backend nfs --mountpoint "$MNT" --json >"$RUN_ROOT/mount-doctor.json"
"$CRAB_EXE" mount --repo "$SOURCE" --mountpoint "$MNT" --backend nfs --no-refresh \
    >"$LOG_DIR/mount.log" 2>&1
wait_for_mount

assert_file_text "$MNT/hello.txt" "hello"
assert_file_text "$MNT/dir/nested.txt" "nested"
[ "$(readlink "$MNT/link-to-hello")" = "hello.txt" ] || die "unexpected symlink target"
assert_file_text "$MNT/link-to-hello" "hello"
assert_gitdir_file
python3 - "$MNT/native-read.bin" "$RUN_ROOT/native-read-benchmark.json" "$CRAB_EXE" "$MNT" <<'PY'
import hashlib
import json
import subprocess
import sys
import time
from pathlib import Path

path = Path(sys.argv[1])
output = Path(sys.argv[2])
crab = sys.argv[3]
mountpoint = sys.argv[4]
read_size = 256 * 1024
passes = 2
reads = 0
bytes_returned = 0
digest = hashlib.sha256()

PROTOCOL_COUNTERS = ("read_rpcs", "read_requested_bytes", "read_returned_bytes")
READ_LEASE_COUNTERS = (
    "temporary_overflows",
    "hits",
    "misses",
    "evictions",
    "stale_retries",
)
VFS_COUNTERS = (
    "open_read_calls",
    "read_at_calls",
    "returned_bytes",
    "source_cache_hits",
    "resolver_calls_avoided",
    "source_cache_misses",
    "source_cache_evictions",
    "source_cache_invalidations",
    "source_cache_stale_evictions",
    "stale_generation_rejections",
    "stale_overlay_view_rejections",
    "stale_overlay_file_rejections",
)
HYDRATION_COUNTERS = (
    "read_range_requests",
    "read_range_requested_bytes",
    "read_range_returned_bytes",
    "read_window_cache_hits",
    "read_window_cache_misses",
    "read_window_inflight_waits",
    "read_window_remote_fetches",
    "read_window_remote_bytes",
    "read_window_prefetch_requests",
    "read_window_prefetch_scheduled",
    "read_window_prefetch_skipped",
    "read_window_prefetch_errors",
    "chunk_cache_hits",
    "chunk_cache_misses",
    "chunk_inflight_waits",
    "chunk_remote_fetches",
    "chunk_remote_bytes",
)
SOURCE_NAMES = ("base_pointer", "base_blob", "base_empty", "overlay_file")
ADAPTIVE_NAMES = ("first", "sequential", "strided", "repeated", "random")


def counter_map(payload, keys):
    return {key: int(payload.get(key, 0)) for key in keys}


def vfs_snapshot(vfs):
    snapshot = counter_map(vfs, VFS_COUNTERS)
    for source_name in SOURCE_NAMES:
        source = vfs.get(source_name, {})
        if not isinstance(source, dict):
            source = {}
        snapshot[f"{source_name}_reads"] = int(source.get("reads", 0))
        snapshot[f"{source_name}_bytes"] = int(source.get("bytes", 0))
    for adaptive_name in ADAPTIVE_NAMES:
        total = 0
        for source_name in SOURCE_NAMES:
            source = vfs.get(source_name, {})
            if not isinstance(source, dict):
                source = {}
            adaptive = source.get("adaptive", {})
            if not isinstance(adaptive, dict):
                adaptive = {}
            total += int(adaptive.get(adaptive_name, 0))
        snapshot[f"adaptive_{adaptive_name}"] = total
    return snapshot


def runtime_snapshot():
    payload = json.loads(
        subprocess.check_output(
            [crab, "mount", "status", "--mountpoint", mountpoint, "--json"],
            text=True,
        )
    )
    runtime = payload.get("nfs_runtime", {})
    protocol = runtime.get("protocol")
    if not isinstance(protocol, dict):
        raise SystemExit("mount status did not include NFS protocol counters")
    read_leases = runtime.get("read_leases")
    if not isinstance(read_leases, dict):
        raise SystemExit("mount status did not include NFS read lease counters")
    vfs = runtime.get("vfs")
    if not isinstance(vfs, dict):
        raise SystemExit("mount status did not include VFS read counters")
    hydration = runtime.get("hydration")
    if not isinstance(hydration, dict):
        raise SystemExit("mount status did not include hydration counters")
    return {
        "protocol": counter_map(protocol, PROTOCOL_COUNTERS),
        "read_leases": counter_map(read_leases, READ_LEASE_COUNTERS),
        "vfs": vfs_snapshot(vfs),
        "hydration": counter_map(hydration, HYDRATION_COUNTERS),
    }


def counter_delta(before, after):
    return {key: after[key] - before[key] for key in before}


before = runtime_snapshot()
start = time.perf_counter_ns()
for pass_index in range(passes):
    with path.open("rb", buffering=0) as handle:
        while True:
            chunk = handle.read(read_size)
            if not chunk:
                break
            if pass_index == 0:
                digest.update(chunk)
            reads += 1
            bytes_returned += len(chunk)
elapsed_ns = max(time.perf_counter_ns() - start, 1)
elapsed_ms = max(round(elapsed_ns / 1_000_000), 1)
after = runtime_snapshot()
protocol_delta = counter_delta(before["protocol"], after["protocol"])
read_leases_delta = counter_delta(before["read_leases"], after["read_leases"])
vfs_delta = counter_delta(before["vfs"], after["vfs"])
hydration_delta = counter_delta(before["hydration"], after["hydration"])
user_mib = bytes_returned / (1024 * 1024)
report = {
    "schema_version": 1,
    "suite": "nfs-native-read-benchmark",
    "scenario": "native_sequential_read",
    "path": str(path),
    "mountpoint": mountpoint,
    "file_size": path.stat().st_size,
    "read_size": read_size,
    "reads": reads,
    "bytes_returned": bytes_returned,
    "elapsed_ms": elapsed_ms,
    "mib_per_sec": (bytes_returned / (1024 * 1024)) / (elapsed_ns / 1_000_000_000),
    "sha256": digest.hexdigest(),
    "nfs_protocol_before": before["protocol"],
    "nfs_protocol_after": after["protocol"],
    "nfs_protocol_delta": protocol_delta,
    "nfs_read_leases_before": before["read_leases"],
    "nfs_read_leases_after": after["read_leases"],
    "nfs_read_leases_delta": read_leases_delta,
    "nfs_vfs_before": before["vfs"],
    "nfs_vfs_after": after["vfs"],
    "nfs_vfs_delta": vfs_delta,
    "nfs_hydration_before": before["hydration"],
    "nfs_hydration_after": after["hydration"],
    "nfs_hydration_delta": hydration_delta,
    "efficiency": {
        "requested_bytes_per_user_byte": protocol_delta["read_requested_bytes"] / bytes_returned,
        "returned_bytes_per_user_byte": protocol_delta["read_returned_bytes"] / bytes_returned,
        "read_rpcs_per_mib": protocol_delta["read_rpcs"] / user_mib,
    },
}
output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
PY

if ! (set -C; printf "exclusive" >"$MNT/exclusive.txt") 2>"$LOG_DIR/exclusive-create.err"; then
    die "exclusive create unexpectedly failed"
fi
if (set -C; printf "again" >"$MNT/exclusive.txt") >"$LOG_DIR/exclusive-recreate.out" 2>"$LOG_DIR/exclusive-recreate.err"; then
    die "exclusive recreate unexpectedly succeeded"
fi

printf "created" >"$MNT/created.txt"
printf "++" >>"$MNT/hello.txt"
mkdir "$MNT/newdir"
mv "$MNT/created.txt" "$MNT/newdir/renamed.txt"
ln -s ../hello.txt "$MNT/newdir/link-created"
if truncate -s 0 "$MNT/newdir" >"$LOG_DIR/truncate-dir.out" 2>"$LOG_DIR/truncate-dir.err"; then
    die "truncate on directory unexpectedly succeeded"
fi
rm "$MNT/dir/nested.txt"
rmdir "$MNT/dir"

if sh -c "printf bad > '$MNT/.git'" >"$LOG_DIR/git-overwrite.out" 2>"$LOG_DIR/git-overwrite.err"; then
    die "synthetic .git overwrite unexpectedly succeeded"
fi
if mv "$MNT/newdir/renamed.txt" "$MNT/.git" >"$LOG_DIR/git-rename.out" 2>"$LOG_DIR/git-rename.err"; then
    die "rename over synthetic .git unexpectedly succeeded"
fi

assert_file_text "$MNT/newdir/renamed.txt" "created"
assert_file_text "$MNT/exclusive.txt" "exclusive"
assert_file_text "$MNT/hello.txt" "hello++"
[ "$(readlink "$MNT/newdir/link-created")" = "../hello.txt" ] || die "unexpected created symlink target"
assert_file_text "$MNT/newdir/link-created" "hello++"
[ ! -e "$MNT/dir" ] || die "removed directory is still visible"
assert_gitdir_file

"$CRAB_EXE" mount commit --mountpoint "$MNT" -m "native mount commit one" \
    >"$LOG_DIR/mount-commit-one.log" 2>&1
assert_file_text "$MNT/hello.txt" "hello++"
assert_file_text "$MNT/newdir/renamed.txt" "created"
assert_file_text "$MNT/exclusive.txt" "exclusive"

printf "second" >"$MNT/second-commit.txt"
"$CRAB_EXE" mount commit --mountpoint "$MNT" -m "native mount commit two" \
    >"$LOG_DIR/mount-commit-two.log" 2>&1
assert_file_text "$MNT/hello.txt" "hello++"
assert_file_text "$MNT/newdir/renamed.txt" "created"
assert_file_text "$MNT/exclusive.txt" "exclusive"
assert_file_text "$MNT/second-commit.txt" "second"
[ "$(git -C "$SOURCE" rev-list --count HEAD)" = "3" ] || die "mounted commits did not advance source HEAD twice"

"$CRAB_EXE" mount list --json | redact_retained_control_endpoint_filter >"$RUN_ROOT/mount-list.json"
"$CRAB_EXE" mount status --mountpoint "$MNT" --json | redact_retained_control_endpoint_filter >"$RUN_ROOT/mount-status.json"
"$CRAB_EXE" mount status --mountpoint "$MNT" --live-only --json | redact_retained_control_endpoint_filter >"$RUN_ROOT/control-status.json"
grep -F "$SOURCE" "$RUN_ROOT/mount-list.json" >/dev/null || die "mount registry did not include the NFS source"
grep -F "running" "$RUN_ROOT/mount-list.json" >/dev/null || die "mount registry did not show a running mount"
python3 - "$RUN_ROOT/mount-status.json" <<'PY'
import json
import sys
from pathlib import Path

runtime = json.loads(Path(sys.argv[1]).read_text()).get("nfs_runtime")
if not runtime:
    raise SystemExit("mount status did not include live NFS runtime counters")
if runtime["protocol"]["read_rpcs"] <= 0:
    raise SystemExit("NFS runtime did not record read RPCs")
lifecycle = runtime.get("lifecycle")
if not lifecycle:
    raise SystemExit("NFS runtime did not include lifecycle counters")
for key in ("server_bind_ms", "native_mount_ms", "startup_ms"):
    value = lifecycle.get(key)
    if not isinstance(value, int) or value < 0:
        raise SystemExit(f"NFS lifecycle counter {key} must be a non-negative integer")
if lifecycle["startup_ms"] < lifecycle["server_bind_ms"]:
    raise SystemExit("NFS lifecycle startup_ms must cover server_bind_ms")
if lifecycle["startup_ms"] < lifecycle["native_mount_ms"]:
    raise SystemExit("NFS lifecycle startup_ms must cover native_mount_ms")
read_leases = runtime.get("read_leases")
if not read_leases:
    raise SystemExit("NFS runtime did not include read lease counters")
for key in (
    "entries",
    "max_entries",
    "estimated_bytes",
    "max_estimated_bytes",
    "pinned_entries",
    "active_pins",
    "temporary_overflows",
    "hits",
    "misses",
    "evictions",
    "stale_retries",
):
    value = read_leases.get(key)
    if not isinstance(value, int) or value < 0:
        raise SystemExit(f"NFS read lease counter {key} must be a non-negative integer")
for key in ("max_entries", "max_estimated_bytes"):
    if read_leases[key] <= 0:
        raise SystemExit(f"NFS read lease budget {key} must be positive")
if read_leases["hits"] <= 0:
    raise SystemExit("NFS read lease hits must be positive")
if read_leases["misses"] <= 0:
    raise SystemExit("NFS read lease misses must be positive")
directory_pages = runtime.get("directory_pages")
if not directory_pages:
    raise SystemExit("NFS runtime did not include directory page cache counters")
for key in ("entries", "max_entries", "estimated_bytes", "max_estimated_bytes", "hits", "misses", "evictions", "stale_evictions"):
    value = directory_pages.get(key)
    if not isinstance(value, int) or value < 0:
        raise SystemExit(f"NFS directory page cache counter {key} must be a non-negative integer")
write_journal = runtime.get("write_journal")
if not write_journal:
    raise SystemExit("NFS runtime did not include write journal counters")
for key in ("sync_attempts", "sync_successes", "sync_failures", "total_sync_latency_ms"):
    value = write_journal.get(key)
    if not isinstance(value, int) or value < 0:
        raise SystemExit(f"NFS write journal counter {key} must be a non-negative integer")
if write_journal["sync_successes"] + write_journal["sync_failures"] > write_journal["sync_attempts"]:
    raise SystemExit("NFS write journal sync successes and failures must not exceed attempts")
PY
python3 - "$RUN_ROOT/mount-status.json" "$RUN_ROOT/writeback-check.json" <<'PY'
import json
import sys
from pathlib import Path

status = json.loads(Path(sys.argv[1]).read_text())
payload = {
    "schema_version": 1,
    "action": "writeback",
    "mountpoint": status["mountpoint"],
    "source": status["source"],
    "state": status["state"],
    "pid": status["pid"],
    "control_endpoint": status["control_endpoint"],
    "log_path": status["log_path"],
    "content_checks": {
        "hello_appended": True,
        "renamed_file_created": True,
        "exclusive_file_created": True,
        "symlink_created": True,
        "gitdir_preserved": True,
        "gitdir_overwrite_rejected": True,
        "gitdir_rename_rejected": True,
        "removed_directory_absent": True,
    },
}
encoded = json.dumps(payload, indent=2) + "\n"
Path(sys.argv[2]).write_text(encoded)
PY

"$CRAB_EXE" unmount --mountpoint "$MNT" >"$LOG_DIR/unmount.log" 2>&1
wait_for_unmount
python3 - "$RUN_ROOT/mount-status.json" "$RUN_ROOT/unmount-check.json" <<'PY'
import json
import sys
from pathlib import Path

status = json.loads(Path(sys.argv[1]).read_text())
payload = {
    "schema_version": 1,
    "action": "control_shutdown",
    "mountpoint": status["mountpoint"],
    "source": status["source"],
    "pid": status["pid"],
    "control_endpoint": status["control_endpoint"],
    "log_path": status["log_path"],
    "mounted_after": False,
}
encoded = json.dumps(payload, indent=2) + "\n"
Path(sys.argv[2]).write_text(encoded)
Path(sys.argv[2]).with_name("control-shutdown.json").write_text(encoded)
PY

"$CRAB_EXE" mount --repo "$SOURCE" --mountpoint "$MNT" --backend nfs --no-refresh \
    >"$LOG_DIR/remount.log" 2>&1
wait_for_mount

assert_file_text "$MNT/hello.txt" "hello++"
assert_file_text "$MNT/newdir/renamed.txt" "created"
assert_file_text "$MNT/exclusive.txt" "exclusive"
assert_file_text "$MNT/second-commit.txt" "second"
[ "$(readlink "$MNT/newdir/link-created")" = "../hello.txt" ] || die "unexpected created symlink target after remount"
assert_file_text "$MNT/newdir/link-created" "hello++"
[ ! -e "$MNT/dir" ] || die "removed directory is visible after remount"
assert_gitdir_file
"$CRAB_EXE" mount status --mountpoint "$MNT" --json | redact_retained_control_endpoint_filter >"$RUN_ROOT/remount-status.json"
python3 - "$RUN_ROOT/remount-status.json" "$RUN_ROOT/remount-check.json" <<'PY'
import json
import sys
from pathlib import Path

status = json.loads(Path(sys.argv[1]).read_text())
payload = {
    "schema_version": 1,
    "action": "remount",
    "mountpoint": status["mountpoint"],
    "source": status["source"],
    "state": status["state"],
    "pid": status["pid"],
    "control_endpoint": status["control_endpoint"],
    "log_path": status["log_path"],
    "mounted_after": True,
    "content_checks": {
        "hello_preserved": True,
        "renamed_file_preserved": True,
        "exclusive_file_preserved": True,
        "symlink_preserved": True,
        "gitdir_preserved": True,
        "removed_directory_absent": True,
    },
}
Path(sys.argv[2]).write_text(json.dumps(payload, indent=2) + "\n")
PY

"$CRAB_EXE" unmount --mountpoint "$MNT" >"$LOG_DIR/remount-unmount.log" 2>&1
wait_for_unmount
trap - EXIT

python3 - "$RUN_ROOT/nfs-smoke-report.json" "$RUN_ID" "$RUN_ROOT" "$GIT_COMMIT" <<'PY'
import json
import sys
from pathlib import Path

report_path = Path(sys.argv[1])
run_id = sys.argv[2]
run_root = Path(sys.argv[3])
git_commit = sys.argv[4]
report = {
    "schema_version": 1,
    "suite": "mount-nfs-macos",
    "platform": "macos",
    "status": "ok",
    "backend": "nfs",
    "run_id": run_id,
    "git_commit": git_commit,
    "artifact_root": str(run_root),
    "crab_version": (run_root / "crab-version.txt").read_text().strip(),
    "helper_version": (run_root / "crab-nfs-mount-version.txt").read_text().strip(),
    "checks": [
        "build",
        "helper_version",
        "mount_doctor",
        "initial_read",
        "native_read_benchmark",
        "writeback",
        "mount_list",
        "mount_status",
        "control_status",
        "unmount",
        "control_shutdown",
        "remount",
    ],
    "artifacts": {
        "mount_list": str(run_root / "mount-list.json"),
        "mount_doctor": str(run_root / "mount-doctor.json"),
        "mount_status": str(run_root / "mount-status.json"),
        "control_status": str(run_root / "control-status.json"),
        "native_read_benchmark": str(run_root / "native-read-benchmark.json"),
        "writeback_check": str(run_root / "writeback-check.json"),
        "unmount_check": str(run_root / "unmount-check.json"),
        "control_shutdown": str(run_root / "control-shutdown.json"),
        "remount_check": str(run_root / "remount-check.json"),
    },
}
report_path.write_text(json.dumps(report, indent=2) + "\n")
PY
python3 "$CRAB_DIR/scripts/verify-nfs-smoke-report.py" \
    "$RUN_ROOT/nfs-smoke-report.json" \
    --suite mount-nfs-macos \
    --platform macos \
    --require-artifacts \
    --expected-git-commit "$GIT_COMMIT"
printf "nfs_smoke_report=%s\n" "$RUN_ROOT/nfs-smoke-report.json"
printf "macos_nfs_mount_smoke=ok\n"
