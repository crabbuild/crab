#!/usr/bin/env bash
# Run the ignored active-active cross-region smoke for selected coordinators.

set -euo pipefail
export LC_ALL=C
export LANG=C

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CRAB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CARGO="${CARGO:-cargo}"

enabled() {
    local name="$1"
    local value="${!name:-}"
    local normalized
    normalized="$(printf '%s' "$value" | tr '[:upper:]' '[:lower:]')"
    case "$normalized" in
        1|true|yes|on) return 0 ;;
        *) return 1 ;;
    esac
}

upper_provider() {
    case "$1" in
        dynamodb) echo DYNAMODB ;;
        spanner) echo SPANNER ;;
        cosmosdb) echo COSMOSDB ;;
        *) echo "error: unsupported coordinator provider '$1'" >&2; exit 2 ;;
    esac
}

require_value() {
    local name="$1"
    local value="${!name:-}"
    if [[ -z "$value" ]]; then
        echo "error: $name is required for active-active cross-region smoke" >&2
        exit 2
    fi
    printf '%s' "$value"
}

value_or() {
    local preferred="$1"
    local fallback="$2"
    local preferred_value="${!preferred:-}"
    if [[ -n "$preferred_value" ]]; then
        printf '%s' "$preferred_value"
        return
    fi
    require_value "$fallback"
}

value_or_default() {
    local preferred="$1"
    local fallback="$2"
    local default="$3"
    local preferred_value="${!preferred:-}"
    local fallback_value="${!fallback:-}"
    if [[ -n "$preferred_value" ]]; then
        printf '%s' "$preferred_value"
    elif [[ -n "$fallback_value" ]]; then
        printf '%s' "$fallback_value"
    else
        printf '%s' "$default"
    fi
}

value_or_one_of() {
    local label="$1"
    shift
    local name=""
    for name in "$@"; do
        local value="${!name:-}"
        if [[ -n "$value" ]]; then
            printf '%s' "$value"
            return
        fi
    done

    echo "error: one of $* is required for $label" >&2
    exit 2
}

run_smoke() {
    "$CARGO" test \
        --manifest-path "$CRAB_DIR/Cargo.toml" \
        --test replica_live_cross_region \
        -- --ignored live_active_active_cross_region_push_fetch_hydrate_smoke
}

run_load() {
    "$CARGO" test \
        --manifest-path "$CRAB_DIR/Cargo.toml" \
        --test replica_live_cross_region \
        -- --ignored live_active_active_production_load_evidence
}

run_load_if_enabled() {
    if enabled CRAB_REPLICA_LIVE_PRODUCTION_LOAD; then
        echo "running active-active production load evidence"
        run_load
    fi
}

selected=()
for provider in dynamodb spanner cosmosdb; do
    upper="$(upper_provider "$provider")"
    if enabled "CRAB_REPLICA_LIVE_${upper}"; then
        selected+=("$provider")
    fi
done

if [[ ${#selected[@]} -eq 0 ]]; then
    run_smoke
    run_load_if_enabled
    exit 0
fi

multi_provider=false
if [[ ${#selected[@]} -gt 1 ]]; then
    multi_provider=true
fi

for provider in "${selected[@]}"; do
    upper="$(upper_provider "$provider")"
    prefix="CRAB_REPLICA_LIVE_${upper}"
    smoke_prefix="${prefix}_SMOKE"

    if [[ "$multi_provider" == true ]]; then
        writer_a_url="$(require_value "${smoke_prefix}_WRITER_A_URL")"
        writer_a_region="$(require_value "${smoke_prefix}_WRITER_A_REGION")"
        writer_b_url="$(require_value "${smoke_prefix}_WRITER_B_URL")"
        writer_b_region="$(require_value "${smoke_prefix}_WRITER_B_REGION")"
        coordinator_url="$(require_value "${smoke_prefix}_COORDINATOR_URL")"
    else
        writer_a_url="$(value_or "${smoke_prefix}_WRITER_A_URL" CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL)"
        writer_a_region="$(value_or "${smoke_prefix}_WRITER_A_REGION" CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION)"
        writer_b_url="$(value_or "${smoke_prefix}_WRITER_B_URL" CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL)"
        writer_b_region="$(value_or "${smoke_prefix}_WRITER_B_REGION" CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION)"
        coordinator_url="$(value_or "${smoke_prefix}_COORDINATOR_URL" CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL)"
    fi

    CRAB_REPLICA_LIVE_SMOKE_WRITER_A_NAME="$(
        value_or_default "${smoke_prefix}_WRITER_A_NAME" CRAB_REPLICA_LIVE_SMOKE_WRITER_A_NAME "${provider}-writer-a"
    )"
    export CRAB_REPLICA_LIVE_SMOKE_WRITER_A_NAME
    export CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL="$writer_a_url"
    export CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION="$writer_a_region"

    CRAB_REPLICA_LIVE_SMOKE_WRITER_B_NAME="$(
        value_or_default "${smoke_prefix}_WRITER_B_NAME" CRAB_REPLICA_LIVE_SMOKE_WRITER_B_NAME "${provider}-writer-b"
    )"
    export CRAB_REPLICA_LIVE_SMOKE_WRITER_B_NAME
    export CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL="$writer_b_url"
    export CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION="$writer_b_region"

    export CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL="$coordinator_url"
    if [[ "$multi_provider" == true ]]; then
        CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION="$(
            value_or "${smoke_prefix}_COORDINATOR_REGION" "${prefix}_REGION"
        )"
    else
        CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION="$(
            value_or_one_of "active-active cross-region smoke" \
                "${smoke_prefix}_COORDINATOR_REGION" \
                "${prefix}_REGION" \
                CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION
        )"
    fi
    export CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION
    if [[ "$multi_provider" == true ]]; then
        CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION="$(
            value_or "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" "${prefix}_FAILOVER_REGION"
        )"
    else
        CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION="$(
            value_or_one_of "active-active cross-region smoke" \
                "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" \
                "${prefix}_FAILOVER_REGION" \
                CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION
        )"
    fi
    export CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION
    export CRAB_REPLICA_LIVE_SMOKE_TIMEOUT_SECS="${CRAB_REPLICA_LIVE_SMOKE_TIMEOUT_SECS:-900}"

    echo "running active-active cross-region smoke for ${provider}"
    run_smoke
    run_load_if_enabled
done
