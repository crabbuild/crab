#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: bench-add-local.sh [--files N] [--size SIZE] [--jobs N] [--runs N] [--mode unique|duplicate] [--crab PATH] [--generator PATH]

Runs a local no-remote `crab add --json` benchmark in a temporary git repo.

Defaults:
  --files      8
  --size       64M
  --jobs       omitted, so crab uses its CLI default
  --runs       1
  --mode       unique
  --crab       target/release/crab from the workspace root
  --generator  target/release/generate_test_file from the workspace root

Set KEEP_BENCH_DIR=1 to keep the temporary repo for inspection.
USAGE
}

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd -- "$script_dir/../.." && pwd)"

files=8
size=64M
jobs=
runs=1
mode=unique
crab_bin="$workspace_root/target/release/crab"
generator_bin="$workspace_root/target/release/generate_test_file"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --files)
      files="${2:?missing --files value}"
      shift 2
      ;;
    --size)
      size="${2:?missing --size value}"
      shift 2
      ;;
    --jobs)
      jobs="${2:?missing --jobs value}"
      shift 2
      ;;
    --runs)
      runs="${2:?missing --runs value}"
      shift 2
      ;;
    --mode)
      mode="${2:?missing --mode value}"
      shift 2
      ;;
    --crab)
      crab_bin="${2:?missing --crab value}"
      shift 2
      ;;
    --generator)
      generator_bin="${2:?missing --generator value}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

case "$mode" in
  unique|duplicate)
    ;;
  *)
    echo "invalid --mode: $mode" >&2
    usage >&2
    exit 2
    ;;
esac

case "$files:$runs" in
  *[!0-9:]*|0:*|*:0)
    echo "--files and --runs must be positive integers" >&2
    exit 2
    ;;
esac

if [[ -n "$jobs" ]]; then
  case "$jobs" in
    *[!0-9]*|0)
      echo "--jobs must be a positive integer" >&2
      exit 2
      ;;
  esac
fi

jobs_label="${jobs:-cli-default}"
add_args=(add --json)
if [[ -n "$jobs" ]]; then
  add_args+=(-j "$jobs")
fi
add_args+=('*.bin')

if [[ ! -x "$crab_bin" ]]; then
  echo "release crab binary not found: $crab_bin" >&2
  echo "run: cargo build -p crab --release" >&2
  exit 1
fi
crab_bin="$(cd -- "$(dirname -- "$crab_bin")" && pwd)/$(basename -- "$crab_bin")"

if [[ ! -x "$generator_bin" ]]; then
  echo "generate_test_file binary not found: $generator_bin" >&2
  echo "run: cargo build -p crab --release" >&2
  exit 1
fi
generator_bin="$(cd -- "$(dirname -- "$generator_bin")" && pwd)/$(basename -- "$generator_bin")"

bench_dir="$(mktemp -d "${TMPDIR:-/tmp}/crab-add-bench.XXXXXX")"
cleanup() {
  if [[ "${KEEP_BENCH_DIR:-0}" == "1" ]]; then
    echo "kept benchmark repo: $bench_dir" >&2
  else
    rm -rf "$bench_dir"
  fi
}
trap cleanup EXIT

echo "mode=$mode files=$files size=$size jobs=$jobs_label runs=$runs" >&2
echo "crab=$crab_bin" >&2
echo "generator=$generator_bin" >&2

for run in $(seq 1 "$runs"); do
  run_dir="$bench_dir/run-$run"
  mkdir "$run_dir"
  cd "$run_dir"
  git init -q
  "$crab_bin" track '*.bin' >/dev/null

  case "$mode" in
    unique)
      for i in $(seq 1 "$files"); do
        "$generator_bin" --size "$size" --seed "$i" "file-$i.bin" >/dev/null
      done
      ;;
    duplicate)
      "$generator_bin" --size "$size" --seed 1 file-1.bin >/dev/null
      if (( files > 1 )); then
        for i in $(seq 2 "$files"); do
          cp file-1.bin "file-$i.bin"
        done
      fi
      ;;
  esac

  /usr/bin/time -p "$crab_bin" "${add_args[@]}" > add.json 2> time.txt

  echo "=== run $run ==="
  cat add.json
  cat time.txt
  du -sh .crab/staging .crab/staging/segments 2>/dev/null || true
done
