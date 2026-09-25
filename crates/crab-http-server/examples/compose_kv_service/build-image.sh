#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../../../../" && pwd)"
: "${CARGO_TARGET_DIR:?set a per-checkout CARGO_TARGET_DIR below \$HOME/Workspace/crabbuild-target}"
case "$CARGO_TARGET_DIR" in
  "$HOME"/Workspace/crabbuild-target/*) ;;
  *) echo "CARGO_TARGET_DIR must be on the mounted workspace volume." >&2; exit 2 ;;
esac
test -d "$HOME/Workspace/crabbuild-target" &&
  test -w "$HOME/Workspace/crabbuild-target" || {
    echo "Workspace target volume is unavailable." >&2
    exit 2
  }
mkdir -p "$CARGO_TARGET_DIR"

architecture="$(docker info --format '{{.Architecture}}')"
case "$architecture" in
  aarch64|arm64) target=aarch64-unknown-linux-gnu ;;
  x86_64|amd64) target=x86_64-unknown-linux-gnu ;;
  *) echo "Unsupported Docker architecture: $architecture" >&2; exit 2 ;;
esac

if [ "$(uname -s)" = Darwin ]; then
  linker="${target%-unknown-linux-gnu}-linux-gnu-gcc"
  command -v "$linker" >/dev/null || {
    echo "Missing Linux cross linker: $linker" >&2
    exit 2
  }
  case "$target" in
    aarch64-unknown-linux-gnu) export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$linker" ;;
    x86_64-unknown-linux-gnu) export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="$linker" ;;
  esac
fi

cd "$repo_root"
CRAB_CELL_SCALE_SOURCE_REVISION="$(git rev-parse HEAD)" \
  cargo build -p crab-http-server --example compose_kv_service --target "$target" --locked
context="$CARGO_TARGET_DIR/cell-scale-image-$target"
mkdir -p "$context"
cp "$CARGO_TARGET_DIR/$target/debug/examples/compose_kv_service" "$context/compose_kv_service"
docker build --file "$script_dir/Dockerfile" --tag "${CRAB_CELL_SCALE_IMAGE:-crab-cell-scale:local}" "$context"
