#!/bin/sh
set -eu
run_name=$1
shift
export DOCKER_HOST=unix:///Volumes/Workspace/crabbuild-target/vfs-02d0-vm/default/docker.sock
exec docker run --rm --name "crab-vfs-$run_name" --privileged --device /dev/fuse \
 -v "$PWD:/src:ro" \
 -v /Volumes/Workspace/crabbuild-target/crab-ios-02d01345-vfs/linux:/root/Workspace/crabbuild-target/vfs-audit \
 -v crab-02d01345-vfs-unit-fixtures:/root/Workspace/crabbuild-target/vfs-audit/run \
 -v /Volumes/Workspace/crabbuild-target/crab-ios-02d01345-vfs/linux/mapped-before-source/mount_runtime.rs:/src/crates/crab-vfs/src/mount_runtime.rs:ro \
 -v /Volumes/Workspace/crabbuild-target/crab-ios-02d01345-vfs/linux/mapped-before-source/refresh.rs:/src/crates/crab-vfs/src/refresh.rs:ro \
 -w /src \
 -e CARGO_HOME=/root/Workspace/crabbuild-target/vfs-audit/cargo \
 -e CARGO_TARGET_DIR=/root/Workspace/crabbuild-target/vfs-audit/target \
 -e TMPDIR=/root/Workspace/crabbuild-target/vfs-audit/run \
 -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=cc \
 -e CARGO_PROFILE_DEV_DEBUG=0 -e CARGO_PROFILE_TEST_DEBUG=0 \
 -e CARGO_PROFILE_DEV_INCREMENTAL=false -e CARGO_PROFILE_TEST_INCREMENTAL=false \
 -e COMPILER_PATH=/root/Workspace/crabbuild-target/vfs-audit/linker \
 -e CARGO_BUILD_JOBS=4 -e GIT_CEILING_DIRECTORIES=/src \
 crab-vfs-audit:1.97-windows sh -c 'env -u CARGO_INCREMENTAL cargo --config profile.test.package.crab-vfs.incremental=true "$@"' sh "$@"
