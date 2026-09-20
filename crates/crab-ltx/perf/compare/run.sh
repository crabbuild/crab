#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
target_dir=${CARGO_TARGET_DIR:-${HOME}/Workspace/crabbuild-target/crab-ltx-perf}
transactions=${LTX_TRANSACTIONS:-128}
payload_bytes=${LTX_PAYLOAD_BYTES:-4096}
rounds=${LTX_ROUNDS:-5}
warmup=${LTX_WARMUP:-1}

common_args=(
  --transactions "$transactions"
  --payload-bytes "$payload_bytes"
  --rounds "$rounds"
  --warmup "$warmup"
)

crab_report=$(mktemp)
celld_report=$(mktemp)
trap 'rm -f "$crab_report" "$celld_report"' EXIT

CARGO_TARGET_DIR="$target_dir" cargo run --quiet --release \
  --manifest-path "$script_dir/crab/Cargo.toml" -- "${common_args[@]}" >"$crab_report"
CARGO_TARGET_DIR="$target_dir" cargo run --quiet --release \
  --manifest-path "$script_dir/celld/Cargo.toml" -- "${common_args[@]}" >"$celld_report"

printf '%s\n' "=== crab-ltx ==="
cat "$crab_report"
printf '%s\n' "=== celld-ltx ==="
cat "$celld_report"
