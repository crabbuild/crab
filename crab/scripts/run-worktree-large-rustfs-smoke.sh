#!/usr/bin/env bash
# Verify large-file linked-worktree CoW, CDC dedup, push, clone, and hydration.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REAL_CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
REAL_RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
RUN_ID="${CRAB_WORKTREE_RUN_ID:-worktree-cow-$(date -u +%Y%m%d-%H%M%S)}"
RUN_ROOT="${CRAB_WORKTREE_ROOT:-/Volumes/Workspace/CrabCLI/worktree-cow}/$RUN_ID"
BIN_DIR="${CRAB_WORKTREE_BIN_DIR:-$RUN_ROOT/bin}"
CRAB_BIN="${CRAB_WORKTREE_CRAB_BIN:-$BIN_DIR/crab}"
TARGET_DIR="${CARGO_TARGET_DIR:-/Volumes/Workspace/crabbuild-target/crab-worktree-cow-smoke}"
ENDPOINT_URL="${CRAB_WORKTREE_ENDPOINT_URL:-http://127.0.0.1:9000}"
BUCKET="${CRAB_WORKTREE_BUCKET:-crab}"
REGION="${AWS_REGION:-us-east-1}"
FIRST_MIB="${CRAB_WORKTREE_FIRST_MIB:-32}"
SECOND_MIB="${CRAB_WORKTREE_SECOND_MIB:-48}"
LARGE_FILE_COUNT="${CRAB_WORKTREE_LARGE_FILE_COUNT:-3}"
DIRECTORY_ENTRIES="${CRAB_WORKTREE_DIRECTORY_ENTRIES:-1000}"
REMOTE_URL="crab://$BUCKET/verify-cli/$RUN_ID"
TEST_HOME="$RUN_ROOT/home"

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

[[ "$FIRST_MIB" =~ ^[0-9]+$ ]] || die "CRAB_WORKTREE_FIRST_MIB must be an integer"
[[ "$SECOND_MIB" =~ ^[0-9]+$ ]] || die "CRAB_WORKTREE_SECOND_MIB must be an integer"
[[ "$LARGE_FILE_COUNT" =~ ^[0-9]+$ ]] || die "CRAB_WORKTREE_LARGE_FILE_COUNT must be an integer"
[[ "$DIRECTORY_ENTRIES" =~ ^[0-9]+$ ]] || die "CRAB_WORKTREE_DIRECTORY_ENTRIES must be an integer"
((FIRST_MIB >= 32)) || die "CRAB_WORKTREE_FIRST_MIB must be at least 32"
((SECOND_MIB > FIRST_MIB)) || die "CRAB_WORKTREE_SECOND_MIB must exceed FIRST_MIB"
((LARGE_FILE_COUNT >= 3)) || die "CRAB_WORKTREE_LARGE_FILE_COUNT must be at least 3"
[[ ! -e "$RUN_ROOT" ]] || die "run root already exists: $RUN_ROOT"

for command in aws git python3; do
    command -v "$command" >/dev/null 2>&1 || die "missing required command: $command"
done

mkdir -p "$RUN_ROOT/logs" "$BIN_DIR" "$TEST_HOME" "$RUN_ROOT/cache"

with_env() {
    local cache_dir="$1"
    shift
    HOME="$TEST_HOME" \
        PATH="$BIN_DIR:$PATH" \
        CRAB_CACHE_DIR="$cache_dir" \
        AWS_ACCESS_KEY_ID=crab \
        AWS_SECRET_ACCESS_KEY=crab \
        AWS_REGION="$REGION" \
        AWS_DEFAULT_REGION="$REGION" \
        AWS_ENDPOINT_URL="$ENDPOINT_URL" \
        AWS_ENDPOINT_URL_S3="$ENDPOINT_URL" \
        AWS_ALLOW_HTTP=true \
        AWS_EC2_METADATA_DISABLED=true \
        AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false \
        VIRTUAL_HOSTED_STYLE_REQUEST=false \
        GIT_TERMINAL_PROMPT=0 \
        "$@"
}

run_crab() {
    local cwd="$1"
    local cache_dir="$2"
    shift 2
    (cd "$cwd" && with_env "$cache_dir" "$CRAB_BIN" "$@")
}

if [[ "${CRAB_WORKTREE_SKIP_INSTALL:-0}" != "1" ]]; then
    (
        cd "$CRAB_DIR"
        HOME="$TEST_HOME" \
            CARGO_HOME="$REAL_CARGO_HOME" \
            RUSTUP_HOME="$REAL_RUSTUP_HOME" \
            CARGO_TARGET_DIR="$TARGET_DIR" \
            CARGO_PROFILE_RELEASE_LTO=false \
            CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
            CARGO_PROFILE_RELEASE_DEBUG=0 \
            PREFIX="$BIN_DIR" \
            make install
    ) >"$RUN_ROOT/logs/make-install.log" 2>&1
fi
[[ -x "$CRAB_BIN" ]] || die "Crab binary is unavailable: $CRAB_BIN"
[[ -x "$BIN_DIR/git-remote-crab" ]] || die "git-remote-crab is unavailable in $BIN_DIR"

with_env "$RUN_ROOT/cache/preflight" aws --endpoint-url "$ENDPOINT_URL" \
    s3api head-bucket --bucket "$BUCKET" >/dev/null || die "RustFS bucket is unavailable"
with_env "$RUN_ROOT/cache/preflight" "$CRAB_BIN" --version | tee "$RUN_ROOT/crab-version.txt"

SEED="$RUN_ROOT/seed"
CLONE="$RUN_ROOT/clone"
POINTER_WORKTREE="$RUN_ROOT/worktree-pointer"
FULL_WORKTREE="$RUN_ROOT/worktree-full"
FRESH="$RUN_ROOT/fresh"
mkdir -p "$SEED"
git -C "$SEED" init -b main >"$RUN_ROOT/logs/git-init.log"
git -C "$SEED" config user.email verify@crab.local
git -C "$SEED" config user.name "Crab Worktree Verify"
run_crab "$SEED" "$RUN_ROOT/cache/seed" init "$REMOTE_URL" \
    >"$RUN_ROOT/logs/crab-init.log" 2>&1
run_crab "$SEED" "$RUN_ROOT/cache/seed" track '*.bin' \
    >"$RUN_ROOT/logs/crab-track.log" 2>&1

python3 - "$SEED" "$FIRST_MIB" "$SECOND_MIB" "$LARGE_FILE_COUNT" \
    "$DIRECTORY_ENTRIES" "$RUN_ROOT/hashes.json" <<'PY'
import hashlib
import json
import os
import pathlib
import shutil
import stat
import sys

root = pathlib.Path(sys.argv[1])
first_size = int(sys.argv[2]) * 1024 * 1024
second_size = int(sys.argv[3]) * 1024 * 1024
large_count = int(sys.argv[4])
entry_count = int(sys.argv[5])
manifest_path = pathlib.Path(sys.argv[6])
models = root / "models"
models.mkdir()

def append_deterministic(path, seed, size, append=False):
    mode = "ab" if append else "wb"
    remaining = size
    counter = 0
    with path.open(mode) as handle:
        while remaining:
            block = bytearray()
            want = min(1024 * 1024, remaining)
            while len(block) < want:
                block.extend(hashlib.sha256(f"{seed}:{counter}".encode()).digest())
                counter += 1
            handle.write(block[:want])
            remaining -= want

first = models / "model-00.bin"
second = models / "model-01.bin"
append_deterministic(first, "shared-prefix", first_size)
shutil.copyfile(first, second)
append_deterministic(second, "second-tail", second_size - first_size, append=True)
for index in range(2, large_count):
    source = first if index % 2 == 0 else second
    shutil.copyfile(source, models / f"model-{index:02}.bin")

os.chmod(first, 0o755)
for path in models.glob("*.bin"):
    if path != first:
        os.chmod(path, 0o644)

entries = root / "large-directory"
entries.mkdir()
for index in range(entry_count):
    (entries / f"entry-{index:06}.txt").write_text(f"entry {index}\n")

manifest = {}
for path in sorted(models.glob("*.bin")):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    manifest[path.name] = {
        "sha256": digest.hexdigest(),
        "size": path.stat().st_size,
        "mode": stat.S_IMODE(path.stat().st_mode),
    }
manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
PY

run_crab "$SEED" "$RUN_ROOT/cache/seed" add 'models/*.bin' --json \
    >"$RUN_ROOT/add.json"
run_crab "$SEED" "$RUN_ROOT/cache/seed" stat push-plan --verify --json \
    >"$RUN_ROOT/push-plan.json"
python3 - "$RUN_ROOT/push-plan.json" "$RUN_ROOT/hashes.json" "$LARGE_FILE_COUNT" <<'PY'
import json
import pathlib
import sys

payload = json.loads(pathlib.Path(sys.argv[1]).read_text())
data = payload.get("data", payload)
manifest = json.loads(pathlib.Path(sys.argv[2]).read_text())
file_count = int(sys.argv[3])
if data.get("prepared_xorbs", 0) <= 0 or data.get("prepared_chunks", 0) <= 0:
    raise SystemExit(f"expected prepared chunks and xorbs: {data}")
if data.get("plan_files", file_count) >= file_count:
    raise SystemExit(f"expected exact large-file dedup to collapse a push plan: {data}")
if data.get("planned_file_bytes", sum(item["size"] for item in manifest.values())) >= sum(
    item["size"] for item in manifest.values()
):
    raise SystemExit(f"expected dedup to reduce planned file bytes: {data}")
PY
git -C "$SEED" add .crab.toml .gitattributes large-directory
git -C "$SEED" commit -m "seed large worktree fixture" >"$RUN_ROOT/logs/git-commit.log"
run_crab "$SEED" "$RUN_ROOT/cache/seed" push >"$RUN_ROOT/logs/crab-push.log" 2>&1
with_env "$RUN_ROOT/cache/preflight" aws --endpoint-url "$ENDPOINT_URL" \
    s3api list-objects-v2 --bucket "$BUCKET" --prefix '.crab/xorbs/' --max-items 10 \
    >"$RUN_ROOT/remote-xorbs.json"
with_env "$RUN_ROOT/cache/preflight" aws --endpoint-url "$ENDPOINT_URL" \
    s3api list-objects-v2 --bucket "$BUCKET" --prefix '.crab/shards/' --max-items 10 \
    >"$RUN_ROOT/remote-shards.json"
python3 - "$RUN_ROOT/remote-xorbs.json" "$RUN_ROOT/remote-shards.json" <<'PY'
import json
import pathlib
import sys

for path in sys.argv[1:]:
    payload = json.loads(pathlib.Path(path).read_text())
    if payload.get("KeyCount", 0) <= 0 and not payload.get("Contents"):
        raise SystemExit(f"expected remote content objects: {path}")
PY

run_crab "$RUN_ROOT" "$RUN_ROOT/cache/clone" clone "$REMOTE_URL" "$CLONE" \
    >"$RUN_ROOT/logs/crab-clone.log" 2>&1
python3 - "$CLONE" "$LARGE_FILE_COUNT" <<'PY'
import pathlib
import sys

root = pathlib.Path(sys.argv[1]) / "models"
for index in range(int(sys.argv[2])):
    data = (root / f"model-{index:02}.bin").read_bytes()
    if len(data) > 256 or not data.startswith(b"version https://crab.dev/spec/v1\n"):
        raise SystemExit(f"expected pointer file: model-{index:02}.bin")
PY
run_crab "$CLONE" "$RUN_ROOT/cache/clone" hydrate --all --json \
    >"$RUN_ROOT/clone-hydrate.json"

verify_materialized() {
    python3 - "$1" "$RUN_ROOT/hashes.json" <<'PY'
import hashlib
import json
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1]) / "models"
manifest = json.loads(pathlib.Path(sys.argv[2]).read_text())
for name, expected in manifest.items():
    path = root / name
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    actual = (digest.hexdigest(), path.stat().st_size, stat.S_IMODE(path.stat().st_mode))
    wanted = (expected["sha256"], expected["size"], expected["mode"])
    if actual != wanted:
        raise SystemExit(f"materialization mismatch for {name}: {actual} != {wanted}")
PY
}
verify_materialized "$CLONE"

run_crab "$CLONE" "$RUN_ROOT/cache/clone" worktree add -b cow-edit \
    --hydrate=pointer-only "$POINTER_WORKTREE" HEAD \
    >"$RUN_ROOT/logs/worktree-pointer-add.log" 2>&1
run_crab "$POINTER_WORKTREE" "$RUN_ROOT/cache/pointer-worktree" hydrate --all --json \
    >"$RUN_ROOT/worktree-cow-hydrate.json"
python3 - "$RUN_ROOT/worktree-cow-hydrate.json" "$LARGE_FILE_COUNT" <<'PY'
import json
import pathlib
import sys

payload = json.loads(pathlib.Path(sys.argv[1]).read_text())
data = payload.get("data", payload)
expected = int(sys.argv[2])
if data.get("cow_cloned") != expected:
    raise SystemExit(f"expected {expected} verified CoW clones: {data}")
if data.get("bytes_cow_cloned", 0) < 32 * 1024 * 1024:
    raise SystemExit(f"CoW byte counter is too small: {data}")
PY
verify_materialized "$POINTER_WORKTREE"

run_crab "$POINTER_WORKTREE" "$RUN_ROOT/cache/pointer-worktree" dehydrate --all --json \
    >"$RUN_ROOT/worktree-dehydrate.json"
run_crab "$POINTER_WORKTREE" "$RUN_ROOT/cache/pointer-worktree" hydrate --all --json \
    >"$RUN_ROOT/worktree-rehydrate.json"
verify_materialized "$POINTER_WORKTREE"

run_crab "$CLONE" "$RUN_ROOT/cache/clone" worktree add --detach --hydrate=full \
    "$FULL_WORKTREE" HEAD >"$RUN_ROOT/logs/worktree-full-add.log" 2>&1
verify_materialized "$FULL_WORKTREE"

python3 - "$POINTER_WORKTREE/models/model-01.bin" "$RUN_ROOT/edited.sha256" <<'PY'
import hashlib
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
with path.open("r+b") as handle:
    handle.seek(path.stat().st_size // 2)
    handle.write(b"Crab linked worktree independent edit\n")
digest = hashlib.sha256(path.read_bytes()).hexdigest()
pathlib.Path(sys.argv[2]).write_text(digest + "\n")
PY
run_crab "$POINTER_WORKTREE" "$RUN_ROOT/cache/pointer-worktree" add models/model-01.bin --json \
    >"$RUN_ROOT/edit-add.json"
run_crab "$POINTER_WORKTREE" "$RUN_ROOT/cache/pointer-worktree" stat push-plan --verify --json \
    >"$RUN_ROOT/edit-push-plan.json"
python3 - "$RUN_ROOT/edit-add.json" <<'PY'
import json
import pathlib
import sys

payload = json.loads(pathlib.Path(sys.argv[1]).read_text())
data = payload.get("data", payload)
if data.get("files_staged") != 1 or data.get("chunks_staged", 0) <= 1:
    raise SystemExit(f"expected the edited large file to pass through CDC staging: {data}")
PY
verify_materialized "$CLONE"
verify_materialized "$FULL_WORKTREE"
git -C "$POINTER_WORKTREE" commit -m "edit large file in linked worktree" \
    >"$RUN_ROOT/logs/edit-commit.log"
run_crab "$POINTER_WORKTREE" "$RUN_ROOT/cache/pointer-worktree" push \
    >"$RUN_ROOT/logs/edit-push.log" 2>&1

run_crab "$RUN_ROOT" "$RUN_ROOT/cache/fresh" clone --branch cow-edit "$REMOTE_URL" "$FRESH" \
    >"$RUN_ROOT/logs/fresh-clone.log" 2>&1
run_crab "$FRESH" "$RUN_ROOT/cache/fresh-cold" hydrate --all --json \
    >"$RUN_ROOT/fresh-cold-hydrate.json"
EXPECTED_EDITED="$(tr -d '\n' <"$RUN_ROOT/edited.sha256")"
ACTUAL_EDITED="$(python3 - "$FRESH/models/model-01.bin" <<'PY'
import hashlib
import pathlib
import sys
print(hashlib.sha256(pathlib.Path(sys.argv[1]).read_bytes()).hexdigest())
PY
)"
[[ "$ACTUAL_EDITED" == "$EXPECTED_EDITED" ]] || die "fresh cold hydrate hash mismatch"

run_crab "$POINTER_WORKTREE" "$RUN_ROOT/cache/pointer-worktree" status --json \
    >"$RUN_ROOT/worktree-status.json"
run_crab "$CLONE" "$RUN_ROOT/cache/clone" worktree list --json --with-crab-state \
    >"$RUN_ROOT/worktree-list.json"

printf 'ok: worktree CoW large-file RustFS lifecycle passed\n'
printf 'run_id=%s\nremote=%s\nartifacts=%s\n' "$RUN_ID" "$REMOTE_URL" "$RUN_ROOT"
