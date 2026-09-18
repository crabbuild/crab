#!/usr/bin/env bash
set -euo pipefail

mode="${1:-fast}"
model_dir="$(cd "$(dirname "$0")" && pwd)"
cache_dir="${CRAB_CELL_TLC_CACHE:-${TMPDIR:-/tmp}/crab-cell-tlc}"
toolchain_file="$model_dir/toolchain.env"
source "$toolchain_file"

if ! command -v java >/dev/null 2>&1; then
  echo "error: Java 11 or newer is required for TLC" >&2
  exit 1
fi
if ! command -v curl >/dev/null 2>&1; then
  echo "error: curl is required to obtain the pinned TLC artifact" >&2
  exit 1
fi

mkdir -p "$cache_dir"
jar="$cache_dir/tla2tools-${TLA_VERSION}.jar"
if [ ! -f "$jar" ] || [ "$(shasum -a 256 "$jar" | awk '{print $1}')" != "$TLA_SHA256" ]; then
  temporary="$jar.tmp.$$"
  trap 'rm -f "$temporary"' EXIT
  curl --fail --location --silent --show-error "$TLA_URL" --output "$temporary"
  actual="$(shasum -a 256 "$temporary" | awk '{print $1}')"
  if [ "$actual" != "$TLA_SHA256" ]; then
    echo "error: TLC checksum mismatch (expected $TLA_SHA256, got $actual)" >&2
    exit 1
  fi
  mv "$temporary" "$jar"
  trap - EXIT
fi

run_model() {
  local config="$1"
  local expected="$2"
  local output
  local meta_dir="$cache_dir/meta-negative/${config%.cfg}"
  mkdir -p "$meta_dir"
  output="$(mktemp "${TMPDIR:-/tmp}/crab-cell-tlc.XXXXXX")"
  trap 'rm -f "$output"' RETURN
  set +e
  java -cp "$jar" tlc2.TLC -workers 1 -noGenerateSpecTE \
    -metadir "$meta_dir" -config "$model_dir/$config" \
    "$model_dir/CellCoordination.tla" | tee "$output"
  local status=${PIPESTATUS[0]}
  set -e
  if [ "$status" -eq 0 ]; then
    echo "error: expected TLC violation for $config" >&2
    return 1
  fi
  if ! grep -Fq "Invariant $expected is violated." "$output"; then
    echo "error: $config violated an unexpected invariant (expected $expected)" >&2
    return 1
  fi
  return 0
}

case "$mode" in
  fast)
    java -cp "$jar" tlc2.TLC -workers 1 -depth 6 -nowarning -noGenerateSpecTE \
      -metadir "$cache_dir/meta-fast" \
      -config "$model_dir/CellCoordination.cfg" "$model_dir/CellCoordination.tla"
    java -cp "$jar" tlc2.TLC -workers 1 -depth 6 -nowarning -noGenerateSpecTE \
      -metadir "$cache_dir/meta-liveness-fast" \
      -config "$model_dir/CellCoordinationLiveness.cfg" "$model_dir/CellCoordination.tla"
    ;;
  broad)
    java -cp "$jar" tlc2.TLC -workers 1 -depth 10 -nowarning -noGenerateSpecTE \
      -metadir "$cache_dir/meta-broad" \
      -config "$model_dir/CellCoordination.cfg" "$model_dir/CellCoordination.tla"
    java -cp "$jar" tlc2.TLC -workers 1 -depth 10 -nowarning -noGenerateSpecTE \
      -metadir "$cache_dir/meta-liveness-broad" \
      -config "$model_dir/CellCoordinationLiveness.cfg" "$model_dir/CellCoordination.tla"
    ;;
  negative)
    run_model CellCoordinationBrokenAck.cfg AckHasDurability
    run_model CellCoordinationBrokenOwner.cfg NoDualServing
    run_model CellCoordinationBrokenWinner.cfg OwnerIsLive
    run_model CellCoordinationBrokenRelease.cfg RetainedHasOwner
    ;;
  *)
    echo "usage: $0 {fast|broad|negative}" >&2
    exit 2
    ;;
esac
