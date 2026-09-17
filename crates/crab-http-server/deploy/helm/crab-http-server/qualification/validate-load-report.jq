def valid_latency:
  (.p50_ms | type) == "number" and
  (.p95_ms | type) == "number" and
  (.p99_ms | type) == "number" and
  (.max_ms | type) == "number" and
  .p50_ms <= .p95_ms and .p95_ms <= .p99_ms and .p99_ms <= .max_ms and
  .over_60s == 0;

def valid_traffic($minimum):
  .responses >= .successful_responses and
  .successful_responses >= $minimum and
  .unexpected_responses == 0 and
  .server_errors == 0 and
  .transport_errors == 0 and
  .body_limit_errors == 0 and
  .responses_per_second >= 0 and
  .successful_responses_per_second >= 0 and
  .admission_rejection_percent >= 0 and
  .admission_rejection_percent <= 5 and
  (.latency | valid_latency) and
  (.successful_latency | valid_latency);

def target_repository:
  .path | capture("^/api/repos/[^/]+/(?<repository>[^/]+)/statuses/[0-9a-f]{40}$").repository;

type == "array" and
length >= 3 and
([.[].pod_uid] | length == (unique | length)) and
all(.[];
  .pod != "" and
  (.pod_uid | test("^[0-9a-f-]{36}$")) and
  (.report as $report |
    $report.schema_version == 2 and
    $report.authority == $authority and
    $report.aggregate_requests_per_second == 1000 and
    $report.configured_duration_ms == 60000 and
    $report.minimum_success_percent == 95 and
    $report.minimum_successful_responses == 57000 and
    $report.target_rate_qualified == true and
    $report.qualified == true and
    $report.health_before.status == 200 and
    $report.health_after.status == 200 and
    ($report.targets | length) == 64 and
    ($report.targets | map(target_repository) | unique | length) == 8 and
    ($report.targets | map(target_repository) | sort | group_by(.) | all(length == 8)) and
    all($report.targets[];
      .method == "POST" and
      (.path | test("^/api/repos/[^/]+/[^/]+/statuses/[0-9a-f]{40}$"))) and
    ($report.aggregate | valid_traffic($report.minimum_successful_responses))))
