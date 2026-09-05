#!/usr/bin/env bash
#
# Run an end-to-end Crab mount smoke on native macOS against Docker RustFS.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE_ROOT="$(cd "$CRAB_DIR/.." && pwd)"

DOCKER="${DOCKER:-docker}"
AWS="${AWS:-aws}"
RUN_ID="${CRAB_MOUNT_MACOS_RUN_ID:-mount-macos-$(date -u +%Y%m%d-%H%M%S)}"
ARTIFACT_ROOT="${CRAB_MOUNT_MACOS_ROOT:-/tmp/crab-mount-large-macos-rustfs-smoke}"
RUN_ROOT="$ARTIFACT_ROOT/$RUN_ID"
TIMINGS_FILE="$RUN_ROOT/timings.env"
BIN_DIR="${CRAB_MOUNT_MACOS_BIN_DIR:-$RUN_ROOT/bin}"
TEST_HOME="$RUN_ROOT/home"
BACKEND="${CRAB_MOUNT_MACOS_BACKEND:-fuse}"
SEED_MIB="${CRAB_MOUNT_MACOS_SEED_MIB:-32}"
NEW_MIB="${CRAB_MOUNT_MACOS_NEW_MIB:-40}"
CONCURRENT_WRITERS="${CRAB_MOUNT_MACOS_CONCURRENT_WRITERS:-4}"
CONCURRENT_MIB="${CRAB_MOUNT_MACOS_CONCURRENT_MIB:-16}"
DIRECTORY_ENTRIES="${CRAB_MOUNT_MACOS_DIRECTORY_ENTRIES:-0}"
FS_OPERATION_TIMEOUT_SECS="${CRAB_MOUNT_MACOS_FS_OPERATION_TIMEOUT_SECS:-120}"
CLEANUP_TIMEOUT_SECS="${CRAB_MOUNT_MACOS_CLEANUP_TIMEOUT_SECS:-10}"
RUSTFS_IMAGE="${CRAB_MOUNT_MACOS_RUSTFS_IMAGE:-rustfs/rustfs:1.0.0-beta.8-glibc}"
BUCKET="${CRAB_MOUNT_MACOS_BUCKET:-crab}"
REGION="${AWS_REGION:-us-east-1}"
EXTERNAL_ENDPOINT="${CRAB_MOUNT_MACOS_ENDPOINT_URL:-}"
MACFUSE_FS_PATH="/Library/Filesystems/macfuse.fs"
MACFUSE_LOADER="$MACFUSE_FS_PATH/Contents/Resources/load_macfuse"

NET="net-$RUN_ID"
RUSTFS="rustfs-$RUN_ID"

die() {
    printf "error: %s\n" "$*" >&2
    exit 1
}

with_test_env() {
    with_cache_env "$RUN_ROOT/crab-cache" "$@"
}

with_cache_env() {
    local cache_dir="$1"
    shift
    HOME="$TEST_HOME" \
        PATH="$BIN_DIR:$PATH" \
        CRAB_CACHE_DIR="$cache_dir" \
        AWS_ACCESS_KEY_ID=crab \
        AWS_SECRET_ACCESS_KEY=crab \
        AWS_DEFAULT_REGION="$REGION" \
        AWS_REGION="$REGION" \
        AWS_ALLOW_HTTP=true \
        AWS_EC2_METADATA_DISABLED=true \
        AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false \
        VIRTUAL_HOSTED_STYLE_REQUEST=false \
        "$@"
}

run_with_timeout() {
    local timeout_secs="$1"
    shift
    python3 -c '
import subprocess
import sys

try:
    result = subprocess.run(sys.argv[2:], timeout=int(sys.argv[1]))
except subprocess.TimeoutExpired:
    print(f"command timed out after {sys.argv[1]}s: {sys.argv[2]}", file=sys.stderr)
    raise SystemExit(124)
raise SystemExit(result.returncode)
' "$timeout_secs" "$@"
}

cleanup_mount() {
    local mountpoint="$1"
    [[ -d "$mountpoint" ]] || return 0
    if [[ -x "$BIN_DIR/crab" ]]; then
        with_test_env run_with_timeout "$CLEANUP_TIMEOUT_SECS" \
            "$BIN_DIR/crab" unmount --mountpoint "$mountpoint" \
            >"$RUN_ROOT/logs/cleanup-$(basename "$mountpoint").log" 2>&1 || true
    fi
    run_with_timeout "$CLEANUP_TIMEOUT_SECS" diskutil unmount force "$mountpoint" \
        >/dev/null 2>&1 || true
    run_with_timeout "$CLEANUP_TIMEOUT_SECS" umount -f "$mountpoint" \
        >/dev/null 2>&1 || true
}

cleanup() {
    cleanup_mount "$RUN_ROOT/run/mnt-ro"
    cleanup_mount "$RUN_ROOT/run/mnt-rw"
    if [[ -z "$EXTERNAL_ENDPOINT" ]]; then
        "$DOCKER" rm -f "$RUSTFS" >/dev/null 2>&1 || true
        "$DOCKER" network rm "$NET" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

hash_file() {
    run_with_timeout "$FS_OPERATION_TIMEOUT_SECS" shasum -a 256 "$1" | awk '{print $1}'
}

wait_for_path() {
    local path="$1"
    for _ in $(seq 1 60); do
        [[ -e "$path" ]] && return 0
        sleep 1
    done
    return 1
}

capture_nfs_diagnostics() {
    local label="$1"
    local mountpoint="$2"
    [[ "$BACKEND" == "nfs" ]] || return 0
    mount >"$RUN_ROOT/logs/$label-mounts.txt" 2>&1 || true
    nfsstat -m >"$RUN_ROOT/logs/$label-nfsstat-mounts.txt" 2>&1 || true
    nfsstat -c >"$RUN_ROOT/logs/$label-nfsstat-client.txt" 2>&1 || true
    with_test_env run_with_timeout "$CLEANUP_TIMEOUT_SECS" \
        "$BIN_DIR/crab" mount status --mountpoint "$mountpoint" --json --verbose \
        >"$RUN_ROOT/logs/$label-mount-status.json" \
        2>"$RUN_ROOT/logs/$label-mount-status.err" || true
}

now_ms() {
    python3 -c 'import time; print(time.monotonic_ns() // 1000000)'
}

record_duration_since() {
    local name="$1"
    local start_ms="$2"
    local end_ms
    end_ms="$(now_ms)"
    printf "%s=%s\n" "$name" "$((end_ms - start_ms))" >> "$TIMINGS_FILE"
}

mount_cache_hash() {
    python3 - "$1" <<'PY'
import hashlib
import sys

url = sys.argv[1].strip()
if "://" in url:
    scheme, rest = url.split("://", 1)
    normalized = f"{scheme.lower()}://{rest.rstrip('/')}"
else:
    normalized = url.rstrip("/")
print(hashlib.sha256(normalized.encode()).hexdigest()[:12])
PY
}

macfuse_device_available() {
    ls /dev 2>/dev/null | grep -E '^(fuse|macfuse|osxfuse)' >/dev/null
}

ensure_macfuse_ready() {
    [[ -d "$MACFUSE_FS_PATH" ]] || die "macFUSE is not installed; install and approve it with: brew install --cask macfuse"

    if macfuse_device_available; then
        return 0
    fi

    if [[ -x "$MACFUSE_LOADER" ]]; then
        "$MACFUSE_LOADER" >"$RUN_ROOT/logs/load-macfuse.log" 2>&1 || true
    fi

    if macfuse_device_available; then
        return 0
    fi

    die "macFUSE is installed but its kernel device is not loaded; approve macFUSE in System Settings, reboot if prompted, then retry"
}

[[ "$(uname -s)" == "Darwin" ]] || die "native macOS smoke must run on Darwin; use make mount-large-rustfs-smoke for Docker/Linux"
[[ "$BACKEND" == "fuse" || "$BACKEND" == "nfs" ]] || die "CRAB_MOUNT_MACOS_BACKEND must be fuse or nfs"
[[ "$SEED_MIB" =~ ^[0-9]+$ ]] || die "CRAB_MOUNT_MACOS_SEED_MIB must be an integer"
[[ "$NEW_MIB" =~ ^[0-9]+$ ]] || die "CRAB_MOUNT_MACOS_NEW_MIB must be an integer"
[[ "$CONCURRENT_WRITERS" =~ ^[0-9]+$ ]] || die "CRAB_MOUNT_MACOS_CONCURRENT_WRITERS must be an integer"
[[ "$CONCURRENT_MIB" =~ ^[0-9]+$ ]] || die "CRAB_MOUNT_MACOS_CONCURRENT_MIB must be an integer"
[[ "$DIRECTORY_ENTRIES" =~ ^[0-9]+$ ]] || die "CRAB_MOUNT_MACOS_DIRECTORY_ENTRIES must be an integer"
[[ "$FS_OPERATION_TIMEOUT_SECS" =~ ^[1-9][0-9]*$ ]] || die "CRAB_MOUNT_MACOS_FS_OPERATION_TIMEOUT_SECS must be a positive integer"
[[ "$CLEANUP_TIMEOUT_SECS" =~ ^[1-9][0-9]*$ ]] || die "CRAB_MOUNT_MACOS_CLEANUP_TIMEOUT_SECS must be a positive integer"
((SEED_MIB >= 32)) || die "CRAB_MOUNT_MACOS_SEED_MIB must be at least 32"
((NEW_MIB >= 8)) || die "CRAB_MOUNT_MACOS_NEW_MIB must be at least 8"
((CONCURRENT_WRITERS >= 2)) || die "CRAB_MOUNT_MACOS_CONCURRENT_WRITERS must be at least 2"
((CONCURRENT_MIB >= 1)) || die "CRAB_MOUNT_MACOS_CONCURRENT_MIB must be at least 1"

if [[ -z "$EXTERNAL_ENDPOINT" ]]; then
    command -v "$DOCKER" >/dev/null 2>&1 || die "docker is required"
fi
command -v "$AWS" >/dev/null 2>&1 || die "aws CLI is required"
command -v git >/dev/null 2>&1 || die "git is required"
command -v make >/dev/null 2>&1 || die "make is required"
command -v python3 >/dev/null 2>&1 || die "python3 is required"
command -v shasum >/dev/null 2>&1 || die "shasum is required"

mkdir -p "$RUN_ROOT/logs" "$RUN_ROOT/run" "$BIN_DIR" "$TEST_HOME" "$RUN_ROOT/crab-cache"
: > "$TIMINGS_FILE"

printf "run_id=%s\n" "$RUN_ID"
printf "artifact_root=%s\n" "$RUN_ROOT"
if [[ "$BACKEND" == "fuse" ]]; then
    ensure_macfuse_ready
fi
if [[ -z "$EXTERNAL_ENDPOINT" ]]; then
    "$DOCKER" info >/dev/null 2>&1 || die "Docker daemon is not running"
fi

if [[ "${CRAB_MOUNT_MACOS_SKIP_INSTALL:-0}" != "1" ]]; then
    (
        cd "$CRAB_DIR"
        CARGO_PROFILE_RELEASE_LTO=false \
            CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
            CARGO_PROFILE_RELEASE_DEBUG=0 \
            make PREFIX="$BIN_DIR" CARGO_BIN="$BIN_DIR" install
    ) >"$RUN_ROOT/logs/make-install.log" 2>&1
fi

[[ -x "$BIN_DIR/crab" ]] || die "crab binary was not installed into run bin dir"
[[ -x "$BIN_DIR/crab-$BACKEND-mount" ]] || die "crab-$BACKEND-mount was not installed into run bin dir"
[[ -L "$BIN_DIR/git-remote-crab" ]] || die "git-remote-crab symlink was not installed into run bin dir"

with_test_env "$BIN_DIR/crab" --version | tee "$RUN_ROOT/crab-version.txt"
with_test_env "$BIN_DIR/crab-$BACKEND-mount" --version | tee "$RUN_ROOT/crab-$BACKEND-mount-version.txt"
if [[ "$BACKEND" == "fuse" ]]; then
    sed -n '/name = "fuser"/,/dependencies =/s/version = "\(.*\)"/dependency=fuser-\1/p' \
        "$WORKSPACE_ROOT/Cargo.lock" | head -n1 | tee "$RUN_ROOT/fuser-version.txt"
fi
with_test_env "$BIN_DIR/crab" mount --help >"$RUN_ROOT/logs/mount-help.txt"

ENDPOINT_URL="$EXTERNAL_ENDPOINT"
if [[ -z "$ENDPOINT_URL" ]]; then
    "$DOCKER" network create "$NET" >/dev/null
    "$DOCKER" run -d \
        --name "$RUSTFS" \
        --network "$NET" \
        -p 127.0.0.1::9000 \
        -e RUSTFS_ACCESS_KEY=crab \
        -e RUSTFS_SECRET_KEY=crab \
        "$RUSTFS_IMAGE" >/dev/null

    HOST_PORT=""
    for _ in $(seq 1 60); do
        HOST_PORT="$("$DOCKER" port "$RUSTFS" 9000/tcp 2>/dev/null | sed -E 's/.*:([0-9]+)$/\1/' | head -n1 || true)"
        if [[ -n "$HOST_PORT" ]]; then
            if AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab AWS_DEFAULT_REGION="$REGION" AWS_EC2_METADATA_DISABLED=true \
                "$AWS" --endpoint-url "http://127.0.0.1:$HOST_PORT" s3api list-buckets >/dev/null 2>&1; then
                break
            fi
        fi
        sleep 1
    done
    [[ -n "$HOST_PORT" ]] || die "RustFS did not publish a host port"
    ENDPOINT_URL="http://127.0.0.1:$HOST_PORT"
fi

AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab AWS_DEFAULT_REGION="$REGION" AWS_EC2_METADATA_DISABLED=true \
    "$AWS" --endpoint-url "$ENDPOINT_URL" s3api create-bucket --bucket "$BUCKET" >/dev/null 2>&1 || true
AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab AWS_DEFAULT_REGION="$REGION" AWS_EC2_METADATA_DISABLED=true \
    "$AWS" --endpoint-url "$ENDPOINT_URL" s3api head-bucket --bucket "$BUCKET" >/dev/null

export AWS_ENDPOINT_URL="$ENDPOINT_URL"
export AWS_ENDPOINT_URL_S3="$ENDPOINT_URL"
export GIT_TERMINAL_PROMPT=0

printf "rustfs=ready\n"
SCENARIO_START_MS="$(now_ms)"

REMOTE_URL="crab://$BUCKET/mount-large-macos/$RUN_ID"
SEED="$RUN_ROOT/run/seed"
mkdir -p "$SEED" "$RUN_ROOT/run/doctor-mount"
cd "$SEED"
with_test_env "$BIN_DIR/crab" mount doctor --backend "$BACKEND" --mountpoint "$RUN_ROOT/run/doctor-mount" --json \
    >"$RUN_ROOT/mount-doctor.json"
python3 - "$RUN_ROOT/mount-doctor.json" <<'PY'
import json
import pathlib
import sys

payload = json.loads(pathlib.Path(sys.argv[1]).read_text())
if not payload.get("summary", {}).get("ready"):
    raise SystemExit(json.dumps(payload, sort_keys=True))
PY
git init -b main >"$RUN_ROOT/logs/git-init.log"
with_test_env "$BIN_DIR/crab" init "$REMOTE_URL" >"$RUN_ROOT/logs/crab-init.log" 2>&1
with_test_env "$BIN_DIR/crab" track "*.bin" >"$RUN_ROOT/logs/crab-track.log" 2>&1
git add crab.toml .gitattributes
mkdir -p models archive

python3 - "$SEED_MIB" "$RUN_ROOT/seed-model.sha256" <<'PY'
import hashlib
import pathlib
import sys

size = int(sys.argv[1]) * 1024 * 1024
hash_path = pathlib.Path(sys.argv[2])
path = pathlib.Path("models/model.bin")

def stream_bytes(seed, total, fh):
    remaining = total
    counter = 0
    digest = hashlib.sha256()
    while remaining:
        want = min(1024 * 1024, remaining)
        buf = bytearray()
        while len(buf) < want:
            buf.extend(hashlib.sha256(f"{seed}:{counter}".encode()).digest())
            counter += 1
        data = bytes(buf[:want])
        fh.write(data)
        digest.update(data)
        remaining -= want
    return digest.hexdigest()

with path.open("wb") as fh:
    digest = stream_bytes("seed-model", size, fh)
hash_path.write_text(digest + "\n")
PY
cp models/model.bin archive/base-move.bin
cp models/model.bin models/delete-me.bin
cp "$RUN_ROOT/seed-model.sha256" "$RUN_ROOT/base-move.sha256"
cp "$RUN_ROOT/seed-model.sha256" "$RUN_ROOT/delete-me.sha256"

if ((DIRECTORY_ENTRIES > 0)); then
    python3 - "$DIRECTORY_ENTRIES" <<'PY'
import pathlib
import sys

count = int(sys.argv[1])
root = pathlib.Path("large-directory")
root.mkdir()
for index in range(count):
    (root / f"file-{index:05}.txt").write_text(f"entry {index}\n")
PY
    git add large-directory
fi

with_test_env "$BIN_DIR/crab" add --jobs 0 models/model.bin archive/base-move.bin models/delete-me.bin >"$RUN_ROOT/logs/crab-add-seed.log" 2>&1
git show :models/model.bin > "$RUN_ROOT/seed-pointer.txt"
git show :archive/base-move.bin > "$RUN_ROOT/seed-base-move-pointer.txt"
git show :models/delete-me.bin > "$RUN_ROOT/seed-delete-me-pointer.txt"
cmp "$RUN_ROOT/seed-pointer.txt" "$RUN_ROOT/seed-base-move-pointer.txt"
cmp "$RUN_ROOT/seed-pointer.txt" "$RUN_ROOT/seed-delete-me-pointer.txt"
git -c user.email=mount-e2e@example.invalid -c user.name="Crab Mount E2E" \
    commit -m "seed large model" >"$RUN_ROOT/logs/git-commit-seed.log" 2>&1
AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab AWS_DEFAULT_REGION="$REGION" AWS_EC2_METADATA_DISABLED=true \
    "$AWS" --endpoint-url "$ENDPOINT_URL" s3api list-objects-v2 \
    --bucket "$BUCKET" --prefix ".crab/xorbs/" --output json > "$RUN_ROOT/seed-xorbs-before.json"
phase_start_ms="$(now_ms)"
with_test_env "$BIN_DIR/crab" push --json --upload-concurrency 0 origin HEAD:refs/heads/main \
    >"$RUN_ROOT/logs/crab-push-seed.json" 2>"$RUN_ROOT/logs/crab-push-seed.err"
record_duration_since seed_push_ms "$phase_start_ms"
AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab AWS_DEFAULT_REGION="$REGION" AWS_EC2_METADATA_DISABLED=true \
    "$AWS" --endpoint-url "$ENDPOINT_URL" s3api list-objects-v2 \
    --bucket "$BUCKET" --prefix ".crab/xorbs/" --output json > "$RUN_ROOT/seed-xorbs.json"
python3 - "$RUN_ROOT/seed-xorbs-before.json" "$RUN_ROOT/seed-xorbs.json" "$SEED_MIB" "$RUN_ROOT/seed-dedup.json" <<'PY'
import json
import pathlib
import sys

before_path = pathlib.Path(sys.argv[1])
objects_path = pathlib.Path(sys.argv[2])
seed_bytes = int(sys.argv[3]) * 1024 * 1024
output_path = pathlib.Path(sys.argv[4])
before = {
    item["Key"]: int(item.get("Size", 0))
    for item in json.loads(before_path.read_text()).get("Contents", [])
}
objects = {
    item["Key"]: int(item.get("Size", 0))
    for item in json.loads(objects_path.read_text()).get("Contents", [])
}
new_objects = {key: size for key, size in objects.items() if key not in before}
stored_bytes = sum(new_objects.values())
logical_bytes = seed_bytes * 3
if not objects:
    raise SystemExit("seed push did not leave xorb data in the bucket")
if stored_bytes >= seed_bytes * 2:
    raise SystemExit(
        f"three identical files stored {stored_bytes} xorb bytes for {logical_bytes} logical bytes"
    )
payload = {
    "logical_bytes": logical_bytes,
    "stored_xorb_bytes": stored_bytes,
    "xorb_count": len(new_objects),
    "bucket_xorb_bytes": sum(objects.values()),
    "bucket_xorb_count": len(objects),
    "dedup_ratio": 1 - stored_bytes / logical_bytes,
}
output_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
PY
echo "seed_push=ok"

RO="$RUN_ROOT/run/mnt-ro"
mkdir -p "$RO"
phase_start_ms="$(now_ms)"
with_test_env "$BIN_DIR/crab" mount --repo "$REMOTE_URL" --mountpoint "$RO" --ref main --backend "$BACKEND" --read-only --no-refresh \
    >"$RUN_ROOT/logs/mount-ro.log" 2>&1
wait_for_path "$RO/models/model.bin"
record_duration_since ro_mount_ready_ms "$phase_start_ms"
phase_start_ms="$(now_ms)"
capture_nfs_diagnostics ro-before-read "$RO"
if ! hash_file "$RO/models/model.bin" > "$RUN_ROOT/ro-model.sha256"; then
    capture_nfs_diagnostics ro-read-timeout "$RO"
    die "mounted file hash did not complete within ${FS_OPERATION_TIMEOUT_SECS}s"
fi
capture_nfs_diagnostics ro-after-read "$RO"
cmp "$RUN_ROOT/seed-model.sha256" "$RUN_ROOT/ro-model.sha256"

if ((DIRECTORY_ENTRIES > 0)); then
    python3 - "$RO/large-directory" "$DIRECTORY_ENTRIES" "$RUN_ROOT/ro-large-directory.json" <<'PY'
import json
import os
import pathlib
import sys

directory = pathlib.Path(sys.argv[1])
expected = int(sys.argv[2])
names = sorted(entry.name for entry in os.scandir(directory))
if len(names) != expected:
    raise SystemExit(f"expected {expected} directory entries, found {len(names)}")
if names[0] != "file-00000.txt" or names[-1] != f"file-{expected - 1:05}.txt":
    raise SystemExit(f"directory bounds mismatch: {names[0]} {names[-1]}")
sample_indices = sorted(set(range(min(40, expected))) | {expected // 2, expected - 1})
for index in sample_indices:
    path = directory / names[index]
    if not path.is_file():
        raise SystemExit(f"directory entry is not a regular file: {names[index]}")
    if path.read_text() != f"entry {index}\n":
        raise SystemExit(f"directory entry content mismatch: {names[index]}")
pathlib.Path(sys.argv[3]).write_text(json.dumps({
    "count": len(names),
    "first": names[0],
    "last": names[-1],
    "metadata_samples": len(sample_indices),
}, indent=2) + "\n")
PY
fi

RUN_ROOT_ENV="$RUN_ROOT" python3 - <<'PY'
import os
import pathlib

root = pathlib.Path(os.environ["RUN_ROOT_ENV"])
src = root / "run/seed/models/model.bin"
mounted = root / "run/mnt-ro/models/model.bin"
ranges = [
    (0, 4096),
    (8 * 1024 * 1024 - 2048, 8192),
    (15 * 1024 * 1024 + 123, 2 * 1024 * 1024),
    (31 * 1024 * 1024, 1024 * 1024),
]
with src.open("rb") as a, mounted.open("rb") as b:
    for offset, length in ranges:
        a.seek(offset)
        b.seek(offset)
        if a.read(length) != b.read(length):
            raise SystemExit(f"range mismatch at {offset}:{length}")
PY

if RUN_ROOT_ENV="$RUN_ROOT" python3 - <<'PY' >"$RUN_ROOT/logs/ro-write-attempt.log" 2>&1
import os
import pathlib

root = pathlib.Path(os.environ["RUN_ROOT_ENV"])
(root / "run/mnt-ro/models/should-fail.bin").write_bytes(b"nope")
PY
then
    die "read-only write unexpectedly succeeded"
fi
if with_test_env "$BIN_DIR/crab" mount commit --mountpoint "$RO" --message "should fail" --push --json \
    >"$RUN_ROOT/logs/ro-commit-attempt.log" 2>&1; then
    die "read-only commit unexpectedly succeeded"
fi
record_duration_since ro_verify_ms "$phase_start_ms"
with_test_env "$BIN_DIR/crab" unmount --mountpoint "$RO" >"$RUN_ROOT/logs/unmount-ro-explicit.log" 2>&1
echo "read_only_mount=ok"

RW="$RUN_ROOT/run/mnt-rw"
mkdir -p "$RW"
phase_start_ms="$(now_ms)"
with_test_env "$BIN_DIR/crab" mount --repo "$REMOTE_URL" --mountpoint "$RW" --ref main --backend "$BACKEND" --no-refresh \
    >"$RUN_ROOT/logs/mount-rw.log" 2>&1
wait_for_path "$RW/models/model.bin"
record_duration_since rw_mount_ready_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
RUN_ROOT_ENV="$RUN_ROOT" python3 - <<'PY'
import os
import pathlib

root = pathlib.Path(os.environ["RUN_ROOT_ENV"]) / "run/mnt-rw"
with (root / "models/model.bin").open("r+b") as fh:
    fh.write(b"reset-probe")
(root / "models/reset-new.bin").write_bytes(b"reset should discard this file\n")
(root / "models/delete-me.bin").unlink()
PY
with_test_env "$BIN_DIR/crab" mount diff --mountpoint "$RW" --json > "$RUN_ROOT/reset-diff-before.json"
RUN_ROOT_ENV="$RUN_ROOT" python3 - <<'PY'
import os
import pathlib

root = pathlib.Path(os.environ["RUN_ROOT_ENV"])
text = (root / "reset-diff-before.json").read_text()
for path in ["models/model.bin", "models/reset-new.bin", "models/delete-me.bin"]:
    if path not in text:
        raise SystemExit(text)
PY
with_test_env "$BIN_DIR/crab" mount reset --mountpoint "$RW" --overlay --yes --json > "$RUN_ROOT/reset.json"
with_test_env "$BIN_DIR/crab" mount diff --mountpoint "$RW" --json > "$RUN_ROOT/reset-diff-after.json"
RUN_ROOT_ENV="$RUN_ROOT" python3 - <<'PY'
import json
import os
import pathlib

root = pathlib.Path(os.environ["RUN_ROOT_ENV"])
payload = json.loads((root / "reset-diff-after.json").read_text())
changes = payload.get("data", {}).get("diff", {}).get("changes", [])
if changes:
    raise SystemExit(json.dumps(changes, sort_keys=True))
PY
hash_file "$RW/models/model.bin" > "$RUN_ROOT/reset-model.sha256"
hash_file "$RW/models/delete-me.bin" > "$RUN_ROOT/reset-delete-me.sha256"
cmp "$RUN_ROOT/seed-model.sha256" "$RUN_ROOT/reset-model.sha256"
cmp "$RUN_ROOT/delete-me.sha256" "$RUN_ROOT/reset-delete-me.sha256"
[[ ! -e "$RW/models/reset-new.bin" ]]
record_duration_since reset_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
python3 "$SCRIPT_DIR/mount-concurrent-writer-probe.py" write \
    --models "$RW/models" \
    --writers "$CONCURRENT_WRITERS" \
    --mib "$CONCURRENT_MIB" \
    --output "$RUN_ROOT/concurrent-writes.json"
record_duration_since concurrent_overlay_write_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
cp "$RUN_ROOT/run/seed/models/model.bin" "$RUN_ROOT/run/expected-model.bin"
mv "$RW/archive" "$RW/moved-archive"
hash_file "$RW/moved-archive/base-move.bin" > "$RUN_ROOT/rw-base-move.sha256"
cmp "$RUN_ROOT/base-move.sha256" "$RUN_ROOT/rw-base-move.sha256"
[[ ! -e "$RW/archive/base-move.bin" ]]
rm "$RW/models/delete-me.bin"
[[ ! -e "$RW/models/delete-me.bin" ]]

RUN_ROOT_ENV="$RUN_ROOT" NEW_MIB_ENV="$NEW_MIB" python3 - <<'PY'
import hashlib
import os
import pathlib

root = pathlib.Path(os.environ["RUN_ROOT_ENV"])
new_mib = int(os.environ["NEW_MIB_ENV"])
truncate_size = 31 * 1024 * 1024
sparse_size = 48 * 1024 * 1024
patches = [
    (0, 2 * 1024 * 1024, "head"),
    (8 * 1024 * 1024 - 4096, 3 * 1024 * 1024, "boundary"),
    (30 * 1024 * 1024, 2 * 1024 * 1024, "tail"),
]

def make_bytes(seed, size):
    out = bytearray()
    counter = 0
    while len(out) < size:
        out.extend(hashlib.sha256(f"{seed}:{counter}".encode()).digest())
        counter += 1
    return bytes(out[:size])

for target in [
    root / "run/mnt-rw/models/model.bin",
    root / "run/expected-model.bin",
]:
    with target.open("r+b") as fh:
        for offset, length, seed in patches:
            fh.seek(offset)
            fh.write(make_bytes(seed, length))
        fh.truncate(truncate_size)

new_size = new_mib * 1024 * 1024
for target in [
    root / "run/mnt-rw/models/new-large.bin",
    root / "run/expected-new-large.bin",
]:
    with target.open("wb") as fh:
        remaining = new_size
        counter = 0
        while remaining:
            want = min(1024 * 1024, remaining)
            buf = bytearray()
            while len(buf) < want:
                buf.extend(hashlib.sha256(f"new-large:{counter}".encode()).digest())
                counter += 1
            fh.write(bytes(buf[:want]))
            remaining -= want

for target in [
    root / "run/mnt-rw/models/sparse-extend.bin",
    root / "run/expected-sparse-extend.bin",
]:
    with target.open("wb") as fh:
        fh.write(make_bytes("sparse-head", 4096))
        fh.seek(sparse_size - 4096)
        fh.write(make_bytes("sparse-tail", 4096))
PY

ln -s model.bin "$RW/models/model-link.bin"
[[ "$(readlink "$RW/models/model-link.bin")" == "model.bin" ]]
chmod 755 "$RW/models/model.bin" "$RW/models/new-large.bin" "$RW/models/sparse-extend.bin"
[[ "$(stat -f "%Lp" "$RW/models/model.bin")" == "755" ]]
[[ "$(stat -f "%Lp" "$RW/models/new-large.bin")" == "755" ]]
[[ "$(stat -f "%Lp" "$RW/models/sparse-extend.bin")" == "755" ]]
[[ "$(stat -f "%z" "$RW/models/model.bin")" == "$((31 * 1024 * 1024))" ]]
[[ "$(stat -f "%z" "$RW/models/sparse-extend.bin")" == "$((48 * 1024 * 1024))" ]]
mkdir -p "$RW/dir-before/nested"
printf "directory rename content\n" > "$RW/dir-before/nested/note.txt"
mv "$RW/dir-before" "$RW/dir-after"
grep -q "directory rename content" "$RW/dir-after/nested/note.txt"
[[ ! -e "$RW/dir-before/nested/note.txt" ]]

hash_file "$RUN_ROOT/run/expected-model.bin" > "$RUN_ROOT/expected-model.sha256"
hash_file "$RW/models/model.bin" > "$RUN_ROOT/rw-model-before-commit.sha256"
cmp "$RUN_ROOT/expected-model.sha256" "$RUN_ROOT/rw-model-before-commit.sha256"
hash_file "$RUN_ROOT/run/expected-new-large.bin" > "$RUN_ROOT/expected-new-large.sha256"
hash_file "$RW/models/new-large.bin" > "$RUN_ROOT/rw-new-before-commit.sha256"
cmp "$RUN_ROOT/expected-new-large.sha256" "$RUN_ROOT/rw-new-before-commit.sha256"
hash_file "$RUN_ROOT/run/expected-sparse-extend.bin" > "$RUN_ROOT/expected-sparse-extend.sha256"
hash_file "$RW/models/sparse-extend.bin" > "$RUN_ROOT/rw-sparse-before-commit.sha256"
cmp "$RUN_ROOT/expected-sparse-extend.sha256" "$RUN_ROOT/rw-sparse-before-commit.sha256"
record_duration_since overlay_write_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
with_test_env "$BIN_DIR/crab" mount diff --mountpoint "$RW" --json > "$RUN_ROOT/rw-diff.json"
with_test_env "$BIN_DIR/crab" mount status --mountpoint "$RW" --json --verbose > "$RUN_ROOT/rw-status-dirty.json"
RUN_ROOT_ENV="$RUN_ROOT" python3 - <<'PY'
import json
import os
import pathlib

root = pathlib.Path(os.environ["RUN_ROOT_ENV"])
text = (root / "rw-diff.json").read_text()
concurrent = json.loads((root / "concurrent-writes.json").read_text())
expected_paths = [
    "models/model.bin",
    "models/new-large.bin",
    "models/sparse-extend.bin",
    "models/model-link.bin",
    "models/delete-me.bin",
    "moved-archive/base-move.bin",
    "dir-after/nested/note.txt",
]
expected_paths.extend(f"models/{item['path']}" for item in concurrent["files"])
for path in expected_paths:
    if path not in text:
        raise SystemExit(text)
if "symlink" not in text:
    raise SystemExit(text)

status = json.loads((root / "rw-status-dirty.json").read_text())
paths = set(status.get("overlay_dirty_paths", []))
if status.get("overlay_dirty_count", 0) < 1:
    raise SystemExit(json.dumps(status, sort_keys=True))
if "models/delete-me.bin" not in paths:
    raise SystemExit(json.dumps(status, sort_keys=True))
PY
record_duration_since diff_ms "$phase_start_ms"

EXPORT="$RUN_ROOT/run/export"
phase_start_ms="$(now_ms)"
with_test_env "$BIN_DIR/crab" mount export --mountpoint "$RW" --to "$EXPORT" --json > "$RUN_ROOT/rw-export.json"
hash_file "$EXPORT/models/model.bin" > "$RUN_ROOT/export-model.sha256"
hash_file "$EXPORT/models/new-large.bin" > "$RUN_ROOT/export-new-large.sha256"
hash_file "$EXPORT/models/sparse-extend.bin" > "$RUN_ROOT/export-sparse-extend.sha256"
hash_file "$EXPORT/moved-archive/base-move.bin" > "$RUN_ROOT/export-base-move.sha256"
cmp "$RUN_ROOT/expected-model.sha256" "$RUN_ROOT/export-model.sha256"
cmp "$RUN_ROOT/expected-new-large.sha256" "$RUN_ROOT/export-new-large.sha256"
cmp "$RUN_ROOT/expected-sparse-extend.sha256" "$RUN_ROOT/export-sparse-extend.sha256"
cmp "$RUN_ROOT/base-move.sha256" "$RUN_ROOT/export-base-move.sha256"
python3 "$SCRIPT_DIR/mount-concurrent-writer-probe.py" verify-files \
    --manifest "$RUN_ROOT/concurrent-writes.json" \
    --models "$EXPORT/models" \
    --label export
[[ "$(readlink "$EXPORT/models/model-link.bin")" == "model.bin" ]]
grep -q '^archive/base-move.bin$' "$EXPORT/.crab-overlay-deletions"
grep -q '^models/delete-me.bin$' "$EXPORT/.crab-overlay-deletions"
[[ ! -e "$EXPORT/models/delete-me.bin" ]]
record_duration_since export_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
MOUNT_CACHE="$TEST_HOME/.crab/mounts/repos/$(mount_cache_hash "$REMOTE_URL")"
MOUNT_GIT_DIR="$MOUNT_CACHE"
if [[ -d "$MOUNT_CACHE/.git" ]]; then
    MOUNT_GIT_DIR="$MOUNT_CACHE/.git"
fi
[[ -f "$MOUNT_GIT_DIR/HEAD" ]]
git --git-dir "$MOUNT_GIT_DIR" rev-parse refs/heads/main > "$RUN_ROOT/local-ref-before-push-failure.txt"
# Keep the fetch URL healthy for any promised base objects needed while the
# commit is built. A broken push-only URL isolates retry behavior after the
# local commit has actually been created.
git --git-dir "$MOUNT_GIT_DIR" config remote.origin.pushurl "$RUN_ROOT/run/missing-origin.git"
if with_test_env "$BIN_DIR/crab" mount commit --mountpoint "$RW" --message "mount large writeback should retry" --push --json \
    > "$RUN_ROOT/rw-commit-push-failure.json" 2>"$RUN_ROOT/logs/rw-commit-push-failure.err"; then
    die "commit unexpectedly succeeded with broken origin"
fi
git --git-dir "$MOUNT_GIT_DIR" rev-parse refs/heads/main > "$RUN_ROOT/local-ref-after-push-failure.txt"
cmp "$RUN_ROOT/local-ref-before-push-failure.txt" "$RUN_ROOT/local-ref-after-push-failure.txt"
with_test_env "$BIN_DIR/crab" mount diff --mountpoint "$RW" --json > "$RUN_ROOT/rw-diff-after-push-failure.json"
RUN_ROOT_ENV="$RUN_ROOT" python3 - <<'PY'
import json
import os
import pathlib

root = pathlib.Path(os.environ["RUN_ROOT_ENV"])
payload = json.loads((root / "rw-diff-after-push-failure.json").read_text())
changes = payload.get("data", {}).get("diff", {}).get("changes", [])
paths = {change.get("path") for change in changes}
for path in ["models/model.bin", "models/new-large.bin", "models/sparse-extend.bin"]:
    if path not in paths:
        raise SystemExit(json.dumps(changes, sort_keys=True))
PY
python3 - "$MOUNT_CACHE" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
transactions = sorted((root / "publish/transactions").glob("*.json"), key=lambda path: path.stat().st_mtime_ns)
if not transactions:
    raise SystemExit("missing publish transaction")
payload = json.loads(transactions[-1].read_text())
if payload.get("status") != "failed" or payload.get("pushed"):
    raise SystemExit(json.dumps(payload, sort_keys=True))
if not payload.get("commit_oid"):
    raise SystemExit(json.dumps(payload, sort_keys=True))
PY
git --git-dir "$MOUNT_GIT_DIR" config --unset-all remote.origin.pushurl
record_duration_since push_failure_retry_probe_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
with_test_env "$BIN_DIR/crab" mount commit --mountpoint "$RW" --message "mount large writeback" --push --json \
    > "$RUN_ROOT/rw-commit.json" 2>"$RUN_ROOT/logs/rw-commit.err"
with_test_env "$BIN_DIR/crab" mount diff --mountpoint "$RW" --json > "$RUN_ROOT/rw-diff-after-commit.json"
RUN_ROOT_ENV="$RUN_ROOT" python3 - <<'PY'
import json
import os
import pathlib

root = pathlib.Path(os.environ["RUN_ROOT_ENV"])
payload = json.loads((root / "rw-diff-after-commit.json").read_text())
changes = payload.get("data", {}).get("diff", {}).get("changes", [])
if changes:
    raise SystemExit(json.dumps(changes, sort_keys=True))
PY
record_duration_since commit_push_ms "$phase_start_ms"
with_test_env "$BIN_DIR/crab" unmount --mountpoint "$RW" >"$RUN_ROOT/logs/unmount-rw-explicit.log" 2>&1
echo "read_write_mount=ok"

CLONE="$RUN_ROOT/run/clone"
phase_start_ms="$(now_ms)"
with_test_env "$BIN_DIR/crab" clone --branch main --no-lazy "$REMOTE_URL" "$CLONE" --jsonl \
    >"$RUN_ROOT/logs/crab-clone.log" 2>&1
hash_file "$CLONE/models/model.bin" > "$RUN_ROOT/clone-model.sha256"
hash_file "$CLONE/models/new-large.bin" > "$RUN_ROOT/clone-new-large.sha256"
hash_file "$CLONE/models/sparse-extend.bin" > "$RUN_ROOT/clone-sparse-extend.sha256"
hash_file "$CLONE/moved-archive/base-move.bin" > "$RUN_ROOT/clone-base-move.sha256"
cmp "$RUN_ROOT/expected-model.sha256" "$RUN_ROOT/clone-model.sha256"
cmp "$RUN_ROOT/expected-new-large.sha256" "$RUN_ROOT/clone-new-large.sha256"
cmp "$RUN_ROOT/expected-sparse-extend.sha256" "$RUN_ROOT/clone-sparse-extend.sha256"
cmp "$RUN_ROOT/base-move.sha256" "$RUN_ROOT/clone-base-move.sha256"
python3 "$SCRIPT_DIR/mount-concurrent-writer-probe.py" verify-files \
    --manifest "$RUN_ROOT/concurrent-writes.json" \
    --models "$CLONE/models" \
    --label clone
[[ "$(readlink "$CLONE/models/model-link.bin")" == "model.bin" ]]
[[ "$(stat -f "%z" "$CLONE/models/model.bin")" == "$((31 * 1024 * 1024))" ]]
[[ "$(stat -f "%z" "$CLONE/models/sparse-extend.bin")" == "$((48 * 1024 * 1024))" ]]
grep -q "directory rename content" "$CLONE/dir-after/nested/note.txt"
[[ ! -e "$CLONE/dir-before/nested/note.txt" ]]
[[ ! -e "$CLONE/archive/base-move.bin" ]]
[[ ! -e "$CLONE/models/delete-me.bin" ]]
if ((DIRECTORY_ENTRIES > 0)); then
    python3 - "$CLONE/large-directory" "$DIRECTORY_ENTRIES" <<'PY'
import pathlib
import sys

directory = pathlib.Path(sys.argv[1])
expected = int(sys.argv[2])
files = sorted(directory.iterdir())
if len(files) != expected:
    raise SystemExit(f"clone expected {expected} directory entries, found {len(files)}")
if files[0].read_text() != "entry 0\n" or files[-1].read_text() != f"entry {expected - 1}\n":
    raise SystemExit("clone large-directory content mismatch")
PY
fi

cd "$CLONE"
git show HEAD:models/model.bin > "$RUN_ROOT/clone-model-pointer.txt"
git show HEAD:models/new-large.bin > "$RUN_ROOT/clone-new-large-pointer.txt"
git show HEAD:models/sparse-extend.bin > "$RUN_ROOT/clone-sparse-extend-pointer.txt"
git show HEAD:moved-archive/base-move.bin > "$RUN_ROOT/clone-base-move-pointer.txt"
git show HEAD:dir-after/nested/note.txt > "$RUN_ROOT/clone-renamed-dir-note.txt"
git ls-tree HEAD models/model.bin | awk '{print $1}' > "$RUN_ROOT/clone-model-mode.txt"
git ls-tree HEAD models/new-large.bin | awk '{print $1}' > "$RUN_ROOT/clone-new-large-mode.txt"
git ls-tree HEAD models/sparse-extend.bin | awk '{print $1}' > "$RUN_ROOT/clone-sparse-extend-mode.txt"
git ls-tree HEAD moved-archive/base-move.bin | awk '{print $1}' > "$RUN_ROOT/clone-base-move-mode.txt"
git ls-tree HEAD models/model-link.bin | awk '{print $1}' > "$RUN_ROOT/clone-model-link-mode.txt"
grep -q "version https://crab.build/spec/v1" "$RUN_ROOT/clone-model-pointer.txt"
grep -q "version https://crab.build/spec/v1" "$RUN_ROOT/clone-new-large-pointer.txt"
grep -q "version https://crab.build/spec/v1" "$RUN_ROOT/clone-sparse-extend-pointer.txt"
grep -q "version https://crab.build/spec/v1" "$RUN_ROOT/clone-base-move-pointer.txt"
python3 "$SCRIPT_DIR/mount-concurrent-writer-probe.py" verify-pointers \
    --manifest "$RUN_ROOT/concurrent-writes.json" \
    --repo "$CLONE"
grep -q "^100755$" "$RUN_ROOT/clone-model-mode.txt"
grep -q "^100755$" "$RUN_ROOT/clone-new-large-mode.txt"
grep -q "^100755$" "$RUN_ROOT/clone-sparse-extend-mode.txt"
grep -q "^100644$" "$RUN_ROOT/clone-base-move-mode.txt"
grep -q "^120000$" "$RUN_ROOT/clone-model-link-mode.txt"
[[ "$(wc -c < "$RUN_ROOT/clone-model-pointer.txt" | tr -d ' ')" -lt 1000 ]]
[[ "$(wc -c < "$RUN_ROOT/clone-new-large-pointer.txt" | tr -d ' ')" -lt 1000 ]]
[[ "$(wc -c < "$RUN_ROOT/clone-sparse-extend-pointer.txt" | tr -d ' ')" -lt 1000 ]]
[[ "$(wc -c < "$RUN_ROOT/clone-base-move-pointer.txt" | tr -d ' ')" -lt 1000 ]]
[[ -x "$CLONE/models/model.bin" ]]
[[ -x "$CLONE/models/new-large.bin" ]]
[[ -x "$CLONE/models/sparse-extend.bin" ]]
grep -q "directory rename content" "$RUN_ROOT/clone-renamed-dir-note.txt"
if git cat-file -e HEAD:dir-before/nested/note.txt 2>/dev/null; then
    die "renamed directory source path still exists in commit"
fi
if git cat-file -e HEAD:archive/base-move.bin 2>/dev/null; then
    die "base directory rename source path still exists in commit"
fi
if git cat-file -e HEAD:models/delete-me.bin 2>/dev/null; then
    die "deleted large file still exists in commit"
fi
record_duration_since clone_verify_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
with_test_env "$BIN_DIR/crab" dehydrate --all --ignore-profiles --json \
    >"$RUN_ROOT/dehydrate.json"
cmp "$CLONE/models/model.bin" "$RUN_ROOT/clone-model-pointer.txt"
cmp "$CLONE/models/new-large.bin" "$RUN_ROOT/clone-new-large-pointer.txt"
cmp "$CLONE/models/sparse-extend.bin" "$RUN_ROOT/clone-sparse-extend-pointer.txt"
cmp "$CLONE/moved-archive/base-move.bin" "$RUN_ROOT/clone-base-move-pointer.txt"
python3 "$SCRIPT_DIR/mount-concurrent-writer-probe.py" verify-pointers \
    --manifest "$RUN_ROOT/concurrent-writes.json" \
    --models "$CLONE/models"
[[ -x "$CLONE/models/model.bin" ]]
[[ -x "$CLONE/models/new-large.bin" ]]
[[ -x "$CLONE/models/sparse-extend.bin" ]]
[[ ! -x "$CLONE/moved-archive/base-move.bin" ]]
record_duration_since dehydrate_verify_ms "$phase_start_ms"

COLD_HYDRATE_CACHE="$RUN_ROOT/cold-hydrate-cache"
mkdir -p "$COLD_HYDRATE_CACHE"
phase_start_ms="$(now_ms)"
with_cache_env "$COLD_HYDRATE_CACHE" "$BIN_DIR/crab" hydrate --all --jsonl \
    >"$RUN_ROOT/hydrate-cold.jsonl"
hash_file "$CLONE/models/model.bin" > "$RUN_ROOT/rehydrated-model.sha256"
hash_file "$CLONE/models/new-large.bin" > "$RUN_ROOT/rehydrated-new-large.sha256"
hash_file "$CLONE/models/sparse-extend.bin" > "$RUN_ROOT/rehydrated-sparse-extend.sha256"
hash_file "$CLONE/moved-archive/base-move.bin" > "$RUN_ROOT/rehydrated-base-move.sha256"
cmp "$RUN_ROOT/expected-model.sha256" "$RUN_ROOT/rehydrated-model.sha256"
cmp "$RUN_ROOT/expected-new-large.sha256" "$RUN_ROOT/rehydrated-new-large.sha256"
cmp "$RUN_ROOT/expected-sparse-extend.sha256" "$RUN_ROOT/rehydrated-sparse-extend.sha256"
cmp "$RUN_ROOT/base-move.sha256" "$RUN_ROOT/rehydrated-base-move.sha256"
python3 "$SCRIPT_DIR/mount-concurrent-writer-probe.py" verify-files \
    --manifest "$RUN_ROOT/concurrent-writes.json" \
    --models "$CLONE/models" \
    --label rehydrated
[[ -x "$CLONE/models/model.bin" ]]
[[ -x "$CLONE/models/new-large.bin" ]]
[[ -x "$CLONE/models/sparse-extend.bin" ]]
[[ ! -x "$CLONE/moved-archive/base-move.bin" ]]
record_duration_since cold_hydrate_verify_ms "$phase_start_ms"
record_duration_since scenario_total_ms "$SCENARIO_START_MS"

python3 - "$RUN_ID" "$REMOTE_URL" "$RUN_ROOT" "$SEED_MIB" "$NEW_MIB" "$BACKEND" "$DIRECTORY_ENTRIES" <<'PY'
import json
import pathlib
import subprocess
import sys

run_id, remote_url, root_arg, seed_mib, new_mib, backend, directory_entries = sys.argv[1:8]
root = pathlib.Path(root_arg)

def read_timings(path):
    timings = {}
    if not path.exists():
        return timings
    for line in path.read_text().splitlines():
        if not line:
            continue
        name, value = line.split("=", 1)
        timings[name] = int(value)
    return timings

summary = {
    "run_id": run_id,
    "remote_url": remote_url,
    "backend": backend,
    "large_directory_entries": int(directory_entries),
    "seed_mib": int(seed_mib),
    "new_large_mib": int(new_mib),
    "sparse_extend_mib": 48,
    "timings_ms": read_timings(root / "timings.env"),
    "seed_sha256": (root / "seed-model.sha256").read_text().strip(),
    "modified_sha256": (root / "expected-model.sha256").read_text().strip(),
    "new_large_sha256": (root / "expected-new-large.sha256").read_text().strip(),
    "sparse_extend_sha256": (root / "expected-sparse-extend.sha256").read_text().strip(),
    "base_move_sha256": (root / "base-move.sha256").read_text().strip(),
    "deleted_large_sha256": (root / "delete-me.sha256").read_text().strip(),
    "seed_dedup": json.loads((root / "seed-dedup.json").read_text()),
    "concurrent_writes": json.loads((root / "concurrent-writes.json").read_text()),
    "clone_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "model_pointer_bytes": (root / "clone-model-pointer.txt").stat().st_size,
    "new_pointer_bytes": (root / "clone-new-large-pointer.txt").stat().st_size,
    "sparse_extend_pointer_bytes": (root / "clone-sparse-extend-pointer.txt").stat().st_size,
    "base_move_pointer_bytes": (root / "clone-base-move-pointer.txt").stat().st_size,
    "model_mode": (root / "clone-model-mode.txt").read_text().strip(),
    "new_large_mode": (root / "clone-new-large-mode.txt").read_text().strip(),
    "sparse_extend_mode": (root / "clone-sparse-extend-mode.txt").read_text().strip(),
    "base_move_mode": (root / "clone-base-move-mode.txt").read_text().strip(),
    "renamed_dir_note": (root / "clone-renamed-dir-note.txt").read_text().strip(),
    "symlink_mode": (root / "clone-model-link-mode.txt").read_text().strip(),
    "symlink_target": (root / "run/clone/models/model-link.bin").readlink().as_posix(),
}
(root / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
print(json.dumps(summary, sort_keys=True))
PY

echo "macos_mount_large_e2e=ok"

AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab AWS_DEFAULT_REGION="$REGION" AWS_EC2_METADATA_DISABLED=true \
    "$AWS" --endpoint-url "$ENDPOINT_URL" s3api list-objects-v2 \
    --bucket "$BUCKET" --prefix "mount-large-macos/$RUN_ID" --output json > "$RUN_ROOT/objects.json"

python3 - "$RUN_ROOT" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
objects = json.loads((root / "objects.json").read_text())
summary = json.loads((root / "summary.json").read_text())
summary["object_count"] = objects.get("KeyCount", len(objects.get("Contents", [])))
(root / "summary-with-objects.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
print(json.dumps(summary, sort_keys=True))
PY

printf "macos_mount_large_rustfs_smoke=ok\n"
