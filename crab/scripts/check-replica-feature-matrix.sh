#!/usr/bin/env bash
# check-replica-feature-matrix.sh - Compile the enterprise replica feature slices.
#
# These checks are intentionally no-default builds. Enterprise packages may ship
# one cloud backend at a time, so each coordinator and storage control-plane
# feature must compile without depending on the default all-cloud feature set.

set -euo pipefail
export LC_ALL=C
export LANG=C

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CARGO="${CARGO:-cargo}"

REQUESTED_SLICES=()
INCLUDE_ALL_CLOUD=false
LIST_ONLY=false
LOCKED=false

GREEN='\033[32m'
YELLOW='\033[33m'
RED='\033[31m'
RESET='\033[0m'

info() { printf "${GREEN}==>${RESET} %s\n" "$1"; }
warn() { printf "${YELLOW}warning:${RESET} %s\n" "$1" >&2; }
die() { printf "${RED}error:${RESET} %s\n" "$1" >&2; exit 1; }

usage() {
    cat <<USAGE
Usage: $0 [options]

Options:
  --slice NAME          Run one named slice. May be repeated.
  --include-all-cloud   Also check the combined all-cloud feature set.
  --locked              Pass --locked to cargo.
  --list                Print available slices.
  -h, --help            Show this help.

Slices:
  replica-evidence-no-default
  coordinator-dynamodb
  coordinator-spanner
  coordinator-cosmosdb
  replication-s3-control-plane
  replication-gcs-control-plane
  replication-azure-control-plane
  all-cloud
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --slice)
            [[ $# -ge 2 ]] || die "--slice requires a slice name"
            REQUESTED_SLICES+=("$2")
            shift 2
            ;;
        --include-all-cloud)
            INCLUDE_ALL_CLOUD=true
            shift
            ;;
        --locked)
            LOCKED=true
            shift
            ;;
        --list)
            LIST_ONLY=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1"
            ;;
    esac
done

SLICE_NAMES=(
    replica-evidence-no-default
    coordinator-dynamodb
    coordinator-spanner
    coordinator-cosmosdb
    replication-s3-control-plane
    replication-gcs-control-plane
    replication-azure-control-plane
    all-cloud
)

SLICE_KINDS=(
    test
    check
    check
    check
    check
    check
    check
    check
)

SLICE_FEATURES=(
    ""
    "coordinator-dynamodb"
    "coordinator-spanner"
    "coordinator-cosmosdb"
    "replication-s3-control-plane"
    "replication-gcs-control-plane"
    "replication-azure-control-plane"
    "coordinator-dynamodb,coordinator-spanner,coordinator-cosmosdb,replication-s3-control-plane,replication-gcs-control-plane,replication-azure-control-plane"
)

slice_index() {
    local name="$1"
    local i
    for i in "${!SLICE_NAMES[@]}"; do
        if [[ "${SLICE_NAMES[$i]}" == "$name" ]]; then
            echo "$i"
            return 0
        fi
    done
    return 1
}

print_slices() {
    local i
    for i in "${!SLICE_NAMES[@]}"; do
        printf "%-34s %s\n" "${SLICE_NAMES[$i]}" "${SLICE_FEATURES[$i]:-(no features)}"
    done
}

run_slice() {
    local name="$1"
    local index
    index="$(slice_index "$name")" || die "unknown slice: $name"

    local kind="${SLICE_KINDS[$index]}"
    local features="${SLICE_FEATURES[$index]}"
    local cargo_args=()

    if [[ "$LOCKED" == true ]]; then
        cargo_args+=(--locked)
    fi

    info "replica feature slice: $name"
    if [[ "$kind" == "test" ]]; then
        "$CARGO" test \
            ${cargo_args[@]+"${cargo_args[@]}"} \
            --manifest-path "$CRAB_DIR/Cargo.toml" \
            --no-default-features \
            --lib evidence_verify
        return
    fi

    "$CARGO" check \
        ${cargo_args[@]+"${cargo_args[@]}"} \
        --manifest-path "$CRAB_DIR/Cargo.toml" \
        --no-default-features \
        --features "$features" \
        --lib
}

if [[ "$LIST_ONLY" == true ]]; then
    print_slices
    exit 0
fi

if [[ ${#REQUESTED_SLICES[@]} -eq 0 ]]; then
    REQUESTED_SLICES=(
        replica-evidence-no-default
        coordinator-dynamodb
        coordinator-spanner
        coordinator-cosmosdb
        replication-s3-control-plane
        replication-gcs-control-plane
        replication-azure-control-plane
    )
    if [[ "$INCLUDE_ALL_CLOUD" == true ]]; then
        REQUESTED_SLICES+=(all-cloud)
    fi
fi

for slice in "${REQUESTED_SLICES[@]}"; do
    if [[ "$slice" == "all-cloud" && "$INCLUDE_ALL_CLOUD" != true ]]; then
        warn "running all-cloud because it was explicitly requested"
    fi
    run_slice "$slice"
done

info "replica feature matrix passed"
