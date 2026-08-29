#!/usr/bin/env bash
#
# Run a Linux native-NFS mount smoke in Docker.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

DOCKER="${DOCKER:-docker}"
RUN_ID="${CRAB_NFS_SMOKE_RUN_ID:-mount-nfs-linux-$(date -u +%Y%m%d-%H%M%S)}"
ARTIFACT_ROOT="${CRAB_NFS_SMOKE_ROOT:-/tmp/crab-mount-nfs-linux-smoke}"
CACHE_ROOT="${CRAB_NFS_SMOKE_CACHE_ROOT:-$ARTIFACT_ROOT/cache}"
RUN_ROOT="$ARTIFACT_ROOT/$RUN_ID"
HOST_TARGET="${CRAB_NFS_SMOKE_TARGET_CACHE:-$CACHE_ROOT/target}"
HOST_CARGO="${CRAB_NFS_SMOKE_CARGO_CACHE:-$CACHE_ROOT/cargo}"
RUST_IMAGE="${CRAB_NFS_SMOKE_RUST_IMAGE:-rust:1.91-bookworm}"
RUNNER="crab-nfs-smoke-$RUN_ID"

die() {
    printf "error: %s\n" "$*" >&2
    exit 1
}

cleanup() {
    "$DOCKER" rm -f "$RUNNER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

command -v "$DOCKER" >/dev/null 2>&1 || die "docker is required"
"$DOCKER" info >/dev/null 2>&1 || die "Docker daemon is not running"
GIT_COMMIT="$(git -C "$REPO_ROOT" rev-parse HEAD)" || die "could not resolve checkout commit"

mkdir -p "$RUN_ROOT" "$HOST_TARGET" "$HOST_CARGO"

printf "run_id=%s\n" "$RUN_ID"
printf "artifact_root=%s\n" "$RUN_ROOT"

"$DOCKER" run --rm -i \
    --name "$RUNNER" \
    --cap-add SYS_ADMIN \
    --security-opt apparmor:unconfined \
    -v "$REPO_ROOT:/src" \
    -v "$HOST_TARGET:/src/target" \
    -v "$HOST_CARGO:/cargo" \
    -v "$RUN_ROOT:/e2e" \
    -e CARGO_HOME=/cargo \
    -e CARGO_INCREMENTAL=0 \
    -e CARGO_PROFILE_DEV_DEBUG=0 \
    -e GIT_COMMIT="$GIT_COMMIT" \
    -e CRAB_NFS_SMOKE_RUN_ID="$RUN_ID" \
    -e CRAB_NFS_SMOKE_REPORT="$RUN_ROOT/nfs-smoke-report.json" \
    "$RUST_IMAGE" bash -s <<'INNER'
set -euo pipefail

export HOME=/e2e/home
export CRAB_CACHE_DIR=/e2e/crab-cache
export GIT_TERMINAL_PROMPT=0
export PATH=/src/target/debug:$PATH

HOST_TRIPLE="$(rustc -vV | awk '/^host:/ { print $2 }')"
case "$HOST_TRIPLE" in
    aarch64-unknown-linux-gnu)
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER:-cc}"
        ;;
    x86_64-unknown-linux-gnu)
        export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER:-cc}"
        ;;
esac

SOURCE=/e2e/run/source
MNT="/e2e/run/Crab Mount"

mkdir -p "$HOME" "$CRAB_CACHE_DIR" /e2e/logs /e2e/run

cleanup_mounts() {
    set +e
    if command -v crab >/dev/null 2>&1; then
        crab unmount --mountpoint "$MNT" >/e2e/logs/unmount-cleanup.log 2>&1
    fi
    umount "$MNT" >/dev/null 2>&1
}
trap cleanup_mounts EXIT

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

export DEBIAN_FRONTEND=noninteractive
apt-get update >/e2e/logs/apt-update.log
apt-get install -y --no-install-recommends \
    ca-certificates git make nfs-common pkg-config procps python3 util-linux \
    >/e2e/logs/apt-install.log

cd /src/crab
cargo build -p crab --bin crab --no-default-features --features nfs \
    >/e2e/logs/cargo-build-nfs.log 2>&1
ln -sf crab /src/target/debug/crab-nfs-mount

crab --version | tee /e2e/crab-version.txt
crab-nfs-mount --version | tee /e2e/crab-nfs-mount-version.txt
command -v mount.nfs >/e2e/mount-nfs-path.txt

mkdir -p "$SOURCE" "$MNT"
git -C "$SOURCE" init -b main >/e2e/logs/git-init.log
git -C "$SOURCE" config user.email nfs-smoke@crab.local
git -C "$SOURCE" config user.name "Crab NFS Smoke"
printf "hello" > "$SOURCE/hello.txt"
mkdir -p "$SOURCE/dir"
printf "nested" > "$SOURCE/dir/nested.txt"
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
git -C "$SOURCE" commit -m seed >/e2e/logs/git-commit.log

crab mount doctor --backend nfs --mountpoint "$MNT" --json >/e2e/mount-doctor.json
crab mount --repo "$SOURCE" --mountpoint "$MNT" --backend nfs --no-refresh \
    >/e2e/logs/mount.log 2>&1

for _ in $(seq 1 60); do
    if mountpoint -q "$MNT" && [ -f "$MNT/hello.txt" ] && [ -f "$MNT/.git" ]; then
        break
    fi
    sleep 1
done
mountpoint -q "$MNT"

[ "$(cat "$MNT/hello.txt")" = "hello" ]
[ "$(cat "$MNT/dir/nested.txt")" = "nested" ]
[ "$(readlink "$MNT/link-to-hello")" = "hello.txt" ]
[ "$(cat "$MNT/link-to-hello")" = "hello" ]
case "$(cat "$MNT/.git")" in
    gitdir:*) ;;
    *) echo "synthetic .git did not render gitdir file" >&2; exit 1 ;;
esac
python3 - "$MNT/native-read.bin" /e2e/native-read-benchmark.json crab "$MNT" <<'PY'
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

if ! (set -C; printf "exclusive" >"$MNT/exclusive.txt") 2>/e2e/logs/exclusive-create.err; then
    echo "exclusive create unexpectedly failed" >&2
    exit 1
fi
if (set -C; printf "again" >"$MNT/exclusive.txt") >/e2e/logs/exclusive-recreate.out 2>/e2e/logs/exclusive-recreate.err; then
    echo "exclusive recreate unexpectedly succeeded" >&2
    exit 1
fi

printf "created" > "$MNT/created.txt"
printf "++" >> "$MNT/hello.txt"
mkdir "$MNT/newdir"
mv "$MNT/created.txt" "$MNT/newdir/renamed.txt"
ln -s ../hello.txt "$MNT/newdir/link-created"
if truncate -s 0 "$MNT/newdir" >/e2e/logs/truncate-dir.out 2>/e2e/logs/truncate-dir.err; then
    echo "truncate on directory unexpectedly succeeded" >&2
    exit 1
fi
rm "$MNT/dir/nested.txt"
rmdir "$MNT/dir"

if sh -c "printf bad > '$MNT/.git'" >/e2e/logs/git-overwrite.out 2>/e2e/logs/git-overwrite.err; then
    echo "synthetic .git overwrite unexpectedly succeeded" >&2
    exit 1
fi
if mv "$MNT/newdir/renamed.txt" "$MNT/.git" >/e2e/logs/git-rename.out 2>/e2e/logs/git-rename.err; then
    echo "rename over synthetic .git unexpectedly succeeded" >&2
    exit 1
fi

[ "$(cat "$MNT/newdir/renamed.txt")" = "created" ]
[ "$(cat "$MNT/exclusive.txt")" = "exclusive" ]
[ "$(cat "$MNT/hello.txt")" = "hello++" ]
[ "$(readlink "$MNT/newdir/link-created")" = "../hello.txt" ]
[ "$(cat "$MNT/newdir/link-created")" = "hello++" ]
[ ! -e "$MNT/dir" ]
case "$(cat "$MNT/.git")" in
    gitdir:*) ;;
    *) echo "synthetic .git changed after failed mutations" >&2; exit 1 ;;
esac

crab mount list --json | redact_retained_control_endpoint_filter >/e2e/mount-list.json
crab mount status --mountpoint "$MNT" --json | redact_retained_control_endpoint_filter >/e2e/mount-status.json
crab mount status --mountpoint "$MNT" --live-only --json | redact_retained_control_endpoint_filter >/e2e/control-status.json
python3 - <<'PY'
import json
from pathlib import Path

entries = json.loads(Path("/e2e/mount-list.json").read_text())
if not entries:
    raise SystemExit("mount registry did not include the NFS mount")
entry = entries[0]
if entry.get("source") != "/e2e/run/source":
    raise SystemExit(f"unexpected source: {entry.get('source')!r}")
if entry.get("state", "").startswith("running") is False:
    raise SystemExit(f"mount is not running: {entry.get('state')!r}")
PY

python3 - <<'PY'
import json
from pathlib import Path

runtime = json.loads(Path("/e2e/mount-status.json").read_text()).get("nfs_runtime")
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
python3 - <<'PY'
import json
from pathlib import Path

status = json.loads(Path("/e2e/mount-status.json").read_text())
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
Path("/e2e/writeback-check.json").write_text(json.dumps(payload, indent=2) + "\n")
PY

crab unmount --mountpoint "$MNT" >/e2e/logs/unmount.log 2>&1
if mountpoint -q "$MNT"; then
    echo "mountpoint is still mounted after crab unmount" >&2
    exit 1
fi
python3 - <<'PY'
import json
from pathlib import Path

status = json.loads(Path("/e2e/mount-status.json").read_text())
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
Path("/e2e/unmount-check.json").write_text(encoded)
Path("/e2e/control-shutdown.json").write_text(encoded)
PY

crab mount --repo "$SOURCE" --mountpoint "$MNT" --backend nfs --no-refresh \
    >/e2e/logs/remount.log 2>&1
for _ in $(seq 1 60); do
    if mountpoint -q "$MNT" && [ -f "$MNT/hello.txt" ]; then
        break
    fi
    sleep 1
done
mountpoint -q "$MNT"

[ "$(cat "$MNT/hello.txt")" = "hello++" ]
[ "$(cat "$MNT/newdir/renamed.txt")" = "created" ]
[ "$(cat "$MNT/exclusive.txt")" = "exclusive" ]
[ "$(readlink "$MNT/newdir/link-created")" = "../hello.txt" ]
[ "$(cat "$MNT/newdir/link-created")" = "hello++" ]
[ ! -e "$MNT/dir" ]
case "$(cat "$MNT/.git")" in
    gitdir:*) ;;
    *) echo "synthetic .git changed after remount" >&2; exit 1 ;;
esac
crab mount status --mountpoint "$MNT" --json | redact_retained_control_endpoint_filter >/e2e/remount-status.json
python3 - <<'PY'
import json
from pathlib import Path

status = json.loads(Path("/e2e/remount-status.json").read_text())
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
Path("/e2e/remount-check.json").write_text(json.dumps(payload, indent=2) + "\n")
PY

crab unmount --mountpoint "$MNT" >/e2e/logs/remount-unmount.log 2>&1
if mountpoint -q "$MNT"; then
    echo "mountpoint is still mounted after remount unmount" >&2
    exit 1
fi
trap - EXIT

python3 - <<'PY'
import json
import os
from pathlib import Path

report = {
    "schema_version": 1,
    "suite": "mount-nfs-linux",
    "platform": "linux",
    "status": "ok",
    "backend": "nfs",
    "run_id": os.environ.get("CRAB_NFS_SMOKE_RUN_ID", "unknown"),
    "git_commit": os.environ["GIT_COMMIT"],
    "artifact_root": "/e2e",
    "crab_version": Path("/e2e/crab-version.txt").read_text().strip(),
    "helper_version": Path("/e2e/crab-nfs-mount-version.txt").read_text().strip(),
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
        "mount_list": "/e2e/mount-list.json",
        "mount_doctor": "/e2e/mount-doctor.json",
        "mount_status": "/e2e/mount-status.json",
        "control_status": "/e2e/control-status.json",
        "native_read_benchmark": "/e2e/native-read-benchmark.json",
        "writeback_check": "/e2e/writeback-check.json",
        "unmount_check": "/e2e/unmount-check.json",
        "control_shutdown": "/e2e/control-shutdown.json",
        "remount_check": "/e2e/remount-check.json",
    },
}
Path("/e2e/nfs-smoke-report.json").write_text(json.dumps(report, indent=2) + "\n")
PY
python3 /src/crab/scripts/verify-nfs-smoke-report.py \
    /e2e/nfs-smoke-report.json \
    --suite mount-nfs-linux \
    --platform linux \
    --require-artifacts \
    --expected-git-commit "$GIT_COMMIT"
echo "nfs_smoke_report=$CRAB_NFS_SMOKE_REPORT"
echo "linux_nfs_mount_smoke=ok"
INNER
