#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 9 ]; then
  echo "usage: $0 TAG VERSION SOURCE_SHA IMAGE_REPOSITORY IMAGE_DIGEST CHART_REPOSITORY CHART_DIGEST CHART_PACKAGE_DIGEST OUTPUT" >&2
  exit 2
fi

tag="$1"
version="$2"
source_sha="$3"
image_repository="$4"
image_digest="$5"
chart_repository="$6"
chart_digest="$7"
chart_package_digest="$8"
output="$9"

if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  [ "$tag" != "crab-http-server-v${version}" ]; then
  echo "release tag and version must identify the same stable SemVer release" >&2
  exit 1
fi
if [[ ! "$source_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "source commit must be a lowercase 40-character Git commit" >&2
  exit 1
fi
if [[ ! "$image_repository" =~ ^ghcr\.io/[a-z0-9._-]+/crab-http-server$ ]]; then
  echo "image repository must be an untagged GHCR crab-http-server repository" >&2
  exit 1
fi
if [[ ! "$chart_repository" =~ ^oci://ghcr\.io/[a-z0-9._-]+/charts/crab-http-server$ ]]; then
  echo "chart repository must be the unversioned GHCR Crab chart repository" >&2
  exit 1
fi
for digest in "$image_digest" "$chart_digest" "$chart_package_digest"; do
  if [[ ! "$digest" =~ ^sha256:[0-9a-f]{64}$ ]]; then
    echo "release artifact digests must be lowercase SHA-256 values" >&2
    exit 1
  fi
done

mkdir -p "$(dirname -- "$output")"
temporary="$(mktemp "${output}.tmp.XXXXXX")"
trap 'rm -f "$temporary"' EXIT
jq --null-input \
  --arg tag "$tag" \
  --arg version "$version" \
  --arg source_commit "$source_sha" \
  --arg image_repository "$image_repository" \
  --arg image_digest "$image_digest" \
  --arg image_reference "${image_repository}@${image_digest}" \
  --arg chart_repository "$chart_repository" \
  --arg chart_digest "$chart_digest" \
  --arg chart_reference "${chart_repository}@${chart_digest}" \
  --arg chart_package "crab-http-server-${version}.tgz" \
  --arg chart_package_digest "$chart_package_digest" \
  '{
    schema: 1,
    tag: $tag,
    version: $version,
    source_commit: $source_commit,
    image: {
      repository: $image_repository,
      digest: $image_digest,
      reference: $image_reference,
      platforms: ["linux/amd64", "linux/arm64"]
    },
    chart: {
      repository: $chart_repository,
      digest: $chart_digest,
      reference: $chart_reference,
      package: $chart_package,
      package_digest: $chart_package_digest
    }
  }' > "$temporary"
mv "$temporary" "$output"
trap - EXIT
