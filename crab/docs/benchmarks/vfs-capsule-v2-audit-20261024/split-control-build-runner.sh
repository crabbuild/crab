#!/bin/sh
set -eu
name=$1
shift
export DOCKER_HOST=unix:///Volumes/Workspace/crabbuild-target/vfs-02d0-vm/default/docker.sock
exec docker run --rm --name "crab-vfs-$name" --privileged --device /dev/fuse --network crab-vfs-catalog-rustfs \
  -v "$PWD:/src:ro" -w /src \
  -v "$HOME/Workspace/crabbuild-target/crab-ios-02d01345-vfs/linux:/root/Workspace/crabbuild-target/vfs-audit" \
  -v crab-02d01345-vfs-unit-fixtures:/root/Workspace/crabbuild-target/vfs-audit/run \
  -e CARGO_HOME=/root/Workspace/crabbuild-target/vfs-audit/cargo \
  -e CARGO_TARGET_DIR=/root/Workspace/crabbuild-target/vfs-audit/target \
  -e TMPDIR=/root/Workspace/crabbuild-target/vfs-audit/run \
  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=cc \
  -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 -e CARGO_INCREMENTAL=0 -e CARGO_BUILD_JOBS=4 \
  crab-vfs-audit:1.97-windows "$@"
