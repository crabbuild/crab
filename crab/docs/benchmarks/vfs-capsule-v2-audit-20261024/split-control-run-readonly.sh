#!/bin/sh
set -eu
backend=$1
export DOCKER_HOST=unix:///Volumes/Workspace/crabbuild-target/vfs-02d0-vm/default/docker.sock
exec docker run --rm --name "crab-vfs-split-control-provider-$backend" \
 --network crab-vfs-catalog-rustfs --privileged --device /dev/fuse \
 -v /Volumes/Workspace/crabbuild-target/crab-ios-02d01345-vfs/linux/split-control-provider:/e2e \
 -v crab-02d01345-vfs-unit-fixtures:/root/Workspace/crabbuild-target/vfs-audit/run \
 -e MAPPED_HELPER_CONTROLS="${MAPPED_HELPER_CONTROLS:-0}" \
 -e TMPDIR=/root/Workspace/crabbuild-target/vfs-audit/run \
 -e AWS_ACCESS_KEY_ID=crab -e AWS_SECRET_ACCESS_KEY=crab \
 -e AWS_DEFAULT_REGION=us-east-1 -e AWS_REGION=us-east-1 \
 -e AWS_ENDPOINT_URL=http://fence-rustfs:9000 \
 -e AWS_ENDPOINT_URL_S3=http://fence-rustfs:9000 \
 -e AWS_ALLOW_HTTP=true -e AWS_EC2_METADATA_DISABLED=true \
 -e AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false -e VIRTUAL_HOSTED_STYLE_REQUEST=false \
 crab-vfs-audit:1.97-windows python3 /e2e/readonly.py "$backend"
