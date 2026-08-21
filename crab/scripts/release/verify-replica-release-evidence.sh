#!/usr/bin/env bash
# Verify the retained live evidence bundle required for enterprise releases.

set -euo pipefail
export LC_ALL=C
export LANG=C

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
WORKSPACE_DIR="$(cd "$CRAB_DIR/.." && pwd)"
CARGO="${CARGO:-cargo}"

evidence_dir="${1:-${REPLICA_RELEASE_EVIDENCE_DIR:-}}"
output="${2:-${REPLICA_RELEASE_EVIDENCE_OUTPUT:-replica-release-evidence-verify.json}}"
expected_run_id="${3:-${REPLICA_RELEASE_EVIDENCE_EXPECTED_RUN_ID:-}}"

if [[ -z "$evidence_dir" ]]; then
    echo "error: evidence directory is required" >&2
    echo "usage: $0 <replica-live-evidence-dir> [output-json] [expected-run-id]" >&2
    exit 2
fi

if [[ -z "$expected_run_id" ]]; then
    echo "error: expected run-attempt ID is required for release evidence verification" >&2
    echo "usage: $0 <replica-live-evidence-dir> [output-json] replica-live-<github-run-id>-<attempt>" >&2
    exit 2
fi

if [[ ! "$expected_run_id" =~ ^replica-live-[0-9]+-[0-9]+$ ]]; then
    echo "error: expected run-attempt ID must match replica-live-<github-run-id>-<attempt>" >&2
    exit 2
fi

if [[ ! -d "$evidence_dir" ]]; then
    echo "error: $evidence_dir is not an evidence directory" >&2
    exit 2
fi

cd "$WORKSPACE_DIR"
args=(
    replica evidence verify "$evidence_dir"
    --profile enterprise \
    --require-redacted \
    --expected-run-id "$expected_run_id" \
    --json
)
"$CARGO" run --manifest-path crab/Cargo.toml --locked --bin crab -- "${args[@]}" | tee "$output"
