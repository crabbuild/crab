#!/usr/bin/env bash
# check-replica-live-env.sh - Fail fast when selected live replica suites would skip.
#
# The live replica harnesses intentionally skip when env is missing. That is
# friendly for local development, but production evidence runs must fail before
# tests if a selected cloud topology is incomplete.

set -euo pipefail
export LC_ALL=C
export LANG=C

SUITES=()
STORAGE_PROVIDERS=()
COORDINATORS=()
HYDRATE_PROVIDERS=()
REQUIRE_MUTATE=false
REQUIRE_EVIDENCE=false
REQUIRE_REDACTED=false
REQUIRE_CLOUD_CREDENTIALS=false
REQUIRE_REPAIR_WORKER_DEPLOYMENT=false
EVIDENCE_PROFILE=""
ENTERPRISE_SUITE=false
CLOUDS=()
AZURE_MANAGEMENT_REQUIRED=false

RED='\033[31m'
GREEN='\033[32m'
RESET='\033[0m'

info() { printf "${GREEN}==>${RESET} %s\n" "$1"; }
err() { printf "${RED}error:${RESET} %s\n" "$1" >&2; }

usage() {
    cat <<USAGE
Usage: $0 [options]

Options:
  --suite NAME              Suite to preflight: control-plane, hydrate, cross-region, enterprise.
                            May be repeated. enterprise expands to control-plane + hydrate + cross-region.
  --storage-provider NAME   Storage control-plane provider: s3, gcs, azure, all, none.
                            May be repeated.
  --coordinator NAME        Coordinator provider: dynamodb, spanner, cosmosdb, all, none.
                            May be repeated.
  --hydrate-provider NAME   Binary hydrate provider: s3, gcs, azure, all, none.
                            May be repeated.
  --mutate                  Require CRAB_REPLICA_LIVE_MUTATE=1.
  --require-evidence        Require CRAB_REPLICA_LIVE_EVIDENCE_DIR.
  --require-redacted        Require CRAB_REPLICA_LIVE_EVIDENCE_REDACT=1.
  --require-cloud-credentials
                            Require ambient provider credentials for every selected cloud.
  --require-repair-worker-deployment
                            Require CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE
                            to point at retained deployment proof for the repair worker.
  --evidence-profile NAME   Verifier profile that will be used after the live run.
                            enterprise also requires CRAB_REPLICA_LIVE_RUN_ID.
                            enterprise also requires CRAB_REPLICA_LIVE_PRODUCTION_LOAD=1
                            so the matrix runner records load evidence.
  -h, --help                Show this help.

Examples:
  $0 --suite control-plane --storage-provider s3 --coordinator dynamodb --mutate --require-evidence --require-redacted --evidence-profile control-plane-mutate
  $0 --suite enterprise --storage-provider all --coordinator all --hydrate-provider all --mutate --require-evidence --require-redacted --evidence-profile enterprise
USAGE
}

append_unique() {
    local value="$1"
    local target="$2"
    local existing

    case "$target" in
        SUITES)
            for existing in ${SUITES[@]+"${SUITES[@]}"}; do
                [[ "$existing" == "$value" ]] && return
            done
            SUITES+=("$value")
            ;;
        STORAGE_PROVIDERS)
            for existing in ${STORAGE_PROVIDERS[@]+"${STORAGE_PROVIDERS[@]}"}; do
                [[ "$existing" == "$value" ]] && return
            done
            STORAGE_PROVIDERS+=("$value")
            ;;
        COORDINATORS)
            for existing in ${COORDINATORS[@]+"${COORDINATORS[@]}"}; do
                [[ "$existing" == "$value" ]] && return
            done
            COORDINATORS+=("$value")
            ;;
        HYDRATE_PROVIDERS)
            for existing in ${HYDRATE_PROVIDERS[@]+"${HYDRATE_PROVIDERS[@]}"}; do
                [[ "$existing" == "$value" ]] && return
            done
            HYDRATE_PROVIDERS+=("$value")
            ;;
        CLOUDS)
            for existing in ${CLOUDS[@]+"${CLOUDS[@]}"}; do
                [[ "$existing" == "$value" ]] && return
            done
            CLOUDS+=("$value")
            ;;
        *)
            err "internal error: unknown array '$target'"
            exit 2
            ;;
    esac
}

expand_provider() {
    local value="$1"
    local target="$2"
    shift 2
    local allowed=("$@")

    case "$value" in
        none)
            ;;
        all)
            local provider
            for provider in "${allowed[@]}"; do
                append_unique "$provider" "$target"
            done
            ;;
        *)
            local provider
            for provider in "${allowed[@]}"; do
                if [[ "$provider" == "$value" ]]; then
                    append_unique "$value" "$target"
                    return
                fi
            done
            err "unsupported provider '$value'"
            exit 2
            ;;
    esac
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --suite)
            [[ $# -ge 2 ]] || { err "--suite requires a value"; exit 2; }
            case "$2" in
                control-plane|hydrate|cross-region)
                    append_unique "$2" SUITES
                    ;;
                enterprise)
                    ENTERPRISE_SUITE=true
                    append_unique control-plane SUITES
                    append_unique hydrate SUITES
                    append_unique cross-region SUITES
                    ;;
                *)
                    err "unsupported suite '$2'"
                    exit 2
                    ;;
            esac
            shift 2
            ;;
        --storage-provider)
            [[ $# -ge 2 ]] || { err "--storage-provider requires a value"; exit 2; }
            expand_provider "$2" STORAGE_PROVIDERS s3 gcs azure
            shift 2
            ;;
        --coordinator)
            [[ $# -ge 2 ]] || { err "--coordinator requires a value"; exit 2; }
            expand_provider "$2" COORDINATORS dynamodb spanner cosmosdb
            shift 2
            ;;
        --hydrate-provider)
            [[ $# -ge 2 ]] || { err "--hydrate-provider requires a value"; exit 2; }
            expand_provider "$2" HYDRATE_PROVIDERS s3 gcs azure
            shift 2
            ;;
        --mutate)
            REQUIRE_MUTATE=true
            shift
            ;;
        --require-evidence)
            REQUIRE_EVIDENCE=true
            shift
            ;;
        --require-redacted)
            REQUIRE_EVIDENCE=true
            REQUIRE_REDACTED=true
            shift
            ;;
        --require-cloud-credentials)
            REQUIRE_CLOUD_CREDENTIALS=true
            shift
            ;;
        --require-repair-worker-deployment)
            REQUIRE_REPAIR_WORKER_DEPLOYMENT=true
            shift
            ;;
        --evidence-profile)
            [[ $# -ge 2 ]] || { err "--evidence-profile requires a value"; exit 2; }
            case "$2" in
                control-plane-status|control-plane-mutate|provider-hydrate|active-active-smoke|enterprise)
                    EVIDENCE_PROFILE="$2"
                    ;;
                *)
                    err "unsupported evidence profile '$2'"
                    exit 2
                    ;;
            esac
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            err "unknown option: $1"
            exit 2
            ;;
    esac
done

if [[ ${#SUITES[@]} -eq 0 ]]; then
    err "at least one --suite is required"
    exit 2
fi

is_enabled() {
    local name="$1"
    local value="${!name:-}"
    local normalized
    normalized="$(printf '%s' "$value" | tr '[:upper:]' '[:lower:]')"
    case "$normalized" in
        1|true|yes|on) return 0 ;;
        *) return 1 ;;
    esac
}

missing=()

require_enabled() {
    local name="$1"
    is_enabled "$name" || missing+=("$name=1")
}

require_value() {
    local name="$1"
    [[ -n "${!name:-}" ]] || missing+=("$name")
}

require_live_run_attempt_id() {
    require_value CRAB_REPLICA_LIVE_RUN_ID
    if [[ -n "${CRAB_REPLICA_LIVE_RUN_ID:-}" && ! "${CRAB_REPLICA_LIVE_RUN_ID}" =~ ^replica-live-[0-9]+-[0-9]+$ ]]; then
        missing+=("CRAB_REPLICA_LIVE_RUN_ID matching replica-live-<github-run-id>-<attempt>")
    fi
}

is_secure_artifact_uri() {
    local value="$1"
    local rest
    [[ "$value" =~ [[:space:]] ]] && return 1
    case "$value" in
        https://*|s3://*|gs://*|az://*|azure://*) ;;
        *) return 1 ;;
    esac
    rest="${value#*://}"
    [[ "$rest" == "$value" ]] && return 1
    is_complete_uri_rest "$rest"
}

is_complete_uri_rest() {
    local rest="$1"
    local authority path
    [[ -n "$rest" && ! "$rest" =~ [[:space:]] ]] || return 1
    authority="${rest%%/*}"
    [[ "$authority" == "$rest" || -z "$authority" || "$authority" == *"@"* ]] && return 1
    path="${rest#*/}"
    [[ -n "$path" ]] || return 1
    [[ "$path" != /* && "$path" != *//* && "$path" != */ && "$path" != *\?* && "$path" != *\#* ]] || return 1
    case "$path" in
        .|..|./*|../*|*/./*|*/../*|*/.|*/..) return 1 ;;
    esac
    return 0
}

is_supported_writer_url() {
    local value="$1"
    local rest
    [[ "$value" =~ [[:space:]] ]] && return 1
    case "$value" in
        crab://*|s3://*|gs://*|az://*|azure://*) ;;
        *) return 1 ;;
    esac
    rest="${value#*://}"
    [[ "$rest" == "$value" ]] && return 1
    is_complete_uri_rest "$rest"
}

require_supported_writer_url() {
    local label="$1"
    local name="$2"
    local value="$3"
    if [[ -z "$(trim_value "$value")" ]]; then
        return
    fi
    if ! is_supported_writer_url "$value"; then
        missing+=("$label requires $name to use crab://, s3://, gs://, az://, or azure:// with bucket/container and repo path")
    fi
}

require_artifact_ref() {
    local name="$1"
    local value="${!name:-}"
    if [[ -z "$value" ]]; then
        missing+=("$name")
        return
    fi
    if is_secure_artifact_uri "$value"; then
        return
    fi
    case "$value" in
        http://*)
            missing+=("$name secure artifact URI or relative path inside CRAB_REPLICA_LIVE_EVIDENCE_DIR")
            ;;
        https://*|s3://*|gs://*|az://*|azure://*)
            missing+=("$name complete secure artifact URI or relative path inside CRAB_REPLICA_LIVE_EVIDENCE_DIR")
            ;;
        *)
            if [[ "$REQUIRE_EVIDENCE" == true ]]; then
                case "$value" in
                    /*|.|..|./*|../*|*/.|*/..|*/./*|*/../*)
                        missing+=("$name relative path inside CRAB_REPLICA_LIVE_EVIDENCE_DIR or secure artifact URI")
                        return
                        ;;
                esac
                if [[ -z "${CRAB_REPLICA_LIVE_EVIDENCE_DIR:-}" || ! -f "${CRAB_REPLICA_LIVE_EVIDENCE_DIR%/}/$value" ]]; then
                    missing+=("$name existing relative file inside CRAB_REPLICA_LIVE_EVIDENCE_DIR or secure artifact URI")
                fi
            elif [[ ! -f "$value" ]]; then
                missing+=("$name existing file or secure artifact URI")
            fi
            ;;
    esac
}

require_provider_log_evidence() {
    local provider="$1"
    local prefix
    prefix="CRAB_REPLICA_LIVE_$(upper_provider "$provider")"
    require_artifact_ref "${prefix}_PROVIDER_LOG_EVIDENCE"
}

require_one_of() {
    local label="$1"
    shift
    local name
    for name in "$@"; do
        [[ -n "${!name:-}" ]] && return
    done
    missing+=("$label (${*})")
}

has_value() {
    local name="$1"
    [[ -n "${!name:-}" ]]
}

trim_value() {
    printf '%s' "$1" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//'
}

value_or_env() {
    local preferred="$1"
    local fallback="${2:-}"
    if has_value "$preferred"; then
        printf '%s' "${!preferred}"
        return
    fi
    if [[ -n "$fallback" ]] && has_value "$fallback"; then
        printf '%s' "${!fallback}"
    fi
    return 0
}

require_distinct_values() {
    local label="$1"
    local left_name="$2"
    local left_value
    left_value="$(trim_value "$3")"
    local right_name="$4"
    local right_value
    right_value="$(trim_value "$5")"
    if [[ -n "$left_value" && -n "$right_value" && "$left_value" == "$right_value" ]]; then
        missing+=("$label requires distinct values for $left_name and $right_name")
    fi
}

coordinator_provider_from_url() {
    local value
    value="$(trim_value "$1")"
    case "$value" in
        dynamodb://*) printf 'dynamodb' ;;
        spanner://*) printf 'spanner' ;;
        cosmosdb://*) printf 'cosmosdb' ;;
        *) printf '' ;;
    esac
}

require_supported_coordinator_url() {
    local label="$1"
    local name="$2"
    local value="$3"
    if [[ -z "$(trim_value "$value")" ]]; then
        return
    fi
    if [[ -z "$(coordinator_provider_from_url "$value")" ]]; then
        missing+=("$label requires $name to use dynamodb://, spanner://, or cosmosdb://")
    fi
}

require_coordinator_url_provider() {
    local coordinator="$1"
    local name="$2"
    local value="$3"
    if [[ -z "$(trim_value "$value")" ]]; then
        return
    fi
    local actual
    actual="$(coordinator_provider_from_url "$value")"
    if [[ -z "$actual" ]]; then
        missing+=("active-active smoke coordinator URL for $coordinator requires $name to use ${coordinator}://")
    elif [[ "$actual" != "$coordinator" ]]; then
        missing+=("active-active smoke coordinator URL for $coordinator requires $name to use ${coordinator}://, got ${actual}://")
    fi
}

select_cloud() {
    append_unique "$1" CLOUDS
}

select_cloud_for_storage_provider() {
    case "$1" in
        s3) select_cloud aws ;;
        gcs) select_cloud gcp ;;
        azure)
            select_cloud azure
            AZURE_MANAGEMENT_REQUIRED=true
            ;;
    esac
}

select_cloud_for_hydrate_provider() {
    case "$1" in
        s3) select_cloud aws ;;
        gcs) select_cloud gcp ;;
        azure) select_cloud azure ;;
    esac
}

select_cloud_for_coordinator() {
    case "$1" in
        dynamodb) select_cloud aws ;;
        spanner) select_cloud gcp ;;
        cosmosdb)
            select_cloud azure
            AZURE_MANAGEMENT_REQUIRED=true
            ;;
    esac
}

select_cloud_for_url() {
    local value="$1"
    case "$value" in
        s3://*) select_cloud aws ;;
        gs://*) select_cloud gcp ;;
        az://*|azure://*) select_cloud azure ;;
        dynamodb://*) select_cloud aws ;;
        spanner://*) select_cloud gcp ;;
        cosmosdb://*) select_cloud azure ;;
    esac
}

has_suite() {
    local name="$1"
    local suite
    for suite in ${SUITES[@]+"${SUITES[@]}"}; do
        [[ "$suite" == "$name" ]] && return 0
    done
    return 1
}

upper_provider() {
    case "$1" in
        s3) echo S3 ;;
        gcs) echo GCS ;;
        azure) echo AZURE ;;
        dynamodb) echo DYNAMODB ;;
        spanner) echo SPANNER ;;
        cosmosdb) echo COSMOSDB ;;
    esac
}

suite_count() {
    printf '%s\n' "${#SUITES[@]}"
}

require_evidence_profile() {
    local expected="$1"
    local suite="$2"
    if [[ -z "$EVIDENCE_PROFILE" ]]; then
        return
    fi
    if [[ "$EVIDENCE_PROFILE" != "$expected" ]]; then
        missing+=("evidence profile '$expected' for $suite suite (got '$EVIDENCE_PROFILE')")
    fi
}

require_all_selected() {
    local label="$1"
    shift
    local selected_name="$1"
    shift
    local selected_values=()
    case "$selected_name" in
        STORAGE_PROVIDERS)
            selected_values=(${STORAGE_PROVIDERS[@]+"${STORAGE_PROVIDERS[@]}"})
            ;;
        COORDINATORS)
            selected_values=(${COORDINATORS[@]+"${COORDINATORS[@]}"})
            ;;
        HYDRATE_PROVIDERS)
            selected_values=(${HYDRATE_PROVIDERS[@]+"${HYDRATE_PROVIDERS[@]}"})
            ;;
        *)
            err "internal error: unknown provider matrix '$selected_name'"
            exit 2
            ;;
    esac
    local required=("$@")
    local selected
    local required_value
    for required_value in "${required[@]}"; do
        local found=false
        for selected in ${selected_values[@]+"${selected_values[@]}"}; do
            if [[ "$selected" == "$required_value" ]]; then
                found=true
                break
            fi
        done
        if [[ "$found" != true ]]; then
            missing+=("enterprise evidence requires $label provider '$required_value'")
        fi
    done
}

has_aws_credentials() {
    if has_value AWS_PROFILE; then
        return 0
    fi
    if has_value AWS_ACCESS_KEY_ID && has_value AWS_SECRET_ACCESS_KEY; then
        return 0
    fi
    if has_value AWS_WEB_IDENTITY_TOKEN_FILE && has_value AWS_ROLE_ARN; then
        return 0
    fi
    if has_value AWS_CONTAINER_CREDENTIALS_RELATIVE_URI || has_value AWS_CONTAINER_CREDENTIALS_FULL_URI; then
        return 0
    fi
    return 1
}

has_gcp_credentials() {
    has_value GOOGLE_APPLICATION_CREDENTIALS \
        || has_value GOOGLE_APPLICATION_CREDENTIALS_JSON \
        || has_value GOOGLE_OAUTH_ACCESS_TOKEN
}

has_azure_credentials() {
    if ! has_value AZURE_TENANT_ID || ! has_value AZURE_CLIENT_ID; then
        return 1
    fi
    has_value AZURE_CLIENT_SECRET \
        || has_value AZURE_FEDERATED_TOKEN_FILE \
        || has_value AZURE_CLIENT_CERTIFICATE_PATH
}

validate_cloud_credentials() {
    if [[ "$REQUIRE_CLOUD_CREDENTIALS" != true ]]; then
        return
    fi

    local cloud
    for cloud in ${CLOUDS[@]+"${CLOUDS[@]}"}; do
        case "$cloud" in
            aws)
                if ! has_aws_credentials; then
                    missing+=("AWS credentials (AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, AWS_PROFILE, AWS_WEB_IDENTITY_TOKEN_FILE/AWS_ROLE_ARN, or container credentials)")
                fi
                ;;
            gcp)
                if ! has_gcp_credentials; then
                    missing+=("Google Cloud credentials (GOOGLE_APPLICATION_CREDENTIALS, GOOGLE_APPLICATION_CREDENTIALS_JSON, or GOOGLE_OAUTH_ACCESS_TOKEN)")
                fi
                ;;
            azure)
                if ! has_azure_credentials; then
                    missing+=("Azure credentials (AZURE_TENANT_ID, AZURE_CLIENT_ID, and AZURE_CLIENT_SECRET/AZURE_FEDERATED_TOKEN_FILE/AZURE_CLIENT_CERTIFICATE_PATH)")
                fi
                if [[ "$AZURE_MANAGEMENT_REQUIRED" == true ]]; then
                    require_value AZURE_SUBSCRIPTION_ID
                    require_value AZURE_RESOURCE_GROUP
                fi
                ;;
        esac
    done
}

validate_evidence_profile() {
    if [[ -z "$EVIDENCE_PROFILE" ]]; then
        return
    fi

    if [[ "$ENTERPRISE_SUITE" == true ]]; then
        require_evidence_profile enterprise enterprise
        if ! is_enabled CRAB_REPLICA_LIVE_MUTATE; then
            missing+=("CRAB_REPLICA_LIVE_MUTATE=1 for enterprise evidence profile")
        fi
        require_live_run_attempt_id
        require_artifact_ref CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE
        require_enabled CRAB_REPLICA_LIVE_PRODUCTION_LOAD
        require_all_selected "storage control-plane" STORAGE_PROVIDERS s3 gcs azure
        require_all_selected "coordinator" COORDINATORS dynamodb spanner cosmosdb
        require_all_selected "hydrate" HYDRATE_PROVIDERS s3 gcs azure
        for provider in s3 gcs azure dynamodb spanner cosmosdb; do
            require_provider_log_evidence "$provider"
        done
        return
    fi

    if [[ "$(suite_count)" -ne 1 ]]; then
        missing+=("one --evidence-profile cannot certify multiple independent suites; run separate evidence jobs or use --suite enterprise --evidence-profile enterprise")
        return
    fi

    if has_suite control-plane; then
        if is_enabled CRAB_REPLICA_LIVE_MUTATE; then
            require_evidence_profile control-plane-mutate control-plane
        else
            require_evidence_profile control-plane-status control-plane
        fi
    elif has_suite hydrate; then
        require_evidence_profile provider-hydrate hydrate
    elif has_suite cross-region; then
        require_evidence_profile active-active-smoke cross-region
        require_artifact_ref CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE
        if is_enabled CRAB_REPLICA_LIVE_PRODUCTION_LOAD; then
            missing+=("CRAB_REPLICA_LIVE_PRODUCTION_LOAD=1 requires --suite enterprise --evidence-profile enterprise")
        fi
    fi
}

validate_repair_service_template() {
    local value="${CRAB_REPLICA_LIVE_REPAIR_SERVICE_TEMPLATE:-}"
    if [[ -z "$value" ]]; then
        return
    fi
    local normalized
    normalized="$(printf '%s' "$value" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//' | tr '[:upper:]' '[:lower:]')"
    case "$normalized" in
        systemd|launchd|kubernetes)
            ;;
        *)
            missing+=("CRAB_REPLICA_LIVE_REPAIR_SERVICE_TEMPLATE must be systemd, launchd, or kubernetes")
            ;;
    esac
}

require_positive_integer_if_set() {
    local name="$1"
    local value="${!name:-}"
    if [[ -z "$value" ]]; then
        return
    fi
    if [[ ! "$value" =~ ^[1-9][0-9]*$ ]]; then
        missing+=("$name positive integer")
    fi
}

validate_load_config() {
    if ! has_suite cross-region; then
        return
    fi
    require_positive_integer_if_set CRAB_REPLICA_LIVE_LOAD_FILES
    require_positive_integer_if_set CRAB_REPLICA_LIVE_LOAD_FILE_BYTES
    require_positive_integer_if_set CRAB_REPLICA_LIVE_LOAD_PUSH_LATENCY_BUDGET_MS
    require_positive_integer_if_set CRAB_REPLICA_LIVE_LOAD_READ_LATENCY_BUDGET_MS
}

require_enabled CRAB_REPLICA_LIVE

if [[ "$REQUIRE_MUTATE" == true ]] || has_suite hydrate || has_suite cross-region; then
    require_enabled CRAB_REPLICA_LIVE_MUTATE
fi

validate_evidence_profile
validate_repair_service_template
validate_load_config

if [[ "$REQUIRE_EVIDENCE" == true ]]; then
    require_value CRAB_REPLICA_LIVE_EVIDENCE_DIR
fi

if [[ "$REQUIRE_REDACTED" == true ]]; then
    require_enabled CRAB_REPLICA_LIVE_EVIDENCE_REDACT
fi

if [[ "$REQUIRE_REPAIR_WORKER_DEPLOYMENT" == true ]]; then
    require_artifact_ref CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE
fi

if has_suite control-plane; then
    if [[ ${#STORAGE_PROVIDERS[@]} -eq 0 && ${#COORDINATORS[@]} -eq 0 ]]; then
        missing+=("at least one --storage-provider or --coordinator for control-plane")
    fi

    provider=""
    for provider in ${STORAGE_PROVIDERS[@]+"${STORAGE_PROVIDERS[@]}"}; do
        select_cloud_for_storage_provider "$provider"
        prefix="CRAB_REPLICA_LIVE_$(upper_provider "$provider")"
        require_enabled "$prefix"
        require_value "${prefix}_PRIMARY"
        require_value "${prefix}_REPLICA"
        require_value "${prefix}_REGION"
    done

    coordinator=""
    for coordinator in ${COORDINATORS[@]+"${COORDINATORS[@]}"}; do
        select_cloud_for_coordinator "$coordinator"
        prefix="CRAB_REPLICA_LIVE_$(upper_provider "$coordinator")"
        require_enabled "$prefix"
        require_value "${prefix}_NAME"
        require_value "${prefix}_REGION"
        require_value "${prefix}_FAILOVER_REGION"
        coordinator_region="$(value_or_env "${prefix}_REGION")"
        coordinator_failover_region="$(value_or_env "${prefix}_FAILOVER_REGION")"
        require_distinct_values "coordinator regions for $coordinator" \
            "${prefix}_REGION" "$coordinator_region" \
            "${prefix}_FAILOVER_REGION" "$coordinator_failover_region"
    done
fi

if has_suite hydrate; then
    if [[ ${#HYDRATE_PROVIDERS[@]} -eq 0 ]]; then
        missing+=("at least one --hydrate-provider for hydrate")
    fi

    provider=""
    for provider in ${HYDRATE_PROVIDERS[@]+"${HYDRATE_PROVIDERS[@]}"}; do
        select_cloud_for_hydrate_provider "$provider"
        prefix="CRAB_REPLICA_LIVE_$(upper_provider "$provider")_HYDRATE"
        require_enabled "$prefix"
        case "$provider" in
            azure)
                require_one_of "${prefix}_PRIMARY_CONTAINER" \
                    "${prefix}_PRIMARY_CONTAINER" "${prefix}_PRIMARY_BUCKET"
                require_one_of "${prefix}_REPLICA_CONTAINER" \
                    "${prefix}_REPLICA_CONTAINER" "${prefix}_REPLICA_BUCKET"
                require_one_of "${prefix}_PRIMARY_ACCOUNT" \
                    "${prefix}_PRIMARY_ACCOUNT" AZURE_STORAGE_ACCOUNT
                require_one_of "${prefix}_REPLICA_ACCOUNT" \
                    "${prefix}_REPLICA_ACCOUNT" AZURE_STORAGE_ACCOUNT
                ;;
            s3|gcs)
                require_value "${prefix}_PRIMARY_BUCKET"
                require_value "${prefix}_REPLICA_BUCKET"
                ;;
        esac
        require_one_of "${prefix}_REGION" \
            "${prefix}_REGION" AWS_REGION GOOGLE_CLOUD_REGION AZURE_REGION
    done
fi

if has_suite cross-region; then
    require_enabled CRAB_REPLICA_LIVE_CROSS_REGION
    if [[ ${#COORDINATORS[@]} -eq 0 ]]; then
        require_value CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL
        require_value CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION
        require_value CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL
        require_value CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION
        require_value CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL
        require_value CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION
        require_value CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION
        require_supported_coordinator_url "active-active smoke coordinator URL" \
            CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL "${CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL:-}"
        require_distinct_values "active-active smoke coordinator regions" \
            CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION "${CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION:-}" \
            CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION "${CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION:-}"
        require_distinct_values "active-active smoke writer URLs" \
            CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL "${CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL:-}" \
            CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL "${CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL:-}"
        require_supported_writer_url "active-active smoke writer URL" \
            CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL "${CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL:-}"
        require_supported_writer_url "active-active smoke writer URL" \
            CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL "${CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL:-}"
        require_distinct_values "active-active smoke writer regions" \
            CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION "${CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION:-}" \
            CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION "${CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION:-}"
        select_cloud_for_url "${CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL:-}"
        select_cloud_for_url "${CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL:-}"
        select_cloud_for_url "${CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL:-}"
    elif [[ ${#COORDINATORS[@]} -eq 1 ]]; then
        coordinator="${COORDINATORS[0]}"
        select_cloud_for_coordinator "$coordinator"
        prefix="CRAB_REPLICA_LIVE_$(upper_provider "$coordinator")"
        smoke_prefix="${prefix}_SMOKE"
        require_enabled "$prefix"
        require_value "${prefix}_NAME"
        require_one_of "${smoke_prefix}_WRITER_A_URL" \
            "${smoke_prefix}_WRITER_A_URL" CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL
        require_one_of "${smoke_prefix}_WRITER_A_REGION" \
            "${smoke_prefix}_WRITER_A_REGION" CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION
        require_one_of "${smoke_prefix}_WRITER_B_URL" \
            "${smoke_prefix}_WRITER_B_URL" CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL
        require_one_of "${smoke_prefix}_WRITER_B_REGION" \
            "${smoke_prefix}_WRITER_B_REGION" CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION
        require_one_of "${smoke_prefix}_COORDINATOR_URL" \
            "${smoke_prefix}_COORDINATOR_URL" CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL
        require_one_of "${smoke_prefix}_COORDINATOR_REGION" \
            "${smoke_prefix}_COORDINATOR_REGION" "${prefix}_REGION" CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION
        require_one_of "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" \
            "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" "${prefix}_FAILOVER_REGION" CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION
        coordinator_url="$(value_or_env "${smoke_prefix}_COORDINATOR_URL" CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_URL)"
        require_coordinator_url_provider "$coordinator" \
            "${smoke_prefix}_COORDINATOR_URL" "$coordinator_url"
        coordinator_region="$(value_or_env "${smoke_prefix}_COORDINATOR_REGION" "${prefix}_REGION")"
        if [[ -z "$coordinator_region" ]]; then
            coordinator_region="$(value_or_env "${smoke_prefix}_COORDINATOR_REGION" CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_REGION)"
        fi
        coordinator_failover_region="$(value_or_env "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" "${prefix}_FAILOVER_REGION")"
        if [[ -z "$coordinator_failover_region" ]]; then
            coordinator_failover_region="$(value_or_env "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" CRAB_REPLICA_LIVE_SMOKE_COORDINATOR_FAILOVER_REGION)"
        fi
        require_distinct_values "active-active smoke coordinator regions for $coordinator" \
            "${smoke_prefix}_COORDINATOR_REGION" "$coordinator_region" \
            "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" "$coordinator_failover_region"
        writer_a_url="$(value_or_env "${smoke_prefix}_WRITER_A_URL" CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL)"
        writer_b_url="$(value_or_env "${smoke_prefix}_WRITER_B_URL" CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL)"
        require_distinct_values "active-active smoke writer URLs for $coordinator" \
            "${smoke_prefix}_WRITER_A_URL" "$writer_a_url" \
            "${smoke_prefix}_WRITER_B_URL" "$writer_b_url"
        require_supported_writer_url "active-active smoke writer URL for $coordinator" \
            "${smoke_prefix}_WRITER_A_URL" "$writer_a_url"
        require_supported_writer_url "active-active smoke writer URL for $coordinator" \
            "${smoke_prefix}_WRITER_B_URL" "$writer_b_url"
        writer_a_region="$(value_or_env "${smoke_prefix}_WRITER_A_REGION" CRAB_REPLICA_LIVE_SMOKE_WRITER_A_REGION)"
        writer_b_region="$(value_or_env "${smoke_prefix}_WRITER_B_REGION" CRAB_REPLICA_LIVE_SMOKE_WRITER_B_REGION)"
        require_distinct_values "active-active smoke writer regions for $coordinator" \
            "${smoke_prefix}_WRITER_A_REGION" "$writer_a_region" \
            "${smoke_prefix}_WRITER_B_REGION" "$writer_b_region"
        select_cloud_for_url "$(value_or_env "${smoke_prefix}_WRITER_A_URL" CRAB_REPLICA_LIVE_SMOKE_WRITER_A_URL)"
        select_cloud_for_url "$(value_or_env "${smoke_prefix}_WRITER_B_URL" CRAB_REPLICA_LIVE_SMOKE_WRITER_B_URL)"
        select_cloud_for_url "$coordinator_url"
    else
        coordinator=""
        for coordinator in ${COORDINATORS[@]+"${COORDINATORS[@]}"}; do
            select_cloud_for_coordinator "$coordinator"
            prefix="CRAB_REPLICA_LIVE_$(upper_provider "$coordinator")"
            smoke_prefix="${prefix}_SMOKE"
            require_enabled "$prefix"
            require_value "${prefix}_NAME"
            require_value "${smoke_prefix}_WRITER_A_URL"
            require_value "${smoke_prefix}_WRITER_A_REGION"
            require_value "${smoke_prefix}_WRITER_B_URL"
            require_value "${smoke_prefix}_WRITER_B_REGION"
            require_value "${smoke_prefix}_COORDINATOR_URL"
            require_one_of "${smoke_prefix}_COORDINATOR_REGION" \
                "${smoke_prefix}_COORDINATOR_REGION" "${prefix}_REGION"
            require_one_of "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" \
                "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" "${prefix}_FAILOVER_REGION"
            coordinator_url="$(value_or_env "${smoke_prefix}_COORDINATOR_URL")"
            require_coordinator_url_provider "$coordinator" \
                "${smoke_prefix}_COORDINATOR_URL" "$coordinator_url"
            coordinator_region="$(value_or_env "${smoke_prefix}_COORDINATOR_REGION" "${prefix}_REGION")"
            coordinator_failover_region="$(value_or_env "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" "${prefix}_FAILOVER_REGION")"
            require_distinct_values "active-active smoke coordinator regions for $coordinator" \
                "${smoke_prefix}_COORDINATOR_REGION" "$coordinator_region" \
                "${smoke_prefix}_COORDINATOR_FAILOVER_REGION" "$coordinator_failover_region"
            writer_a_url="$(value_or_env "${smoke_prefix}_WRITER_A_URL")"
            writer_b_url="$(value_or_env "${smoke_prefix}_WRITER_B_URL")"
            require_distinct_values "active-active smoke writer URLs for $coordinator" \
                "${smoke_prefix}_WRITER_A_URL" "$writer_a_url" \
                "${smoke_prefix}_WRITER_B_URL" "$writer_b_url"
            require_supported_writer_url "active-active smoke writer URL for $coordinator" \
                "${smoke_prefix}_WRITER_A_URL" "$writer_a_url"
            require_supported_writer_url "active-active smoke writer URL for $coordinator" \
                "${smoke_prefix}_WRITER_B_URL" "$writer_b_url"
            writer_a_region="$(value_or_env "${smoke_prefix}_WRITER_A_REGION")"
            writer_b_region="$(value_or_env "${smoke_prefix}_WRITER_B_REGION")"
            require_distinct_values "active-active smoke writer regions for $coordinator" \
                "${smoke_prefix}_WRITER_A_REGION" "$writer_a_region" \
                "${smoke_prefix}_WRITER_B_REGION" "$writer_b_region"
            select_cloud_for_url "$(value_or_env "${smoke_prefix}_WRITER_A_URL")"
            select_cloud_for_url "$(value_or_env "${smoke_prefix}_WRITER_B_URL")"
            select_cloud_for_url "$coordinator_url"
        done
    fi
fi

validate_cloud_credentials

if [[ ${#missing[@]} -gt 0 ]]; then
    err "live replica evidence preflight failed; missing required environment:"
    item=""
    for item in "${missing[@]}"; do
        printf '  - %s\n' "$item" >&2
    done
    exit 1
fi

info "live replica evidence preflight passed"
