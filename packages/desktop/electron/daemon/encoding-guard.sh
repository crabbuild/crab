#!/usr/bin/env bash
# encoding-guard.sh — Lint guard ensuring no base64 or toString('utf8')
# calls exist in the PTY daemon data path. The daemon uses binary payload
# tails in frames; any base64 encoding on the data path is a performance
# regression (~33% wire overhead + encode/decode passes).
#
# Exit 0 = clean, Exit 1 = violations found.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Data-path files that MUST NOT use base64 encoding
DATA_PATH_FILES=(
  "$SCRIPT_DIR/protocol.ts"
  "$SCRIPT_DIR/daemon-server.ts"
  "$SCRIPT_DIR/stream-data-batcher.ts"
  "$SCRIPT_DIR/daemon-client.ts"
)

# Files where Buffer.from(*, 'base64') is legitimate (snapshot serialization)
ALLOWED_BASE64_FROM=(
  "$SCRIPT_DIR/fd-handoff.ts"
)

violations=0

# Check for .toString('base64') or .toString("base64") in data-path files
for file in "${DATA_PATH_FILES[@]}"; do
  if [ -f "$file" ]; then
    if grep -nE '\.toString\(('"'"'|")base64('"'"'|")\)' "$file"; then
      echo "ERROR: toString('base64') found in $(basename "$file")"
      violations=$((violations + 1))
    fi
  fi
done

# Check for Buffer.from(*, 'base64') in data-path files (fd-handoff.ts excluded)
for file in "${DATA_PATH_FILES[@]}"; do
  if [ -f "$file" ]; then
    if grep -nE 'Buffer\.from\([^)]*,\s*('"'"'|")base64('"'"'|")\)' "$file"; then
      echo "ERROR: Buffer.from(*, 'base64') found in $(basename "$file")"
      violations=$((violations + 1))
    fi
  fi
done

# Check for .toString('utf8') or .toString("utf8") in frame encoding/decoding path
# These should use Buffer directly without string conversion
for file in "${DATA_PATH_FILES[@]}"; do
  if [ -f "$file" ]; then
    if grep -nE '\.toString\(('"'"'|")utf8('"'"'|")\)' "$file"; then
      echo "ERROR: toString('utf8') found in $(basename "$file")"
      violations=$((violations + 1))
    fi
  fi
done

if [ "$violations" -gt 0 ]; then
  echo ""
  echo "FAIL: $violations encoding violation(s) found in daemon data path."
  echo "The daemon uses binary payload tails in frames — base64 encoding"
  echo "or toString('utf8') on the data path is a performance regression."
  exit 1
fi

echo "OK: No encoding violations in daemon data path."
exit 0
