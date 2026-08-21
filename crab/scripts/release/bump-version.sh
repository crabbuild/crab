#!/usr/bin/env bash
# bump-version.sh — Bump the crab version in Cargo.toml
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

CARGO_TOML="$(dirname "$0")/../../Cargo.toml"

if [[ ! -f "$CARGO_TOML" ]]; then
    echo "error: Cargo.toml not found at $CARGO_TOML" >&2
    exit 1
fi

# Extract current version from the [package] section.
CURRENT=$(grep -m1 '^version' "$CARGO_TOML" | sed 's/.*"\(.*\)"/\1/')

if [[ -z "$CURRENT" ]]; then
    echo "error: could not parse version from $CARGO_TOML" >&2
    exit 1
fi

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
        IFS='.' read -r MAJOR MINOR PATCH <<< "$2"
        ;;
    *)
        echo "usage: $0 {patch|minor|major|set <version>}" >&2
        echo "  current version: $CURRENT" >&2
        exit 1
        ;;
esac

NEW_VERSION="${MAJOR}.${MINOR}.${PATCH}"

# Replace the version line in Cargo.toml (first occurrence only).
sed -i '' "s/^version = \"$CURRENT\"/version = \"$NEW_VERSION\"/" "$CARGO_TOML"

echo "$CURRENT → $NEW_VERSION"
