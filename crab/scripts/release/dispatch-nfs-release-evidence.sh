#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crab_dir="$(cd "$script_dir/../.." && pwd)"
repo_root="$(cd "$crab_dir/.." && pwd)"

self_test() {
  local tmpdir
  tmpdir="$(mktemp -d)"
  trap 'rm -rf "$tmpdir"' RETURN

  local expected_sha
  expected_sha="$(git -C "$repo_root" rev-parse HEAD)"
  local mock_bin="$tmpdir/gh"
  local output_file="$tmpdir/nfs-release-evidence.env"
  local stdout_file="$tmpdir/stdout.txt"
  local log_file="$tmpdir/gh.log"

  cat >"$mock_bin" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>"$CRAB_NFS_RELEASE_EVIDENCE_MOCK_LOG"
if [ "$1 $2 $3" = "workflow run nfs-mount.yml" ]; then
  exit 0
fi
if [ "$1 $2" = "run list" ]; then
  printf '[{"databaseId":12345,"headSha":"%s","status":"completed","conclusion":"success","createdAt":"2999-01-01T00:00:00Z","url":"https://example.invalid/run/12345","event":"workflow_dispatch","attempt":2}]\n' "$CRAB_NFS_RELEASE_EVIDENCE_MOCK_SHA"
  exit 0
fi
if [ "$1 $2" = "run view" ]; then
  conclusion="${CRAB_NFS_RELEASE_EVIDENCE_MOCK_CONCLUSION:-success}"
  head_sha="${CRAB_NFS_RELEASE_EVIDENCE_MOCK_VIEW_SHA:-$CRAB_NFS_RELEASE_EVIDENCE_MOCK_SHA}"
  printf '{"status":"completed","conclusion":"%s","url":"https://example.invalid/run/12345","attempt":2,"headSha":"%s"}\n' "$conclusion" "$head_sha"
  exit 0
fi
printf 'unexpected gh args: %s\n' "$*" >&2
exit 64
EOF
  chmod +x "$mock_bin"

  PATH="$tmpdir:$PATH" \
  CRAB_NFS_RELEASE_EVIDENCE_MOCK_LOG="$log_file" \
  CRAB_NFS_RELEASE_EVIDENCE_MOCK_SHA="$expected_sha" \
  NFS_RELEASE_EVIDENCE_REF=HEAD \
  NFS_RELEASE_EVIDENCE_WAIT=1 \
  NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS=5 \
  NFS_RELEASE_EVIDENCE_POLL_SECS=1 \
  NFS_RELEASE_EVIDENCE_OUTPUT="$output_file" \
  NFS_SMOKE_BASELINE_RUN_ID="22222" \
  NFS_SMOKE_VERIFY_ARGS="--max-native-read-rpcs-per-mib 99" \
  NFS_SMOKE_COMPARE_ARGS="--max-native-read-rpc-density-regression-pct 15" \
  NFS_THRESHOLD_MIN_SMOKE_SUMMARIES="4" \
  NFS_THRESHOLD_REQUIRE_CALIBRATION_READY="1" \
  NFS_THRESHOLD_REQUIRE_RELEASE_GRADE="1" \
    bash "$0" >"$stdout_file"

  grep -q "NFS_RELEASE_EVIDENCE_RUN_ID=12345" "$stdout_file"
  grep -q "NFS_RELEASE_EXPECTED_RUN_SUFFIX=12345-2" "$stdout_file"
  grep -q "Wrote release evidence variables" "$stdout_file"
  grep -q "^NFS_RELEASE_EVIDENCE_RUN_ID='12345'$" "$output_file"
  grep -q "^NFS_RELEASE_EXPECTED_RUN_SUFFIX='12345-2'$" "$output_file"
  grep -q "^NFS_RELEASE_EVIDENCE_URL='https://example.invalid/run/12345'$" "$output_file"
  grep -q "^NFS_RELEASE_EVIDENCE_GIT_COMMIT='$expected_sha'$" "$output_file"
  (
    set -euo pipefail
    # shellcheck disable=SC1090
    source "$output_file"
    [ "$NFS_RELEASE_EVIDENCE_RUN_ID" = "12345" ]
    [ "$NFS_RELEASE_EXPECTED_RUN_SUFFIX" = "12345-2" ]
    [ "$NFS_RELEASE_EVIDENCE_URL" = "https://example.invalid/run/12345" ]
    [ "$NFS_RELEASE_EVIDENCE_GIT_COMMIT" = "$expected_sha" ]
  )
  grep -q "workflow run nfs-mount.yml --ref HEAD" "$log_file"
  grep -q -- "-f nfs_smoke_baseline_run_id=22222" "$log_file"
  grep -q -- "-f nfs_smoke_verify_args=--max-native-read-rpcs-per-mib 99" "$log_file"
  grep -q -- "-f nfs_smoke_compare_args=--max-native-read-rpc-density-regression-pct 15" "$log_file"
  grep -q -- "-f nfs_threshold_min_smoke_summaries=4" "$log_file"
  grep -q -- "-f nfs_threshold_require_calibration_ready=true" "$log_file"
  grep -q -- "-f nfs_threshold_require_release_grade=true" "$log_file"
  grep -q "run view 12345" "$log_file"

  local failed_stdout="$tmpdir/failed-stdout.txt"
  local failed_stderr="$tmpdir/failed-stderr.txt"
  if PATH="$tmpdir:$PATH" \
    CRAB_NFS_RELEASE_EVIDENCE_MOCK_LOG="$log_file" \
    CRAB_NFS_RELEASE_EVIDENCE_MOCK_SHA="$expected_sha" \
    CRAB_NFS_RELEASE_EVIDENCE_MOCK_CONCLUSION="failure" \
    NFS_RELEASE_EVIDENCE_REF=HEAD \
    NFS_RELEASE_EVIDENCE_WAIT=1 \
    NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS=5 \
    NFS_RELEASE_EVIDENCE_POLL_SECS=1 \
      bash "$0" >"$failed_stdout" 2>"$failed_stderr"; then
    echo "error: failed NFS Mount Evidence run was not rejected" >&2
    exit 1
  fi
  grep -q "completed with conclusion 'failure'" "$failed_stderr"

  local mismatch_stdout="$tmpdir/mismatch-stdout.txt"
  local mismatch_stderr="$tmpdir/mismatch-stderr.txt"
  if PATH="$tmpdir:$PATH" \
    CRAB_NFS_RELEASE_EVIDENCE_MOCK_LOG="$log_file" \
    CRAB_NFS_RELEASE_EVIDENCE_MOCK_SHA="$expected_sha" \
    CRAB_NFS_RELEASE_EVIDENCE_MOCK_VIEW_SHA="0000000000000000000000000000000000000000" \
    NFS_RELEASE_EVIDENCE_REF=HEAD \
    NFS_RELEASE_EVIDENCE_WAIT=1 \
    NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS=5 \
    NFS_RELEASE_EVIDENCE_POLL_SECS=1 \
      bash "$0" >"$mismatch_stdout" 2>"$mismatch_stderr"; then
    echo "error: mismatched NFS Mount Evidence headSha was not rejected" >&2
    exit 1
  fi
  grep -q "headSha changed unexpectedly" "$mismatch_stderr"

  echo "ok: NFS release evidence dispatch self-test passed"
}

if [ "${1:-}" = "self-test" ]; then
  self_test
  exit 0
fi

if ! command -v gh >/dev/null 2>&1; then
  echo "gh CLI is required to dispatch NFS release evidence" >&2
  exit 127
fi

wait_for_completion="${NFS_RELEASE_EVIDENCE_WAIT:-0}"
wait_timeout_secs="${NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS:-7200}"
poll_secs="${NFS_RELEASE_EVIDENCE_POLL_SECS:-30}"
evidence_output="${NFS_RELEASE_EVIDENCE_OUTPUT:-}"

case "$wait_for_completion" in
  1|true|TRUE|yes|YES) wait_for_completion=1 ;;
  0|false|FALSE|no|NO|"") wait_for_completion=0 ;;
  *)
    echo "NFS_RELEASE_EVIDENCE_WAIT must be 0/1, true/false, or yes/no" >&2
    exit 2
    ;;
esac

case "$wait_timeout_secs" in
  ''|*[!0-9]*)
    echo "NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS must be a positive integer" >&2
    exit 2
    ;;
esac
if [ "$wait_timeout_secs" -le 0 ]; then
  echo "NFS_RELEASE_EVIDENCE_WAIT_TIMEOUT_SECS must be a positive integer" >&2
  exit 2
fi

case "$poll_secs" in
  ''|*[!0-9]*)
    echo "NFS_RELEASE_EVIDENCE_POLL_SECS must be a positive integer" >&2
    exit 2
    ;;
esac
if [ "$poll_secs" -le 0 ]; then
  echo "NFS_RELEASE_EVIDENCE_POLL_SECS must be a positive integer" >&2
  exit 2
fi

if [ "$wait_for_completion" = "1" ] && ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required when NFS_RELEASE_EVIDENCE_WAIT=1" >&2
  exit 127
fi

ref="${NFS_RELEASE_EVIDENCE_REF:-}"
if [ -z "$ref" ]; then
  version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$crab_dir/Cargo.toml" | head -1)"
  tag="v${version}"
  if [ -n "$version" ] && git -C "$repo_root" rev-parse --verify --quiet "$tag^{commit}" >/dev/null; then
    ref="$tag"
  else
    ref="$(git -C "$repo_root" branch --show-current)"
  fi
fi

if [ -z "$ref" ]; then
  {
    echo "NFS_RELEASE_EVIDENCE_REF is required when the current checkout is detached and the release tag is absent."
    echo "Use a branch or tag that points at the exact release commit."
  } >&2
  exit 2
fi

expected_sha=""
if [ "$wait_for_completion" = "1" ]; then
  expected_sha="$(git -C "$repo_root" rev-parse --verify "$ref^{commit}" 2>/dev/null || true)"
  if [ -z "$expected_sha" ]; then
    {
      echo "failed to resolve NFS_RELEASE_EVIDENCE_REF '$ref' to a local commit."
      echo "Fetch the release branch/tag first, or set NFS_RELEASE_EVIDENCE_REF to a local ref."
    } >&2
    exit 2
  fi
fi

args=(workflow run nfs-mount.yml --ref "$ref")

add_input() {
  local name="$1"
  local value="$2"
  if [ -n "$value" ]; then
    args+=(-f "$name=$value")
  fi
}

add_bool_input() {
  local name="$1"
  local value="$2"
  case "$value" in
    "") ;;
    1|true|TRUE|yes|YES) add_input "$name" "true" ;;
    0|false|FALSE|no|NO) add_input "$name" "false" ;;
    *)
      echo "$name must be 0/1, true/false, yes/no, or empty" >&2
      exit 2
      ;;
  esac
}

add_input "nfs_smoke_baseline_run_id" "${NFS_SMOKE_BASELINE_RUN_ID:-}"
add_input "nfs_smoke_verify_args" "${NFS_SMOKE_VERIFY_ARGS:-}"
add_input "nfs_smoke_compare_args" "${NFS_SMOKE_COMPARE_ARGS:-}"
add_input "nfs_threshold_min_smoke_summaries" "${NFS_THRESHOLD_MIN_SMOKE_SUMMARIES:-}"
add_bool_input "nfs_threshold_require_calibration_ready" "${NFS_THRESHOLD_REQUIRE_CALIBRATION_READY:-}"
add_bool_input "nfs_threshold_require_release_grade" "${NFS_THRESHOLD_REQUIRE_RELEASE_GRADE:-}"

select_matching_run() {
  EXPECTED_SHA="$expected_sha" DISPATCH_EPOCH="$dispatch_epoch" python3 -c '
import datetime
import json
import os
import sys

expected_sha = os.environ["EXPECTED_SHA"]
dispatch_epoch = float(os.environ["DISPATCH_EPOCH"])
try:
    runs = json.load(sys.stdin)
except json.JSONDecodeError as error:
    print(f"failed to parse gh run list JSON: {error}", file=sys.stderr)
    sys.exit(2)

cutoff = dispatch_epoch - 120.0
for run in runs:
    if run.get("headSha") != expected_sha:
        continue
    if run.get("event") != "workflow_dispatch":
        continue
    created_at = run.get("createdAt")
    if created_at:
        try:
            created = datetime.datetime.fromisoformat(created_at.replace("Z", "+00:00")).timestamp()
        except ValueError:
            created = 0.0
        if created < cutoff:
            continue
    print(
        "\t".join(
            str(run.get(key) or "")
            for key in ("databaseId", "status", "conclusion", "url", "attempt", "createdAt")
        )
    )
    sys.exit(0)
'
}

summarize_run_view() {
  python3 -c '
import json
import sys

try:
    run = json.load(sys.stdin)
except json.JSONDecodeError as error:
    print(f"failed to parse gh run view JSON: {error}", file=sys.stderr)
    sys.exit(2)

print(
    "\t".join(
        str(run.get(key) or "")
        for key in ("status", "conclusion", "url", "attempt", "headSha")
    )
)
'
}

shell_quote() {
  local value="$1"
  printf "'"
  printf "%s" "$value" | sed "s/'/'\\\\''/g"
  printf "'"
}

write_env_assignment() {
  local name="$1"
  local value="$2"
  printf "%s=" "$name"
  shell_quote "$value"
  printf "\n"
}

write_evidence_env() {
  local run_id_value="$1"
  local attempt_value="$2"
  local run_url_value="$3"
  local expected_sha_value="$4"
  {
    write_env_assignment "NFS_RELEASE_EVIDENCE_RUN_ID" "$run_id_value"
    write_env_assignment "NFS_RELEASE_EXPECTED_RUN_SUFFIX" "$run_id_value-$attempt_value"
    write_env_assignment "NFS_RELEASE_EVIDENCE_URL" "$run_url_value"
    write_env_assignment "NFS_RELEASE_EVIDENCE_GIT_COMMIT" "$expected_sha_value"
  } >"$evidence_output"
}

wait_for_evidence_run() {
  local deadline=$((SECONDS + wait_timeout_secs))
  local run_id=""
  local run_url=""
  local attempt=""

  echo "waiting for NFS Mount Evidence run on commit $expected_sha"
  while [ "$SECONDS" -lt "$deadline" ]; do
    local runs_json
    runs_json="$(gh run list --workflow nfs-mount.yml --limit 50 --json databaseId,headSha,status,conclusion,createdAt,url,event,attempt)"
    local selected
    selected="$(printf '%s' "$runs_json" | select_matching_run)"

    if [ -z "$selected" ]; then
      echo "waiting for workflow_dispatch run to appear for $expected_sha..."
      sleep "$poll_secs"
      continue
    fi

    local status
    local conclusion
    local created_at
    IFS=$'\t' read -r run_id status conclusion run_url attempt created_at <<<"$selected"
    echo "found NFS Mount Evidence run $run_id (status=$status, attempt=$attempt, created_at=$created_at)"

    while [ "$SECONDS" -lt "$deadline" ]; do
      local view_json
      view_json="$(gh run view "$run_id" --json status,conclusion,url,attempt,headSha)"
      local view_summary
      view_summary="$(printf '%s' "$view_json" | summarize_run_view)"
      local head_sha
      IFS=$'\t' read -r status conclusion run_url attempt head_sha <<<"$view_summary"
      if [ "$head_sha" != "$expected_sha" ]; then
        echo "run $run_id headSha changed unexpectedly: expected $expected_sha, got $head_sha" >&2
        exit 1
      fi

      if [ "$status" = "completed" ]; then
        if [ "$conclusion" = "success" ]; then
          if [ -n "$evidence_output" ]; then
            local output_dir
            output_dir="$(dirname "$evidence_output")"
            if [ "$output_dir" != "." ]; then
              mkdir -p "$output_dir"
            fi
            write_evidence_env "$run_id" "$attempt" "$run_url" "$expected_sha"
          fi
          cat <<EOF

NFS Mount Evidence succeeded:
  $run_url

Use this exact run for release packaging:
  NFS_RELEASE_EVIDENCE_RUN_ID=$run_id
  NFS_RELEASE_EXPECTED_RUN_SUFFIX=$run_id-$attempt
  make release-ci NFS_RELEASE_EVIDENCE_RUN_ID=$run_id
EOF
          if [ -n "$evidence_output" ]; then
            echo "Wrote release evidence variables to $evidence_output"
            echo "Source it before release commands with: source \"$evidence_output\""
          fi
          return 0
        fi
        echo "NFS Mount Evidence run $run_id completed with conclusion '$conclusion': $run_url" >&2
        exit 1
      fi

      echo "run $run_id is $status; polling again in ${poll_secs}s"
      sleep "$poll_secs"
    done
  done

  echo "timed out after ${wait_timeout_secs}s waiting for NFS Mount Evidence run for $expected_sha" >&2
  exit 124
}

echo "dispatching NFS Mount Evidence workflow for ref $ref"
dispatch_epoch="$(python3 -c 'import time; print(time.time())' 2>/dev/null || date +%s)"
gh "${args[@]}"

if [ "$wait_for_completion" = "1" ]; then
  wait_for_evidence_run
  exit 0
fi

cat <<EOF

After the workflow starts, get the run id with:
  gh run list --workflow nfs-mount.yml --limit 10

Or wait for the exact matching run automatically with:
  make nfs-release-evidence-ci NFS_RELEASE_EVIDENCE_WAIT=1

Then release with:
  make release-ci NFS_RELEASE_EVIDENCE_RUN_ID=<run-id>
EOF
