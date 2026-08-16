#!/usr/bin/env bash
# seed-homebrew-tap.sh — Initialize the crabbuild/homebrew-tap repo with
# a README, then publish the current release formula. Run once after creating
# the repo and publishing the matching crabbuild/crab-release tag.
#
# Usage:
#   ./scripts/seed-homebrew-tap.sh

set -euo pipefail
export GIT_TERMINAL_PROMPT=0

TAP_REPO="crabbuild/homebrew-tap"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
VERSION="$(grep -m1 '^version' "$CRAB_DIR/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')"
TAG="v${VERSION}"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    gh auth setup-git >/dev/null 2>&1 || true
fi

echo "==> Cloning ${TAP_REPO}..."
gh repo clone "$TAP_REPO" "$TMPDIR/tap" -- 2>/dev/null || \
    git clone "https://github.com/${TAP_REPO}.git" "$TMPDIR/tap"

cd "$TMPDIR/tap"

git config user.email "release@crabbuild.com"
git config user.name "Crab Release Bot"

# Create README
cat > README.md << 'EOF'
# Homebrew Tap for Crab

Serverless git remote helper — repositories live entirely in cloud object storage.

## Install

```bash
brew install crabbuild/tap/crab
```

## What you get

- `crab` — the CLI
- `git-remote-crab` — symlink so `git clone crab://bucket/repo` works with unmodified git

## Upgrade

```bash
brew upgrade crab
```

## More info

- [Documentation](https://crab.build/docs)
- [Source](https://github.com/crabbuild/crab)
EOF

# Commit and push
git add -A
git commit -m "Initial tap README"
git branch -M main
git push -u origin main

echo ""
echo "==> README seeded. Publishing formula for ${TAG}..."
"$SCRIPT_DIR/update-homebrew.sh" "$TAG"

echo ""
echo "==> Done! homebrew-tap seeded."
echo "    Users can now run: brew install crabbuild/tap/crab"
