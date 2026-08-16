#!/usr/bin/env bash
# Generate Crab release notes from the source repo history.

set -euo pipefail
export LC_ALL=C
export LANG=C

tag="${1:-}"
previous_tag="${2:-}"
output="${3:-}"

if [[ -z "$tag" || -z "$output" ]]; then
    echo "usage: $0 <tag> <previous-tag-or-empty> <output-file>" >&2
    exit 2
fi

source_repo="${SOURCE_REPO:-crabbuild/crab}"
release_repo="${RELEASE_REPO:-crabbuild/crab-release}"
source_sha="$(git rev-parse HEAD)"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

write_artifact_section() {
    printf "\n## Artifacts\n\n"
    local artifacts=(
        crab-darwin-aarch64.tar.gz
        crab-darwin-x86_64.tar.gz
        crab-linux-aarch64.tar.gz
        crab-linux-x86_64.tar.gz
        crab-windows-aarch64.zip
        crab-windows-x86_64.zip
        SHA256SUMS.txt
    )
    local artifact
    for artifact in "${artifacts[@]}"; do
        printf -- "- %s\n" "$artifact"
    done
}

if command -v gh >/dev/null 2>&1 && [[ -n "$previous_tag" ]]; then
    if gh api "repos/${source_repo}/releases/generate-notes" \
        -f tag_name="$tag" \
        -f previous_tag_name="$previous_tag" \
        -f target_commitish="$source_sha" \
        --jq '.body' > "$tmp" 2>/dev/null && [[ -s "$tmp" ]]; then
        {
            printf "# Crab CLI %s\n\n" "$tag"
            cat "$tmp"
            write_artifact_section
            printf "\n\n## Release Metadata\n\n"
            printf -- "- Source: https://github.com/%s/tree/%s\n" "$source_repo" "$tag"
            printf -- "- Commit: %s\n" "$source_sha"
            printf -- "- Release artifacts: https://github.com/%s/releases/tag/%s\n" "$release_repo" "$tag"
            printf -- "- Checksums: attached as SHA256SUMS.txt\n"
        } > "$output"
        exit 0
    fi
fi

{
    printf "# Crab CLI %s\n\n" "$tag"
    if [[ -n "$previous_tag" ]]; then
        printf "Changes since %s.\n\n" "$previous_tag"
        printf "## Changes\n\n"
        if git rev-parse --verify "$previous_tag" >/dev/null 2>&1; then
            git log --no-merges --format='- %s (%h)' "${previous_tag}..HEAD"
        else
            printf -- "- Previous source tag %s is not available locally; review GitHub compare before publishing.\n" "$previous_tag"
        fi
    else
        printf "## Changes\n\n"
        printf -- "- No previous release tag was detected; review recent commits before publishing.\n"
    fi
    write_artifact_section
    printf "\n## Release Metadata\n\n"
    printf -- "- Source: https://github.com/%s/tree/%s\n" "$source_repo" "$tag"
    printf -- "- Commit: %s\n" "$source_sha"
    printf -- "- Release artifacts: https://github.com/%s/releases/tag/%s\n" "$release_repo" "$tag"
    printf -- "- Checksums: attached as SHA256SUMS.txt\n"
} > "$output"
