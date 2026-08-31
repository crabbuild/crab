#!/usr/bin/env bash
# bump-version.sh — Bump all shipped Crab product versions.
#
# Usage:
#   ./scripts/release/bump-version.sh patch   # 0.1.0 → 0.1.1
#   ./scripts/release/bump-version.sh minor   # 0.1.0 → 0.2.0
#   ./scripts/release/bump-version.sh major   # 0.1.0 → 1.0.0
#   ./scripts/release/bump-version.sh set 2.3.4  # explicit version
#
# After bumping, the next `make install` picks up the new version
# automatically via CARGO_PKG_VERSION → CRAB_BUILD_VERSION.

set -euo pipefail

CRAB_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
WORKSPACE_DIR="$(cd "$CRAB_DIR/.." && pwd)"
CARGO_LOCK="$WORKSPACE_DIR/Cargo.lock"
PRODUCT_MANIFESTS=(
    "$CRAB_DIR/Cargo.toml"
    "$WORKSPACE_DIR/crates/crab-auth-server/Cargo.toml"
    "$WORKSPACE_DIR/crates/crab-cache-server/Cargo.toml"
)

if [[ ! -f "$CARGO_LOCK" ]]; then
    echo "error: workspace lockfile is missing" >&2
    exit 1
fi

# Extract current version from the [package] section.
CURRENT=$(grep -m1 '^version' "${PRODUCT_MANIFESTS[0]}" | sed 's/.*"\(.*\)"/\1/')

if [[ -z "$CURRENT" ]]; then
    echo "error: could not parse current Crab version" >&2
    exit 1
fi
for manifest in "${PRODUCT_MANIFESTS[@]}"; do
    if [[ ! -f "$manifest" ]]; then
        echo "error: product manifest is missing: $manifest" >&2
        exit 1
    fi
    manifest_version=$(grep -m1 '^version' "$manifest" | sed 's/.*"\(.*\)"/\1/')
    if [[ "$manifest_version" != "$CURRENT" ]]; then
        echo "error: $manifest has version $manifest_version, expected $CURRENT" >&2
        exit 1
    fi
done

IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"

case "${1:-}" in
    patch)
        PATCH=$((PATCH + 1))
        ;;
    minor)
        MINOR=$((MINOR + 1))
        PATCH=0
        ;;
    major)
        MAJOR=$((MAJOR + 1))
        MINOR=0
        PATCH=0
        ;;
    set)
        if [[ -z "${2:-}" ]]; then
            echo "usage: $0 set <version>" >&2
            exit 1
        fi
        if [[ ! "$2" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
            echo "error: version must be stable SemVer such as 1.2.3" >&2
            exit 1
        fi
        IFS='.' read -r MAJOR MINOR PATCH <<< "$2"
        ;;
    *)
        echo "usage: $0 {patch|minor|major|set <version>}" >&2
        echo "  current version: $CURRENT" >&2
        exit 1
        ;;
esac

NEW_VERSION="${MAJOR}.${MINOR}.${PATCH}"

# Replace product versions together and let Cargo update the workspace lockfile.
backup_dir="$(mktemp -d)"
for i in "${!PRODUCT_MANIFESTS[@]}"; do
    cp "${PRODUCT_MANIFESTS[$i]}" "$backup_dir/manifest-$i"
done
cp "$CARGO_LOCK" "$backup_dir/Cargo.lock"
restore_on_error() {
    status=$?
    trap - EXIT
    if [[ "$status" -ne 0 ]]; then
        for i in "${!PRODUCT_MANIFESTS[@]}"; do
            cp "$backup_dir/manifest-$i" "${PRODUCT_MANIFESTS[$i]}"
        done
        cp "$backup_dir/Cargo.lock" "$CARGO_LOCK"
    fi
    rm -r "$backup_dir"
    exit "$status"
}
trap restore_on_error EXIT

for manifest in "${PRODUCT_MANIFESTS[@]}"; do
    if [[ "$(uname -s)" == "Darwin" ]]; then
        sed -i '' "s/^version = \"$CURRENT\"/version = \"$NEW_VERSION\"/" "$manifest"
    else
        sed -i "s/^version = \"$CURRENT\"/version = \"$NEW_VERSION\"/" "$manifest"
    fi
done
(cd "$WORKSPACE_DIR" && cargo metadata --format-version 1 >/dev/null)
trap - EXIT
rm -r "$backup_dir"

echo "$CURRENT → $NEW_VERSION"
