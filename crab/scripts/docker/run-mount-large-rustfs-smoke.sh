#!/usr/bin/env bash
#
# Run an end-to-end Crab mount smoke against Docker RustFS.
#
# The smoke builds Crab inside a Linux container, starts a local RustFS S3
# endpoint, seeds a Crab repo with a large tracked file, verifies read-only
# mount behavior, verifies writable overlay writeback for large files, pushes
# the overlay commit, then performs a fresh eager clone and byte-for-byte checks.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

DOCKER="${DOCKER:-docker}"
AWS="${AWS:-aws}"
RUN_ID="${CRAB_MOUNT_SMOKE_RUN_ID:-mount-large-$(date -u +%Y%m%d-%H%M%S)}"
ARTIFACT_ROOT="${CRAB_MOUNT_SMOKE_ROOT:-/tmp/crab-mount-large-rustfs-smoke}"
CACHE_ROOT="${CRAB_MOUNT_SMOKE_CACHE_ROOT:-$ARTIFACT_ROOT/cache}"
RUN_ROOT="$ARTIFACT_ROOT/$RUN_ID"
HOST_TARGET="${CRAB_MOUNT_SMOKE_TARGET_CACHE:-$CACHE_ROOT/target}"
HOST_CARGO="${CRAB_MOUNT_SMOKE_CARGO_CACHE:-$CACHE_ROOT/cargo}"
SEED_MIB="${CRAB_MOUNT_SMOKE_SEED_MIB:-32}"
NEW_MIB="${CRAB_MOUNT_SMOKE_NEW_MIB:-40}"
RUST_IMAGE="${CRAB_MOUNT_SMOKE_RUST_IMAGE:-rust:1.91-bookworm}"
RUSTFS_IMAGE="${CRAB_MOUNT_SMOKE_RUSTFS_IMAGE:-rustfs/rustfs:1.0.0-beta.8-glibc}"
BUCKET="${CRAB_MOUNT_SMOKE_BUCKET:-crab}"
REGION="${AWS_REGION:-us-east-1}"

NET="net-$RUN_ID"
RUSTFS="rustfs-$RUN_ID"
RUNNER="runner-$RUN_ID"

die() {
    printf "error: %s\n" "$*" >&2
    exit 1
}

cleanup() {
    "$DOCKER" rm -f "$RUNNER" >/dev/null 2>&1 || true
    "$DOCKER" rm -f "$RUSTFS" >/dev/null 2>&1 || true
    "$DOCKER" network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT

[[ "$SEED_MIB" =~ ^[0-9]+$ ]] || die "CRAB_MOUNT_SMOKE_SEED_MIB must be an integer"
[[ "$NEW_MIB" =~ ^[0-9]+$ ]] || die "CRAB_MOUNT_SMOKE_NEW_MIB must be an integer"
((SEED_MIB >= 32)) || die "CRAB_MOUNT_SMOKE_SEED_MIB must be at least 32"
((NEW_MIB >= 8)) || die "CRAB_MOUNT_SMOKE_NEW_MIB must be at least 8"

command -v "$DOCKER" >/dev/null 2>&1 || die "docker is required"
command -v "$AWS" >/dev/null 2>&1 || die "aws CLI is required on the host"
"$DOCKER" info >/dev/null 2>&1 || die "Docker daemon is not running"

mkdir -p "$RUN_ROOT" "$HOST_TARGET" "$HOST_CARGO"

"$DOCKER" network create "$NET" >/dev/null
"$DOCKER" run -d \
    --name "$RUSTFS" \
    --network "$NET" \
    --network-alias rustfs \
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

AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab AWS_DEFAULT_REGION="$REGION" AWS_EC2_METADATA_DISABLED=true \
    "$AWS" --endpoint-url "http://127.0.0.1:$HOST_PORT" s3api create-bucket --bucket "$BUCKET" >/dev/null 2>&1 || true
AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab AWS_DEFAULT_REGION="$REGION" AWS_EC2_METADATA_DISABLED=true \
    "$AWS" --endpoint-url "http://127.0.0.1:$HOST_PORT" s3api head-bucket --bucket "$BUCKET" >/dev/null

printf "run_id=%s\n" "$RUN_ID"
printf "artifact_root=%s\n" "$RUN_ROOT"
printf "rustfs=ready\n"

"$DOCKER" run --rm -i \
    --name "$RUNNER" \
    --network "$NET" \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --security-opt apparmor:unconfined \
    -v "$REPO_ROOT:/src" \
    -v "$HOST_TARGET:/src/target" \
    -v "$HOST_CARGO:/cargo" \
    -v "$RUN_ROOT:/e2e" \
    -e AWS_ACCESS_KEY_ID=crab \
    -e AWS_SECRET_ACCESS_KEY=crab \
    -e AWS_DEFAULT_REGION="$REGION" \
    -e AWS_REGION="$REGION" \
    -e AWS_ENDPOINT_URL=http://rustfs:9000 \
    -e AWS_ENDPOINT_URL_S3=http://rustfs:9000 \
    -e AWS_ALLOW_HTTP=true \
    -e AWS_EC2_METADATA_DISABLED=true \
    -e AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false \
    -e VIRTUAL_HOSTED_STYLE_REQUEST=false \
    -e CARGO_HOME=/cargo \
    -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=cc \
    -e CARGO_PROFILE_RELEASE_LTO=false \
    -e CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
    -e CARGO_PROFILE_RELEASE_DEBUG=0 \
    "$RUST_IMAGE" bash -s -- "$RUN_ID" "$BUCKET" "$SEED_MIB" "$NEW_MIB" <<'INNER'
set -euo pipefail

RUN_ID="$1"
BUCKET="$2"
SEED_MIB="$3"
NEW_MIB="$4"

export HOME=/e2e/home
export PATH="$HOME/.cargo/bin:/usr/local/cargo/bin:$PATH"
export CRAB_CACHE_DIR=/e2e/crab-cache
export GIT_TERMINAL_PROMPT=0

mkdir -p "$HOME" "$CRAB_CACHE_DIR" /e2e/logs /e2e/run
TIMINGS_FILE=/e2e/timings.env
: > "$TIMINGS_FILE"

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

cleanup_mounts() {
    set +e
    if command -v crab >/dev/null 2>&1; then
        crab unmount --mountpoint /e2e/run/mnt-ro >/e2e/logs/unmount-ro.log 2>&1
        crab unmount --mountpoint /e2e/run/mnt-rw-refresh >/e2e/logs/unmount-rw-refresh.log 2>&1
        crab unmount --mountpoint /e2e/run/mnt-rw >/e2e/logs/unmount-rw.log 2>&1
    fi
    fusermount3 -u /e2e/run/mnt-ro >/dev/null 2>&1
    fusermount3 -u /e2e/run/mnt-rw-refresh >/dev/null 2>&1
    fusermount3 -u /e2e/run/mnt-rw >/dev/null 2>&1
}
trap cleanup_mounts EXIT

export DEBIAN_FRONTEND=noninteractive
apt-get update >/e2e/logs/apt-update.log
apt-get install -y --no-install-recommends \
    fuse3 libfuse3-dev pkg-config make git ca-certificates python3 procps \
    >/e2e/logs/apt-install.log

git config --global --add safe.directory /src
git config --global user.name "Crab Mount E2E"
git config --global user.email "mount-e2e@example.invalid"

cd /src/crab
make install >/e2e/logs/make-install.log 2>&1
crab --version | tee /e2e/crab-version.txt
sed -n '/name = "fuser"/,/dependencies =/s/version = "\(.*\)"/dependency=fuser-\1/p' \
    /src/Cargo.lock | head -n1 | tee /e2e/fuser-version.txt
crab mount --help >/e2e/logs/mount-help.txt
SCENARIO_START_MS="$(now_ms)"

REMOTE_URL="crab://$BUCKET/mount-large/$RUN_ID"
SEED=/e2e/run/seed
mkdir -p "$SEED"
cd "$SEED"
git init -b main >/e2e/logs/git-init.log
crab init "$REMOTE_URL" >/e2e/logs/crab-init.log 2>&1
crab track "*.bin" >/e2e/logs/crab-track.log 2>&1
git add .crab.toml .gitattributes
mkdir -p models archive

python3 - "$SEED_MIB" <<'PY'
import hashlib
import pathlib
import sys

size = int(sys.argv[1]) * 1024 * 1024
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
    h = stream_bytes("seed-model", size, fh)
pathlib.Path("/e2e/seed-model.sha256").write_text(h + "\n")
PY
cp models/model.bin archive/base-move.bin
cp models/model.bin models/delete-me.bin
cp /e2e/seed-model.sha256 /e2e/base-move.sha256
cp /e2e/seed-model.sha256 /e2e/delete-me.sha256

crab add --jobs 0 models/model.bin archive/base-move.bin models/delete-me.bin >/e2e/logs/crab-add-seed.log 2>&1
git show :models/model.bin > /e2e/seed-pointer.txt
git commit -m "seed large model" >/e2e/logs/git-commit-seed.log 2>&1
phase_start_ms="$(now_ms)"
crab push --json --upload-concurrency 0 origin HEAD:refs/heads/main \
    >/e2e/logs/crab-push-seed.json 2>/e2e/logs/crab-push-seed.err
record_duration_since seed_push_ms "$phase_start_ms"
echo "seed_push=ok"

RO=/e2e/run/mnt-ro
mkdir -p "$RO"
phase_start_ms="$(now_ms)"
crab mount --repo "$REMOTE_URL" --mountpoint "$RO" --ref main --read-only --no-refresh \
    >/e2e/logs/mount-ro.log 2>&1
for _ in $(seq 1 60); do
    [[ -e "$RO/models/model.bin" ]] && break
    sleep 1
done
[[ -e "$RO/models/model.bin" ]]
record_duration_since ro_mount_ready_ms "$phase_start_ms"
phase_start_ms="$(now_ms)"
sha256sum "$RO/models/model.bin" | awk '{print $1}' > /e2e/ro-model.sha256
cmp /e2e/seed-model.sha256 /e2e/ro-model.sha256

python3 - <<'PY'
import pathlib

src = pathlib.Path("/e2e/run/seed/models/model.bin")
mounted = pathlib.Path("/e2e/run/mnt-ro/models/model.bin")
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

if python3 - <<'PY' >/e2e/logs/ro-write-attempt.log 2>&1
from pathlib import Path
Path("/e2e/run/mnt-ro/models/should-fail.bin").write_bytes(b"nope")
PY
then
    echo "read-only write unexpectedly succeeded" >&2
    exit 1
fi
if crab mount commit --mountpoint "$RO" --message "should fail" --push --json \
    >/e2e/logs/ro-commit-attempt.log 2>&1; then
    echo "read-only commit unexpectedly succeeded" >&2
    exit 1
fi
record_duration_since ro_verify_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
python3 <<'PY'
import hashlib
import pathlib

size = 8 * 1024 * 1024
path = pathlib.Path("/e2e/run/seed/models/ro-refresh.bin")
digest = hashlib.sha256()
with path.open("wb") as fh:
    remaining = size
    counter = 0
    while remaining:
        want = min(1024 * 1024, remaining)
        buf = bytearray()
        while len(buf) < want:
            buf.extend(hashlib.sha256(f"ro-refresh:{counter}".encode()).digest())
            counter += 1
        data = bytes(buf[:want])
        fh.write(data)
        digest.update(data)
        remaining -= want
pathlib.Path("/e2e/ro-refresh.sha256").write_text(digest.hexdigest() + "\n")
PY
cd "$SEED"
crab add --jobs 0 models/ro-refresh.bin >/e2e/logs/crab-add-ro-refresh.log 2>&1
git commit -m "manual refresh large file" >/e2e/logs/git-commit-ro-refresh.log 2>&1
crab push --json --upload-concurrency 0 origin HEAD:refs/heads/main \
    >/e2e/logs/crab-push-ro-refresh.json 2>/e2e/logs/crab-push-ro-refresh.err
crab mount refresh --mountpoint "$RO" >/e2e/logs/mount-ro-refresh.log 2>&1
for _ in $(seq 1 60); do
    [[ -e "$RO/models/ro-refresh.bin" ]] && break
    sleep 1
done
[[ -e "$RO/models/ro-refresh.bin" ]]
sha256sum "$RO/models/ro-refresh.bin" | awk '{print $1}' > /e2e/ro-refresh-mounted.sha256
cmp /e2e/ro-refresh.sha256 /e2e/ro-refresh-mounted.sha256
record_duration_since ro_manual_refresh_ms "$phase_start_ms"
echo "read_only_refresh=ok"

crab unmount --mountpoint "$RO" >/e2e/logs/unmount-ro-explicit.log 2>&1
echo "read_only_mount=ok"

RWR=/e2e/run/mnt-rw-refresh
mkdir -p "$RWR"
phase_start_ms="$(now_ms)"
crab mount --repo "$REMOTE_URL" --mountpoint "$RWR" --ref main \
    >/e2e/logs/mount-rw-refresh.log 2>&1
for _ in $(seq 1 60); do
    [[ -e "$RWR/models/ro-refresh.bin" ]] && break
    sleep 1
done
[[ -e "$RWR/models/ro-refresh.bin" ]]
record_duration_since rw_refresh_mount_ready_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
python3 <<'PY'
import hashlib
import pathlib

size = 8 * 1024 * 1024
path = pathlib.Path("/e2e/run/seed/models/auto-refresh.bin")
digest = hashlib.sha256()
with path.open("wb") as fh:
    remaining = size
    counter = 0
    while remaining:
        want = min(1024 * 1024, remaining)
        buf = bytearray()
        while len(buf) < want:
            buf.extend(hashlib.sha256(f"auto-refresh:{counter}".encode()).digest())
            counter += 1
        data = bytes(buf[:want])
        fh.write(data)
        digest.update(data)
        remaining -= want
pathlib.Path("/e2e/auto-refresh.sha256").write_text(digest.hexdigest() + "\n")
PY
cd "$SEED"
crab add --jobs 0 models/auto-refresh.bin >/e2e/logs/crab-add-auto-refresh.log 2>&1
git commit -m "automatic refresh large file" >/e2e/logs/git-commit-auto-refresh.log 2>&1
crab push --json --upload-concurrency 0 origin HEAD:refs/heads/main \
    >/e2e/logs/crab-push-auto-refresh.json 2>/e2e/logs/crab-push-auto-refresh.err
sleep 35
for _ in $(seq 1 90); do
    if [[ -e "$RWR/models/auto-refresh.bin" ]]; then
        sha256sum "$RWR/models/auto-refresh.bin" | awk '{print $1}' > /e2e/auto-refresh-mounted.sha256
        if cmp -s /e2e/auto-refresh.sha256 /e2e/auto-refresh-mounted.sha256; then
            break
        fi
    fi
    sleep 1
done
[[ -e "$RWR/models/auto-refresh.bin" ]]
sha256sum "$RWR/models/auto-refresh.bin" | awk '{print $1}' > /e2e/auto-refresh-mounted.sha256
cmp /e2e/auto-refresh.sha256 /e2e/auto-refresh-mounted.sha256
record_duration_since rw_auto_refresh_ms "$phase_start_ms"
crab unmount --mountpoint "$RWR" >/e2e/logs/unmount-rw-refresh-explicit.log 2>&1
echo "read_write_auto_refresh=ok"

RW=/e2e/run/mnt-rw
mkdir -p "$RW"
phase_start_ms="$(now_ms)"
crab mount --repo "$REMOTE_URL" --mountpoint "$RW" --ref main --no-refresh \
    >/e2e/logs/mount-rw.log 2>&1
for _ in $(seq 1 60); do
    [[ -e "$RW/models/model.bin" ]] && break
    sleep 1
done
[[ -e "$RW/models/model.bin" ]]
record_duration_since rw_mount_ready_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
python3 <<'PY'
import pathlib

root = pathlib.Path("/e2e/run/mnt-rw")
with (root / "models/model.bin").open("r+b") as fh:
    fh.write(b"reset-probe")
(root / "models/reset-new.bin").write_bytes(b"reset should discard this file\n")
(root / "models/delete-me.bin").unlink()
PY
crab mount diff --mountpoint "$RW" --json > /e2e/reset-diff-before.json
python3 <<'PY'
import pathlib

text = pathlib.Path("/e2e/reset-diff-before.json").read_text()
for path in ["models/model.bin", "models/reset-new.bin", "models/delete-me.bin"]:
    if path not in text:
        raise SystemExit(text)
PY
crab mount reset --mountpoint "$RW" --overlay --yes --json > /e2e/reset.json
crab mount diff --mountpoint "$RW" --json > /e2e/reset-diff-after.json
python3 <<'PY'
import json
import pathlib

payload = json.loads(pathlib.Path("/e2e/reset-diff-after.json").read_text())
changes = payload.get("data", {}).get("diff", {}).get("changes", [])
if changes:
    raise SystemExit(json.dumps(changes, sort_keys=True))
PY
sha256sum "$RW/models/model.bin" | awk '{print $1}' > /e2e/reset-model.sha256
sha256sum "$RW/models/delete-me.bin" | awk '{print $1}' > /e2e/reset-delete-me.sha256
cmp /e2e/seed-model.sha256 /e2e/reset-model.sha256
cmp /e2e/delete-me.sha256 /e2e/reset-delete-me.sha256
[[ ! -e "$RW/models/reset-new.bin" ]]
record_duration_since reset_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
cp /e2e/run/seed/models/model.bin /e2e/run/expected-model.bin
mv "$RW/archive" "$RW/moved-archive"
sha256sum "$RW/moved-archive/base-move.bin" | awk '{print $1}' > /e2e/rw-base-move.sha256
cmp /e2e/base-move.sha256 /e2e/rw-base-move.sha256
[[ ! -e "$RW/archive/base-move.bin" ]]
rm "$RW/models/delete-me.bin"
[[ ! -e "$RW/models/delete-me.bin" ]]

python3 - "$NEW_MIB" <<'PY'
import hashlib
import pathlib
import sys

new_mib = int(sys.argv[1])
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
    pathlib.Path("/e2e/run/mnt-rw/models/model.bin"),
    pathlib.Path("/e2e/run/expected-model.bin"),
]:
    with target.open("r+b") as fh:
        for offset, length, seed in patches:
            fh.seek(offset)
            fh.write(make_bytes(seed, length))
        fh.truncate(truncate_size)

new_size = new_mib * 1024 * 1024
for target in [
    pathlib.Path("/e2e/run/mnt-rw/models/new-large.bin"),
    pathlib.Path("/e2e/run/expected-new-large.bin"),
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
    pathlib.Path("/e2e/run/mnt-rw/models/sparse-extend.bin"),
    pathlib.Path("/e2e/run/expected-sparse-extend.bin"),
]:
    with target.open("wb") as fh:
        fh.write(make_bytes("sparse-head", 4096))
        fh.seek(sparse_size - 4096)
        fh.write(make_bytes("sparse-tail", 4096))
PY

ln -s model.bin "$RW/models/model-link.bin"
[[ "$(readlink "$RW/models/model-link.bin")" == "model.bin" ]]
chmod 755 "$RW/models/model.bin" "$RW/models/new-large.bin" "$RW/models/sparse-extend.bin"
[[ "$(stat -c "%a" "$RW/models/model.bin")" == "755" ]]
[[ "$(stat -c "%a" "$RW/models/new-large.bin")" == "755" ]]
[[ "$(stat -c "%a" "$RW/models/sparse-extend.bin")" == "755" ]]
[[ "$(stat -c "%s" "$RW/models/model.bin")" == "$((31 * 1024 * 1024))" ]]
[[ "$(stat -c "%s" "$RW/models/sparse-extend.bin")" == "$((48 * 1024 * 1024))" ]]
mkdir -p "$RW/dir-before/nested"
printf "directory rename content\n" > "$RW/dir-before/nested/note.txt"
mv "$RW/dir-before" "$RW/dir-after"
grep -q "directory rename content" "$RW/dir-after/nested/note.txt"
[[ ! -e "$RW/dir-before/nested/note.txt" ]]

sha256sum /e2e/run/expected-model.bin | awk '{print $1}' > /e2e/expected-model.sha256
sha256sum "$RW/models/model.bin" | awk '{print $1}' > /e2e/rw-model-before-commit.sha256
cmp /e2e/expected-model.sha256 /e2e/rw-model-before-commit.sha256
sha256sum /e2e/run/expected-new-large.bin | awk '{print $1}' > /e2e/expected-new-large.sha256
sha256sum "$RW/models/new-large.bin" | awk '{print $1}' > /e2e/rw-new-before-commit.sha256
cmp /e2e/expected-new-large.sha256 /e2e/rw-new-before-commit.sha256
sha256sum /e2e/run/expected-sparse-extend.bin | awk '{print $1}' > /e2e/expected-sparse-extend.sha256
sha256sum "$RW/models/sparse-extend.bin" | awk '{print $1}' > /e2e/rw-sparse-before-commit.sha256
cmp /e2e/expected-sparse-extend.sha256 /e2e/rw-sparse-before-commit.sha256
record_duration_since overlay_write_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
crab mount diff --mountpoint "$RW" --json > /e2e/rw-diff.json
crab mount status --mountpoint "$RW" --json --verbose > /e2e/rw-status-dirty.json
python3 - <<'PY'
import json
import pathlib

text = pathlib.Path("/e2e/rw-diff.json").read_text()
for path in [
    "models/model.bin",
    "models/new-large.bin",
    "models/sparse-extend.bin",
    "models/model-link.bin",
    "models/delete-me.bin",
    "moved-archive/base-move.bin",
    "dir-after/nested/note.txt",
]:
    if path not in text:
        raise SystemExit(text)
if "symlink" not in text:
    raise SystemExit(text)

status = json.loads(pathlib.Path("/e2e/rw-status-dirty.json").read_text())
paths = set(status.get("overlay_dirty_paths", []))
if status.get("overlay_dirty_count", 0) < 1:
    raise SystemExit(json.dumps(status, sort_keys=True))
if "models/delete-me.bin" not in paths:
    raise SystemExit(json.dumps(status, sort_keys=True))
PY
record_duration_since diff_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
EXPORT=/e2e/run/export
crab mount export --mountpoint "$RW" --to "$EXPORT" --json > /e2e/rw-export.json
sha256sum "$EXPORT/models/model.bin" | awk '{print $1}' > /e2e/export-model.sha256
sha256sum "$EXPORT/models/new-large.bin" | awk '{print $1}' > /e2e/export-new-large.sha256
sha256sum "$EXPORT/models/sparse-extend.bin" | awk '{print $1}' > /e2e/export-sparse-extend.sha256
sha256sum "$EXPORT/moved-archive/base-move.bin" | awk '{print $1}' > /e2e/export-base-move.sha256
cmp /e2e/expected-model.sha256 /e2e/export-model.sha256
cmp /e2e/expected-new-large.sha256 /e2e/export-new-large.sha256
cmp /e2e/expected-sparse-extend.sha256 /e2e/export-sparse-extend.sha256
cmp /e2e/base-move.sha256 /e2e/export-base-move.sha256
[[ "$(readlink "$EXPORT/models/model-link.bin")" == "model.bin" ]]
grep -q '^archive/base-move.bin$' "$EXPORT/.crab-overlay-deletions"
grep -q '^models/delete-me.bin$' "$EXPORT/.crab-overlay-deletions"
[[ ! -e "$EXPORT/models/delete-me.bin" ]]
record_duration_since export_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
MOUNT_CACHE="$HOME/.crab/mounts/repos/$(mount_cache_hash "$REMOTE_URL")"
MOUNT_GIT_DIR="$MOUNT_CACHE"
if [[ -d "$MOUNT_CACHE/.git" ]]; then
    MOUNT_GIT_DIR="$MOUNT_CACHE/.git"
fi
[[ -f "$MOUNT_GIT_DIR/HEAD" ]]
git --git-dir "$MOUNT_GIT_DIR" rev-parse refs/heads/main > /e2e/local-ref-before-push-failure.txt
git --git-dir "$MOUNT_GIT_DIR" config remote.origin.url /e2e/run/missing-origin.git
if crab mount commit --mountpoint "$RW" --message "mount large writeback should retry" --push --json \
    > /e2e/rw-commit-push-failure.json 2>/e2e/logs/rw-commit-push-failure.err; then
    echo "commit unexpectedly succeeded with broken origin" >&2
    exit 1
fi
git --git-dir "$MOUNT_GIT_DIR" rev-parse refs/heads/main > /e2e/local-ref-after-push-failure.txt
cmp /e2e/local-ref-before-push-failure.txt /e2e/local-ref-after-push-failure.txt
crab mount diff --mountpoint "$RW" --json > /e2e/rw-diff-after-push-failure.json
python3 - <<'PY'
import json
import pathlib

payload = json.loads(pathlib.Path("/e2e/rw-diff-after-push-failure.json").read_text())
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
git --git-dir "$MOUNT_GIT_DIR" config remote.origin.url "$REMOTE_URL"
record_duration_since push_failure_retry_probe_ms "$phase_start_ms"

phase_start_ms="$(now_ms)"
crab mount commit --mountpoint "$RW" --message "mount large writeback" --push --json \
    > /e2e/rw-commit.json 2>/e2e/logs/rw-commit.err
crab mount diff --mountpoint "$RW" --json > /e2e/rw-diff-after-commit.json
python3 - <<'PY'
import json
import pathlib

payload = json.loads(pathlib.Path("/e2e/rw-diff-after-commit.json").read_text())
changes = payload.get("data", {}).get("diff", {}).get("changes", [])
if changes:
    raise SystemExit(json.dumps(changes, sort_keys=True))
PY
record_duration_since commit_push_ms "$phase_start_ms"
crab unmount --mountpoint "$RW" >/e2e/logs/unmount-rw-explicit.log 2>&1
echo "read_write_mount=ok"

CLONE=/e2e/run/clone
phase_start_ms="$(now_ms)"
crab clone --branch main --no-lazy "$REMOTE_URL" "$CLONE" --jsonl >/e2e/logs/crab-clone.log 2>&1
sha256sum "$CLONE/models/model.bin" | awk '{print $1}' > /e2e/clone-model.sha256
sha256sum "$CLONE/models/ro-refresh.bin" | awk '{print $1}' > /e2e/clone-ro-refresh.sha256
sha256sum "$CLONE/models/auto-refresh.bin" | awk '{print $1}' > /e2e/clone-auto-refresh.sha256
sha256sum "$CLONE/models/new-large.bin" | awk '{print $1}' > /e2e/clone-new-large.sha256
sha256sum "$CLONE/models/sparse-extend.bin" | awk '{print $1}' > /e2e/clone-sparse-extend.sha256
sha256sum "$CLONE/moved-archive/base-move.bin" | awk '{print $1}' > /e2e/clone-base-move.sha256
cmp /e2e/expected-model.sha256 /e2e/clone-model.sha256
cmp /e2e/ro-refresh.sha256 /e2e/clone-ro-refresh.sha256
cmp /e2e/auto-refresh.sha256 /e2e/clone-auto-refresh.sha256
cmp /e2e/expected-new-large.sha256 /e2e/clone-new-large.sha256
cmp /e2e/expected-sparse-extend.sha256 /e2e/clone-sparse-extend.sha256
cmp /e2e/base-move.sha256 /e2e/clone-base-move.sha256
[[ "$(readlink "$CLONE/models/model-link.bin")" == "model.bin" ]]
[[ "$(stat -c "%s" "$CLONE/models/model.bin")" == "$((31 * 1024 * 1024))" ]]
[[ "$(stat -c "%s" "$CLONE/models/sparse-extend.bin")" == "$((48 * 1024 * 1024))" ]]
grep -q "directory rename content" "$CLONE/dir-after/nested/note.txt"
[[ ! -e "$CLONE/dir-before/nested/note.txt" ]]
[[ ! -e "$CLONE/archive/base-move.bin" ]]
[[ ! -e "$CLONE/models/delete-me.bin" ]]

cd "$CLONE"
git show HEAD:models/model.bin > /e2e/clone-model-pointer.txt
git show HEAD:models/new-large.bin > /e2e/clone-new-large-pointer.txt
git show HEAD:models/sparse-extend.bin > /e2e/clone-sparse-extend-pointer.txt
git show HEAD:moved-archive/base-move.bin > /e2e/clone-base-move-pointer.txt
git show HEAD:dir-after/nested/note.txt > /e2e/clone-renamed-dir-note.txt
git ls-tree HEAD models/model.bin | awk '{print $1}' > /e2e/clone-model-mode.txt
git ls-tree HEAD models/new-large.bin | awk '{print $1}' > /e2e/clone-new-large-mode.txt
git ls-tree HEAD models/sparse-extend.bin | awk '{print $1}' > /e2e/clone-sparse-extend-mode.txt
git ls-tree HEAD moved-archive/base-move.bin | awk '{print $1}' > /e2e/clone-base-move-mode.txt
git ls-tree HEAD models/model-link.bin | awk '{print $1}' > /e2e/clone-model-link-mode.txt
grep -q "version https://crab.dev/spec/v1" /e2e/clone-model-pointer.txt
grep -q "version https://crab.dev/spec/v1" /e2e/clone-new-large-pointer.txt
grep -q "version https://crab.dev/spec/v1" /e2e/clone-sparse-extend-pointer.txt
grep -q "version https://crab.dev/spec/v1" /e2e/clone-base-move-pointer.txt
grep -q "^100755$" /e2e/clone-model-mode.txt
grep -q "^100755$" /e2e/clone-new-large-mode.txt
grep -q "^100755$" /e2e/clone-sparse-extend-mode.txt
grep -q "^100644$" /e2e/clone-base-move-mode.txt
grep -q "^120000$" /e2e/clone-model-link-mode.txt
[[ "$(wc -c < /e2e/clone-model-pointer.txt)" -lt 1000 ]]
[[ "$(wc -c < /e2e/clone-new-large-pointer.txt)" -lt 1000 ]]
[[ "$(wc -c < /e2e/clone-sparse-extend-pointer.txt)" -lt 1000 ]]
[[ "$(wc -c < /e2e/clone-base-move-pointer.txt)" -lt 1000 ]]
[[ -x "$CLONE/models/model.bin" ]]
[[ -x "$CLONE/models/new-large.bin" ]]
[[ -x "$CLONE/models/sparse-extend.bin" ]]
grep -q "directory rename content" /e2e/clone-renamed-dir-note.txt
if git cat-file -e HEAD:dir-before/nested/note.txt 2>/dev/null; then
    echo "renamed directory source path still exists in commit" >&2
    exit 1
fi
if git cat-file -e HEAD:archive/base-move.bin 2>/dev/null; then
    echo "base directory rename source path still exists in commit" >&2
    exit 1
fi
if git cat-file -e HEAD:models/delete-me.bin 2>/dev/null; then
    echo "deleted large file still exists in commit" >&2
    exit 1
fi
record_duration_since clone_verify_ms "$phase_start_ms"
record_duration_since scenario_total_ms "$SCENARIO_START_MS"

python3 - "$RUN_ID" "$REMOTE_URL" "$SEED_MIB" "$NEW_MIB" <<'PY'
import json
import pathlib
import subprocess
import sys

run_id, remote_url, seed_mib, new_mib = sys.argv[1:5]

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
    "seed_mib": int(seed_mib),
    "new_large_mib": int(new_mib),
    "sparse_extend_mib": 48,
    "timings_ms": read_timings(pathlib.Path("/e2e/timings.env")),
    "seed_sha256": pathlib.Path("/e2e/seed-model.sha256").read_text().strip(),
    "ro_refresh_sha256": pathlib.Path("/e2e/ro-refresh.sha256").read_text().strip(),
    "auto_refresh_sha256": pathlib.Path("/e2e/auto-refresh.sha256").read_text().strip(),
    "modified_sha256": pathlib.Path("/e2e/expected-model.sha256").read_text().strip(),
    "new_large_sha256": pathlib.Path("/e2e/expected-new-large.sha256").read_text().strip(),
    "sparse_extend_sha256": pathlib.Path("/e2e/expected-sparse-extend.sha256").read_text().strip(),
    "base_move_sha256": pathlib.Path("/e2e/base-move.sha256").read_text().strip(),
    "deleted_large_sha256": pathlib.Path("/e2e/delete-me.sha256").read_text().strip(),
    "clone_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "model_pointer_bytes": pathlib.Path("/e2e/clone-model-pointer.txt").stat().st_size,
    "new_pointer_bytes": pathlib.Path("/e2e/clone-new-large-pointer.txt").stat().st_size,
    "sparse_extend_pointer_bytes": pathlib.Path("/e2e/clone-sparse-extend-pointer.txt").stat().st_size,
    "base_move_pointer_bytes": pathlib.Path("/e2e/clone-base-move-pointer.txt").stat().st_size,
    "model_mode": pathlib.Path("/e2e/clone-model-mode.txt").read_text().strip(),
    "new_large_mode": pathlib.Path("/e2e/clone-new-large-mode.txt").read_text().strip(),
    "sparse_extend_mode": pathlib.Path("/e2e/clone-sparse-extend-mode.txt").read_text().strip(),
    "base_move_mode": pathlib.Path("/e2e/clone-base-move-mode.txt").read_text().strip(),
    "renamed_dir_note": pathlib.Path("/e2e/clone-renamed-dir-note.txt").read_text().strip(),
    "symlink_mode": pathlib.Path("/e2e/clone-model-link-mode.txt").read_text().strip(),
    "symlink_target": pathlib.Path("/e2e/run/clone/models/model-link.bin").readlink().as_posix(),
}
pathlib.Path("/e2e/summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
print(json.dumps(summary, sort_keys=True))
PY

echo "docker_mount_large_e2e=ok"
INNER

AWS_ACCESS_KEY_ID=crab AWS_SECRET_ACCESS_KEY=crab AWS_DEFAULT_REGION="$REGION" AWS_EC2_METADATA_DISABLED=true \
    "$AWS" --endpoint-url "http://127.0.0.1:$HOST_PORT" s3api list-objects-v2 \
    --bucket "$BUCKET" --prefix "mount-large/$RUN_ID" --output json > "$RUN_ROOT/objects.json"

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

printf "docker_mount_large_rustfs_smoke=ok\n"
