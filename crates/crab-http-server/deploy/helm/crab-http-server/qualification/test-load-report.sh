#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname -- "$0")" && pwd)"
validator="${script_dir}/validate-load-report.jq"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/crab-load-contract.XXXXXX")"
trap 'rm -rf -- "$work_dir"' EXIT
valid="${work_dir}/valid.json"
invalid="${work_dir}/invalid.json"

jq --null-input '
  def latency: {p50_ms: 1, p95_ms: 2, p99_ms: 3, max_ms: 4, over_60s: 0};
  def traffic: {
    elapsed_ms: 60000,
    responses: 60000,
    successful_responses: 59000,
    admission_rejections: 1000,
    unexpected_responses: 0,
    server_errors: 0,
    transport_errors: 0,
    body_limit_errors: 0,
    response_bytes: 1024,
    responses_per_second: 1000,
    successful_responses_per_second: 983.3,
    admission_rejection_percent: 1.67,
    latency: latency,
    successful_latency: latency
  };
  def report: {
    schema_version: 2,
    started_at_ms: 1,
    base_url: "http://127.0.0.1:28788/",
    authority: "git.example.com",
    configured_duration_ms: 60000,
    warmup_ms: 5000,
    aggregate_requests_per_second: 1000,
    minimum_success_percent: 95,
    minimum_successful_responses: 57000,
    target_rate_qualified: true,
    max_response_bytes: 67108864,
    health_before: {status: 200, latency_ms: 1, response_bytes: 2},
    health_after: {status: 200, latency_ms: 1, response_bytes: 2},
    targets: [range(0; 8) as $cell | range(0; 8) as $target | {
      name: ("status" + (($cell * 8 + $target) | tostring)),
      method: "POST",
      path: ("/api/repos/team/repository-load-" + (($cell + 1) | tostring) +
        "/statuses/" + ("a" * 40)),
      concurrency: 2
    }],
    aggregate: traffic,
    qualified: true
  };
  [range(1; 4) | {
    pod: ("pod-" + tostring),
    pod_uid: ("00000000-0000-0000-0000-00000000000" + tostring),
    report: report
  }]
' > "$valid"

jq --exit-status --arg authority git.example.com --from-file "$validator" "$valid" >/dev/null

reject() {
  local name="$1"
  local filter="$2"
  jq "$filter" "$valid" > "$invalid"
  if jq --exit-status --arg authority git.example.com \
      --from-file "$validator" "$invalid" >/dev/null; then
    echo "load report validator accepted ${name}" >&2
    exit 1
  fi
}

reject "fewer than three Pods" '.[0:2]'
reject "a duplicate Pod UID" '.[1].pod_uid = .[0].pod_uid'
reject "the wrong aggregate rate" '.[0].report.aggregate_requests_per_second = 999'
reject "a short duration" '.[0].report.configured_duration_ms = 59000'
reject "too few successful responses" '.[0].report.aggregate.successful_responses = 56999'
reject "a false target-rate proof" '.[0].report.target_rate_qualified = false'
reject "single-cell targets" '.[0].report.targets |= map(.path = "/api/repos/team/repository/statuses/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")'
reject "an admission percentage above five" '.[0].report.aggregate.admission_rejection_percent = 5.01'
reject "one server error" '.[0].report.aggregate.server_errors = 1'
reject "an invalid latency order" '.[0].report.aggregate.successful_latency.p95_ms = 0'
reject "a read-only target" '.[0].report.targets[0].method = "GET"'
