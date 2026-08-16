#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: create-rustfs-cli-fixture.sh [options] [-- <crab args...>]

Creates a disposable RustFS-backed Crab repo with deterministic large files,
pushes it, clones it, hydrates it, verifies byte identity, and optionally runs
one Crab CLI command in the fixture.

Options:
  --run-id ID             Run id. Default: cli-verify-<UTC timestamp>.
  --root DIR              Fixture root. Default: /Volumes/Workspace/CrabCLI.
  --bucket NAME           RustFS bucket. Default: crab.
  --endpoint-url URL      RustFS S3 endpoint. Default: http://127.0.0.1:9000.
  --size-mib N            Size of each generated .bin file. Default: 32.
  --crab-bin PATH         Crab binary to run. Default: crab.
  --command-cwd WHERE     Where to run optional command: clone, seed, run-root. Default: clone.
  -h, --help              Show this help.

Examples:
  create-rustfs-cli-fixture.sh
  create-rustfs-cli-fixture.sh --command-cwd clone -- status --json
  create-rustfs-cli-fixture.sh --command-cwd seed -- fsck --json
USAGE
}

run_id="cli-verify-$(date -u +%Y%m%d-%H%M%S)"
root="/Volumes/Workspace/CrabCLI"
bucket="crab"
endpoint_url="http://127.0.0.1:9000"
size_mib=32
crab_bin="crab"
command_cwd="clone"
declare -a cli_args=()

while (($#)); do
  case "$1" in
    --run-id)
      run_id="${2:?missing value for --run-id}"
      shift
      ;;
    --root)
      root="${2:?missing value for --root}"
      shift
      ;;
    --bucket)
      bucket="${2:?missing value for --bucket}"
      shift
      ;;
    --endpoint-url)
      endpoint_url="${2:?missing value for --endpoint-url}"
      shift
      ;;
    --size-mib)
      size_mib="${2:?missing value for --size-mib}"
      shift
      ;;
    --crab-bin)
      crab_bin="${2:?missing value for --crab-bin}"
      shift
      ;;
    --command-cwd)
      command_cwd="${2:?missing value for --command-cwd}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      cli_args=("$@")
      break
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 127
  fi
}

step() {
  printf '\n==> %s\n' "$*"
}

hash_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

run_logged() {
  local name="$1"
  local cwd="$2"
  shift 2
  local slug
  slug="$(printf '%s' "$name" | tr -cs 'A-Za-z0-9._-' '-' | sed -E 's/^-+|-+$//g')"
  [[ -n "$slug" ]] || slug="command"
  step "$name"
  (
    cd "$cwd"
    "$@"
  ) >"$logs/$slug.stdout.log" 2>"$logs/$slug.stderr.log"
}

case "$command_cwd" in
  clone|seed|run-root) ;;
  *)
    echo "--command-cwd must be one of: clone, seed, run-root" >&2
    exit 2
    ;;
esac

if [[ ! "$size_mib" =~ ^[0-9]+$ ]] || ((size_mib < 1)); then
  echo "--size-mib must be a positive integer" >&2
  exit 2
fi

need git
need aws
need python3
need "$crab_bin"

crab_path="$(command -v "$crab_bin" || true)"
if [[ -n "$crab_path" ]]; then
  export PATH="$(dirname "$crab_path"):$PATH"
fi
need git-remote-crab

run_root="$root/$run_id"
logs="$run_root/logs"
seed="$run_root/seed"
clone="$run_root/clone"
cache="$run_root/crab-cache"
remote_prefix="verify-cli/$run_id"
remote_url="crab://$bucket/$remote_prefix"

mkdir -p "$logs" "$seed" "$clone" "$cache"

export AWS_ACCESS_KEY_ID=crab
export AWS_SECRET_ACCESS_KEY=crab
export AWS_REGION=us-east-1
export AWS_DEFAULT_REGION=us-east-1
export AWS_ENDPOINT_URL="$endpoint_url"
export AWS_ENDPOINT_URL_S3="$endpoint_url"
export AWS_ALLOW_HTTP=true
export AWS_EC2_METADATA_DISABLED=true
export AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false
export VIRTUAL_HOSTED_STYLE_REQUEST=false
export GIT_TERMINAL_PROMPT=0
export CRAB_CACHE_DIR="$cache"

cat >"$run_root/env.sh" <<EOF
export AWS_ACCESS_KEY_ID=crab
export AWS_SECRET_ACCESS_KEY=crab
export AWS_REGION=us-east-1
export AWS_DEFAULT_REGION=us-east-1
export AWS_ENDPOINT_URL=$endpoint_url
export AWS_ENDPOINT_URL_S3=$endpoint_url
export AWS_ALLOW_HTTP=true
export AWS_EC2_METADATA_DISABLED=true
export AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false
export VIRTUAL_HOSTED_STYLE_REQUEST=false
export GIT_TERMINAL_PROMPT=0
export CRAB_CACHE_DIR=$cache
export CRAB_VERIFY_RUN_ROOT=$run_root
export CRAB_VERIFY_SEED_REPO=$seed
export CRAB_VERIFY_CLONE_REPO=$clone
export CRAB_VERIFY_REMOTE_URL=$remote_url
EOF

step "preflight RustFS endpoint"
aws --endpoint-url "$endpoint_url" s3api list-buckets >/dev/null
aws --endpoint-url "$endpoint_url" s3api create-bucket --bucket "$bucket" \
  >"$logs/aws-create-bucket.stdout.log" 2>"$logs/aws-create-bucket.stderr.log" || true
aws --endpoint-url "$endpoint_url" s3api head-bucket --bucket "$bucket" >/dev/null

step "preflight RustFS conditional-write contract"
probe="$run_root/probe.txt"
printf 'probe %s\n' "$run_id" >"$probe"
probe_key="$remote_prefix/cas-probe"
if aws s3api put-object help 2>/dev/null | grep -q -- "--if-none-match"; then
  aws --endpoint-url "$endpoint_url" s3api put-object \
    --bucket "$bucket" --key "$probe_key" --body "$probe" --if-none-match '*' >/dev/null
  if aws --endpoint-url "$endpoint_url" s3api put-object \
    --bucket "$bucket" --key "$probe_key" --body "$probe" --if-none-match '*' \
    >"$logs/aws-if-none-match-conflict.stdout.log" 2>"$logs/aws-if-none-match-conflict.stderr.log"; then
    echo "RustFS accepted duplicate If-None-Match create" >&2
    exit 1
  fi
  etag="$(aws --endpoint-url "$endpoint_url" s3api head-object \
    --bucket "$bucket" --key "$probe_key" --query ETag --output text | tr -d '"')"
  aws --endpoint-url "$endpoint_url" s3api put-object \
    --bucket "$bucket" --key "$probe_key" --body "$probe" --if-match "$etag" >/dev/null
  if aws --endpoint-url "$endpoint_url" s3api put-object \
    --bucket "$bucket" --key "$probe_key" --body "$probe" --if-match deadbeef00000000000000000000dead \
    >"$logs/aws-if-match-conflict.stdout.log" 2>"$logs/aws-if-match-conflict.stderr.log"; then
    echo "RustFS accepted wrong If-Match update" >&2
    exit 1
  fi
else
  need curl
  endpoint_base="${endpoint_url%/}"
  probe_url="$endpoint_base/$bucket/$probe_key"
  curl_auth=(--aws-sigv4 "aws:amz:$AWS_REGION:s3" --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY")

  curl -sS --fail-with-body -D "$logs/curl-if-none-match-create.headers" \
    -o "$logs/curl-if-none-match-create.body" "${curl_auth[@]}" \
    -X PUT -H "If-None-Match: *" --upload-file "$probe" "$probe_url" >/dev/null
  if curl -sS --fail-with-body -D "$logs/curl-if-none-match-conflict.headers" \
    -o "$logs/curl-if-none-match-conflict.body" "${curl_auth[@]}" \
    -X PUT -H "If-None-Match: *" --upload-file "$probe" "$probe_url" >/dev/null; then
    echo "RustFS accepted duplicate If-None-Match create" >&2
    exit 1
  fi
  etag="$(awk 'BEGIN{IGNORECASE=1} /^etag:/ {gsub(/\r/, "", $2); gsub(/"/, "", $2); print $2; exit}' \
    "$logs/curl-if-none-match-create.headers")"
  if [[ -z "$etag" ]]; then
    echo "RustFS conditional create did not return an ETag" >&2
    exit 1
  fi
  curl -sS --fail-with-body -D "$logs/curl-if-match.headers" \
    -o "$logs/curl-if-match.body" "${curl_auth[@]}" \
    -X PUT -H "If-Match: \"$etag\"" --upload-file "$probe" "$probe_url" >/dev/null
  if curl -sS --fail-with-body -D "$logs/curl-if-match-conflict.headers" \
    -o "$logs/curl-if-match-conflict.body" "${curl_auth[@]}" \
    -X PUT -H 'If-Match: "deadbeef00000000000000000000dead"' \
    --upload-file "$probe" "$probe_url" >/dev/null; then
    echo "RustFS accepted wrong If-Match update" >&2
    exit 1
  fi
fi

step "create seed repo"
git -C "$seed" init -b main >"$logs/git-init.stdout.log" 2>"$logs/git-init.stderr.log"
git -C "$seed" config user.name "Crab CLI Verify"
git -C "$seed" config user.email "crab-cli-verify@example.invalid"
run_logged "crab init" "$seed" "$crab_bin" init "$remote_url"
run_logged "crab track" "$seed" "$crab_bin" track "*.bin"
git -C "$seed" add .crab.toml .gitattributes
mkdir -p "$seed/data"

step "write deterministic large files"
python3 - "$size_mib" "$seed/data/model-a.bin" "$seed/data/model-b.bin" "$run_root/original.sha256" <<'PY'
import hashlib
import pathlib
import sys

size = int(sys.argv[1]) * 1024 * 1024
paths = [pathlib.Path(sys.argv[2]), pathlib.Path(sys.argv[3])]
manifest = pathlib.Path(sys.argv[4])

def write_bytes(path: pathlib.Path, seed: str) -> str:
    remaining = size
    counter = 0
    digest = hashlib.sha256()
    with path.open("wb") as fh:
        while remaining:
            want = min(1024 * 1024, remaining)
            buf = bytearray()
            while len(buf) < want:
                buf.extend(hashlib.sha256(f"{seed}:{counter}".encode()).digest())
                counter += 1
            chunk = bytes(buf[:want])
            fh.write(chunk)
            digest.update(chunk)
            remaining -= want
    return digest.hexdigest()

lines = []
for path in paths:
    lines.append(f"{write_bytes(path, path.name)}  {path.name}\n")
manifest.write_text("".join(lines), encoding="utf-8")
PY

run_logged "crab add" "$seed" "$crab_bin" add --jobs 0 data/model-a.bin data/model-b.bin
git -C "$seed" show :data/model-a.bin >"$run_root/model-a.pointer"
grep -q "version https://crab.dev/spec/v1" "$run_root/model-a.pointer"
git -C "$seed" commit -m "verify CLI fixture $run_id" >"$logs/git-commit.stdout.log" 2>"$logs/git-commit.stderr.log"
run_logged "crab push" "$seed" "$crab_bin" push --json --upload-concurrency 0 origin HEAD:refs/heads/main

step "clone and hydrate"
rmdir "$clone"
run_logged "crab clone" "$run_root" "$crab_bin" clone "$remote_url" "$clone" --jsonl
run_logged "crab hydrate" "$clone" "$crab_bin" hydrate --all

step "verify hydrated bytes"
while read -r expected file; do
  actual="$(hash_file "$clone/data/$file")"
  if [[ "$actual" != "$expected" ]]; then
    echo "hash mismatch for $file: expected $expected got $actual" >&2
    exit 1
  fi
done <"$run_root/original.sha256"

if ((${#cli_args[@]} > 0)); then
  case "$command_cwd" in
    clone) cmd_cwd="$clone" ;;
    seed) cmd_cwd="$seed" ;;
    run-root) cmd_cwd="$run_root" ;;
  esac
  run_logged "crab command under test" "$cmd_cwd" "$crab_bin" "${cli_args[@]}"
fi

cat >"$run_root/summary.txt" <<EOF
run_id=$run_id
remote_url=$remote_url
run_root=$run_root
seed_repo=$seed
clone_repo=$clone
logs=$logs
env=$run_root/env.sh
EOF

cat "$run_root/summary.txt"
printf '\nCrab RustFS CLI fixture passed.\n'
