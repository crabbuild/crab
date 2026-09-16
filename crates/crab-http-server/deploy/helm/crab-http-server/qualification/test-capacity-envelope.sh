#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname -- "$0")" && pwd)"
validator="${script_dir}/validate-capacity-envelope.jq"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/crab-capacity-contract.XXXXXX")"
trap 'rm -rf -- "$work_dir"' EXIT
valid="${work_dir}/valid.json"
invalid="${work_dir}/invalid.json"

jq --null-input '[
  {pod: "pod-a", pod_uid: "00000000-0000-0000-0000-000000000001"},
  {pod: "pod-b", pod_uid: "00000000-0000-0000-0000-000000000002"},
  {pod: "pod-c", pod_uid: "00000000-0000-0000-0000-000000000003"}
] | map({
  phase: "before-traffic",
  pod,
  pod_uid,
  envelope: {
    version: 1,
    resources: {
      memory_bytes: 8589934592,
      free_disk_bytes: 107374182400,
      available_file_descriptors: 1048576,
      job_credits: 8
    },
    admission: {
      active_cells: 5000,
      retained_bytes: 322122547,
      blocking_jobs: 8,
      dirty_jobs: 8,
      recovery_jobs: 2,
      scratch_bytes: 32212254720,
      local_disk_bytes: 64424509440,
      disk_reserve_bytes: 10737418240
    },
    reservations: {
      active_cell_page_cache_bytes: 196608,
      active_cell_native_bytes: 65536,
      active_cell_file_descriptors: 8,
      dirty_job_memory_bytes: 67108864,
      maximum_recovery_jobs: 2
    }
  }
})' > "$valid"

jq --exit-status --arg phase before-traffic --from-file "$validator" "$valid" >/dev/null

reject() {
  local name="$1"
  local filter="$2"
  jq "$filter" "$valid" > "$invalid"
  if jq --exit-status --arg phase before-traffic \
      --from-file "$validator" "$invalid" >/dev/null; then
    echo "capacity validator accepted ${name}" >&2
    exit 1
  fi
}

reject "fewer than three Pods" '.[0:2]'
reject "a reused Pod UID" '.[1].pod_uid = .[0].pod_uid'
reject "an invalid Pod UID" '.[0].pod_uid = "pod-uid"'
reject "a mismatched phase" '.[0].phase = "after-rollout"'
reject "zero active Cell capacity" '.[0].envelope.admission.active_cells = 0'
reject "dirty jobs above blocking jobs" \
  '.[0].envelope.admission.dirty_jobs = 9'
reject "recovery jobs above dirty jobs" \
  '.[0].envelope.admission.recovery_jobs = 9'
reject "scratch above local disk" \
  '.[0].envelope.admission.scratch_bytes = 70000000000'
reject "recovery above its reservation" \
  '.[0].envelope.reservations.maximum_recovery_jobs = 1'
