#!/usr/bin/env bash
set -euo pipefail
set +x

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate_dir="$(cd "${script_dir}/.." && pwd)"
repo_root="$(cd "${crate_dir}/../.." && pwd)"
compose_file="${crate_dir}/deploy/compose.yaml"
cluster_file="${crate_dir}/deploy/compose.cluster.yaml"
project="${CRAB_HTTP_CLUSTER_PROJECT:-crab-http-cluster-qualification-$$}"

if [[ ! "$project" =~ ^crab-http-cluster-qualification-[A-Za-z0-9_-]+$ ]]; then
  echo "CRAB_HTTP_CLUSTER_PROJECT must be a unique crab-http-cluster-qualification-* name." >&2
  exit 2
fi

for dependency in curl docker jq; do
  command -v "$dependency" >/dev/null || {
    echo "Missing required command: ${dependency}" >&2
    exit 2
  }
done
docker compose version >/dev/null
if [ -n "$(docker container ls --all --quiet \
  --filter "label=com.docker.compose.project=${project}")" ] ||
  [ -n "$(docker volume ls --quiet \
    --filter "label=com.docker.compose.project=${project}")" ] ||
  [ -n "$(docker network ls --quiet \
    --filter "label=com.docker.compose.project=${project}")" ]; then
  echo "Refusing to reuse existing Compose project ${project}." >&2
  exit 2
fi

export CRAB_HTTP_SERVER_PORT="${CRAB_HTTP_SERVER_PORT:-18878}"
export CRAB_HTTP_CLUSTER_PORT="${CRAB_HTTP_CLUSTER_PORT:-18880}"
export CRAB_HTTP_NODE_A_PORT="${CRAB_HTTP_NODE_A_PORT:-18881}"
export CRAB_HTTP_NODE_B_PORT="${CRAB_HTTP_NODE_B_PORT:-18882}"
export CRAB_HTTP_NODE_C_PORT="${CRAB_HTTP_NODE_C_PORT:-18883}"
export CRAB_HTTP_NODE_D_PORT="${CRAB_HTTP_NODE_D_PORT:-18884}"

compose=(
  docker compose
  --project-name "$project"
  --file "$compose_file"
  --file "$cluster_file"
)
cluster_origin="http://127.0.0.1:${CRAB_HTTP_CLUSTER_PORT}"
node_a_origin="http://127.0.0.1:${CRAB_HTTP_NODE_A_PORT}"
node_b_origin="http://127.0.0.1:${CRAB_HTTP_NODE_B_PORT}"
node_c_origin="http://127.0.0.1:${CRAB_HTTP_NODE_C_PORT}"
node_d_origin="http://127.0.0.1:${CRAB_HTTP_NODE_D_PORT}"
repository_path="api/repos/demo/hello"
failed=false
source_revision="$(git -C "$repo_root" rev-parse --verify HEAD)"
qualified_image_ref="${CRAB_HTTP_QUALIFIED_IMAGE_REF:-source-only}"
qualified_image_digest="${CRAB_HTTP_QUALIFIED_IMAGE_DIGEST:-$(docker image inspect \
  "${CRAB_HTTP_SERVER_IMAGE:-crab-http-server:local}" --format '{{.Id}}')}"
if [[ ! "$source_revision" =~ ^[0-9a-f]{40}$ ]] ||
  [[ ! "$qualified_image_digest" =~ ^sha256:[0-9a-f]{64}$ ]]; then
  echo "qualification requires a Git source revision and image digest." >&2
  exit 2
fi

unix_millis() {
  local seconds fractional
  seconds="$(date +%s)"
  fractional="$(date +%N 2>/dev/null || true)"
  if [[ "$fractional" =~ ^[0-9]{9}$ ]]; then
    printf '%s\n' "$((seconds * 1000 + 10#${fractional:0:3}))"
  else
    printf '%s\n' "$((seconds * 1000))"
  fi
}

cleanup() {
  result=$?
  "${compose[@]}" unpause server >/dev/null 2>&1 || true
  "${compose[@]}" run --rm --no-deps --entrypoint aws bucket-init \
    --endpoint-url http://rustfs:9000 s3api delete-bucket-policy \
    --bucket crab-http-server >/dev/null 2>&1 || true
  if [ "$result" -ne 0 ]; then
    failed=true
    "${compose[@]}" ps --all || true
    "${compose[@]}" logs --no-color || true
  fi
  "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  if $failed; then
    echo "Compose cluster qualification failed." >&2
  fi
  exit "$result"
}
trap cleanup EXIT

wait_for_healthy() {
  local service="$1"
  local container
  for _ in $(seq 1 90); do
    container="$("${compose[@]}" ps --quiet "$service")"
    if [ -n "$container" ] &&
      [ "$(docker inspect --format '{{.State.Health.Status}}' "$container")" = healthy ]; then
      return 0
    fi
    sleep 1
  done
  echo "${service} did not become healthy." >&2
  return 1
}

assert_json_eventually() {
  local origin="$1"
  local path="$2"
  local filter="$3"
  local message="$4"
  local candidate
  for _ in $(seq 1 45); do
    candidate="$(curl --fail-with-body --silent --show-error --max-time 10 \
      "${origin}/${path}" 2>/dev/null || true)"
    if jq --exit-status "$filter" <<<"$candidate" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  echo "$message" >&2
  return 1
}

metric_value() {
  local metrics="$1"
  local name="$2"
  local value
  value="$(awk -v name="$name" '$1 == name { print $2; exit }' <<<"$metrics")"
  printf '%s\n' "${value:-0}"
}

metric_counter() {
  local metrics="$1"
  local kind="$2"
  metric_value "$metrics" "crab_cell_node_log_recovery_work_total{kind=\"${kind}\"}"
}

metric_phase_count() {
  local metrics="$1"
  local phase="$2"
  metric_value "$metrics" "crab_cell_node_log_recovery_phase_seconds_count{phase=\"${phase}\"}"
}

metric_phase_sum() {
  local metrics="$1"
  local phase="$2"
  metric_value "$metrics" "crab_cell_node_log_recovery_phase_seconds_sum{phase=\"${phase}\"}"
}

counter_delta() {
  awk -v before="$1" -v after="$2" \
    'BEGIN { if (after < before) exit 1; printf "%.0f\n", after - before }'
}

duration_delta_ms() {
  awk -v before="$1" -v after="$2" \
    'BEGIN { if (after < before) exit 1; printf "%.0f\n", (after - before) * 1000 }'
}

recovery_work_evidence() {
  local before="$1"
  local after="$2"
  local -a args=(--argjson candidate_count "$(counter_delta \
    "$(metric_counter "$before" candidate_count)" \
    "$(metric_counter "$after" candidate_count)")")
  for kind in \
    affected_cells catalog_shards catalog_pages control_reads follower_pages \
    follower_frames follower_bytes peer_requests bundle_bytes object_reads object_writes; do
    args+=(--argjson "$kind" "$(counter_delta \
      "$(metric_counter "$before" "$kind")" \
      "$(metric_counter "$after" "$kind")")")
  done
  for phase in claim witness scope_validation pin_attach seal; do
    args+=(--argjson "${phase}_count" "$(counter_delta \
      "$(metric_phase_count "$before" "$phase")" \
      "$(metric_phase_count "$after" "$phase")")")
    args+=(--argjson "${phase}_duration_ms" "$(duration_delta_ms \
      "$(metric_phase_sum "$before" "$phase")" \
      "$(metric_phase_sum "$after" "$phase")")")
  done
  jq -n "${args[@]}" '{
    candidate_count: $candidate_count,
    affected_cells: $affected_cells,
    catalog_shards: $catalog_shards,
    catalog_pages: $catalog_pages,
    control_reads: $control_reads,
    follower_pages: $follower_pages,
    follower_frames: $follower_frames,
    follower_bytes: $follower_bytes,
    peer_requests: $peer_requests,
    bundle_bytes: $bundle_bytes,
    object_reads: $object_reads,
    object_writes: $object_writes,
    phases: {
      claim: {count: $claim_count, duration_ms: $claim_duration_ms},
      witness: {count: $witness_count, duration_ms: $witness_duration_ms},
      scope_validation: {count: $scope_validation_count, duration_ms: $scope_validation_duration_ms},
      pin_attach: {count: $pin_attach_count, duration_ms: $pin_attach_duration_ms},
      seal: {count: $seal_count, duration_ms: $seal_duration_ms}
    }
  }'
}

up_mode=(--no-build)
if [ "${CRAB_HTTP_CLUSTER_BUILD:-true}" = true ]; then
  up_mode=(--build)
fi
"${compose[@]}" config --quiet
"${compose[@]}" up --detach "${up_mode[@]}" --wait --wait-timeout 180

# The cluster overlay starts B only after C is healthy. Together with B's
# implicit network-namespace dependency on A, its first immutable epoch can
# recruit both followers without replacing its boot session.

capacity_a="$("${compose[@]}" exec -T server crab-http-server \
  --config /etc/crab/server.toml cells capacity --json --live)"
capacity_b="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells capacity --json --live)"
capacity_c="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells capacity --json --live)"
capacity_d="$("${compose[@]}" exec -T server-d crab-http-server \
  --config /etc/crab/server.toml cells capacity --json --live)"
for capacity in "$capacity_a" "$capacity_b" "$capacity_c" "$capacity_d"; do
  jq --exit-status \
    '.version == 1 and .resources.memory_bytes > 0 and
     .resources.free_disk_bytes > 0 and .admission.active_cells > 0' \
    <<<"$capacity" >/dev/null
done

disk_probe_tolerance_bytes=$((1024 * 1024))
disk_probe_a_kib="$("${compose[@]}" exec -T server sh -ec \
  'df -P -k /var/lib/crab/cells | tail -n 1 | sed -e "s/^ *//" | tr -s " " | cut -d " " -f4')"
disk_probe_b_kib="$("${compose[@]}" exec -T server-b sh -ec \
  'df -P -k /var/lib/crab/cells | tail -n 1 | sed -e "s/^ *//" | tr -s " " | cut -d " " -f4')"
disk_probe_c_kib="$("${compose[@]}" exec -T server-c sh -ec \
  'df -P -k /var/lib/crab/cells | tail -n 1 | sed -e "s/^ *//" | tr -s " " | cut -d " " -f4')"
disk_probe_d_kib="$("${compose[@]}" exec -T server-d sh -ec \
  'df -P -k /var/lib/crab/cells | tail -n 1 | sed -e "s/^ *//" | tr -s " " | cut -d " " -f4')"
[[ "$disk_probe_a_kib" =~ ^[0-9]+$ ]]
[[ "$disk_probe_b_kib" =~ ^[0-9]+$ ]]
[[ "$disk_probe_c_kib" =~ ^[0-9]+$ ]]
[[ "$disk_probe_d_kib" =~ ^[0-9]+$ ]]
disk_probe_a=$((disk_probe_a_kib * 1024))
disk_probe_b=$((disk_probe_b_kib * 1024))
disk_probe_c=$((disk_probe_c_kib * 1024))
disk_probe_d=$((disk_probe_d_kib * 1024))
for pair in \
  "$capacity_a|$disk_probe_a" \
  "$capacity_b|$disk_probe_b" \
  "$capacity_c|$disk_probe_c" \
  "$capacity_d|$disk_probe_d"; do
  capacity="${pair%%|*}"
  observed_disk="${pair#*|}"
  expected_disk="$(jq -r '.resources.free_disk_bytes' <<<"$capacity")"
  [[ "$observed_disk" =~ ^[0-9]+$ ]]
  delta="$(jq -n \
    --argjson expected "$expected_disk" \
    --argjson observed "$observed_disk" \
    '$expected - $observed | if . < 0 then -. else . end')"
  test "$delta" -le "$disk_probe_tolerance_bytes"
done

metrics_a="$("${compose[@]}" exec -T server crab-http-server \
  --config /etc/crab/server.toml cells metrics)"
metrics_b="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells metrics)"
metrics_c="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells metrics)"
metrics_d="$("${compose[@]}" exec -T server-d crab-http-server \
  --config /etc/crab/server.toml cells metrics)"
for pair in \
  "$capacity_a|$metrics_a" \
  "$capacity_b|$metrics_b" \
  "$capacity_c|$metrics_c" \
  "$capacity_d|$metrics_d"; do
  capacity="${pair%%|*}"
  metrics="${pair#*|}"
  expected_disk="$(jq -r '.admission.local_disk_bytes' <<<"$capacity")"
  expected_cells="$(jq -r '.admission.active_cells' <<<"$capacity")"
  observed_disk="$(awk '$1 == "crab_http_server_cell_runtime_local_disk_capacity_bytes" { print $2; exit }' <<<"$metrics")"
  observed_cells="$(awk '$1 == "crab_http_server_cell_runtime_active_cell_capacity" { print $2; exit }' <<<"$metrics")"
  test -n "$observed_disk" && test -n "$observed_cells"
  test "$observed_disk" = "$expected_disk"
  test "$observed_cells" = "$expected_cells"
done

node_session() {
  local service="$1"
  # The single-quoted script must expand path inside the container, not locally.
  # shellcheck disable=SC2016
  "${compose[@]}" exec -T "$service" sh -ec '
    for path in /var/lib/crab/cells/sessions/*; do
      if [ -d "$path" ]; then
        printf "%s\n" "${path##*/}"
        exit 0
      fi
    done
    exit 1
  '
}

session_a="$(node_session server)"
session_b="$(node_session server-b)"
session_c="$(node_session server-c)"
session_d="$(node_session server-d)"
[[ "$session_a" =~ ^[0-9a-f]{32}$ ]]
[[ "$session_b" =~ ^[0-9a-f]{32}$ ]]
[[ "$session_c" =~ ^[0-9a-f]{32}$ ]]
[[ "$session_d" =~ ^[0-9a-f]{32}$ ]]
node_a="$("${compose[@]}" exec -T server crab-http-server \
  --config /etc/crab/server.toml cells node --session "$session_a" --json)"
node_b="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells node --session "$session_b" --json)"
node_c="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells node --session "$session_c" --json)"
node_d="$("${compose[@]}" exec -T server-d crab-http-server \
  --config /etc/crab/server.toml cells node --session "$session_d" --json)"

node_id_for_session() {
  local expected_session="$1"
  local node_json
  for node_json in "$node_a" "$node_b" "$node_c" "$node_d"; do
    if [ "$(jq -r '.session' <<<"$node_json")" = "$expected_session" ]; then
      jq -r '.advertisement.node' <<<"$node_json"
      return 0
    fi
  done
  return 1
}

node_c_id="$(jq -r '.advertisement.node' <<<"$node_c")"
[[ "$node_c_id" =~ ^[0-9a-f]{32}$ ]]

assert_placement_parity() {
  local capacity="$1"
  local metrics="$2"
  local node="$3"
  local observed_active
  observed_active="$(awk '$1 == "crab_http_server_cell_runtime_active_cells" { print $2; exit }' <<<"$metrics")"
  test -n "$observed_active"
  jq --exit-status \
    --argjson expected_memory "$(jq -r '.resources.memory_bytes' <<<"$capacity")" \
    --argjson expected_disk "$(jq -r '.admission.local_disk_bytes' <<<"$capacity")" \
    --argjson expected_cells "$(jq -r '.admission.active_cells' <<<"$capacity")" \
    --argjson observed_active "$observed_active" \
    '.live == true and .advertisement.placement != null and
     .advertisement.placement.memory_capacity_bytes == $expected_memory and
     .advertisement.placement.disk_capacity_bytes == $expected_disk and
     .advertisement.placement.max_active_cells == $expected_cells and
     .advertisement.placement.active_cells == $observed_active' \
    <<<"$node" >/dev/null
}

assert_placement_parity "$capacity_a" "$metrics_a" "$node_a"
assert_placement_parity "$capacity_b" "$metrics_b" "$node_b"
assert_placement_parity "$capacity_c" "$metrics_c" "$node_c"
assert_placement_parity "$capacity_d" "$metrics_d" "$node_d"

create_response=""
for _ in $(seq 1 45); do
  candidate="$(curl --fail-with-body --silent --show-error --max-time 10 \
    --request POST \
    --header 'content-type: application/json' \
    --data '{"request_id":"00000000-0000-4000-8000-000000000101","title":"Owner loss qualification","body":"Created through node B"}' \
    "${node_b_origin}/${repository_path}/issues" 2>/dev/null || true)"
  if jq --exit-status \
    '.number == 1 and .title == "Owner loss qualification"' \
    <<<"$candidate" >/dev/null 2>&1; then
    create_response="$candidate"
    break
  fi
  sleep 1
done
if [ -z "$create_response" ]; then
  echo "Node B did not activate and accept the initial idempotent write." >&2
  exit 1
fi

control_before="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
session_before="$(jq --raw-output '.owner.session' <<<"$control_before")"
epoch_before="$(jq --raw-output '.epoch' <<<"$control_before")"
sequence_before="$(jq --raw-output '.root.commit_sequence' <<<"$control_before")"
root_before_state="$(jq --compact-output '.root' <<<"$control_before")"
jq --exit-status \
  '.state == "serving" and .owner.endpoint == "https://localhost:8889/" and
   .owner_lease.state == "live" and
   .owner_lease.expires_at_ms > .owner_lease.observed_at_ms and
   .recovery == null and
   .root.commit_sequence >= 1' <<<"$control_before" >/dev/null

log_ready=false
for _ in $(seq 1 45); do
  node_before="$("${compose[@]}" exec -T server-c crab-http-server \
    --config /etc/crab/server.toml cells node \
    --session "$session_before" --json)"
  if jq --exit-status \
    '.live == true and .advertisement.log.state == "open" and
     (.advertisement.log.member_nodes | length) == 2' \
    <<<"$node_before" >/dev/null; then
    log_ready=true
    break
  fi
  sleep 1
done
if ! $log_ready; then
  echo "Node B did not activate a two-follower durability log." >&2
  exit 1
fi

for origin in "$node_a_origin" "$node_c_origin" "$cluster_origin"; do
  curl --fail --silent --show-error \
    "${origin}/${repository_path}/issues?state=all" \
    | jq --exit-status \
      '.items | length == 1 and .[0].title == "Owner loss qualification"' \
      >/dev/null
done

deny_cell_objects='{"Version":"2012-10-17","Statement":[{"Sid":"DenyCellImmutableObjectWrites","Effect":"Deny","Principal":"*","Action":"s3:PutObject","Resource":"arn:aws:s3:::crab-http-server/repositories/cells/v1/apps/*/cells/*/inc/*/objects/*"}]}'
"${compose[@]}" run --rm --no-deps --entrypoint aws bucket-init \
  --endpoint-url http://rustfs:9000 s3api put-bucket-policy \
  --bucket crab-http-server --policy "$deny_cell_objects" >/dev/null
fleet_only_response="$(curl --fail-with-body --silent --show-error \
  --max-time 15 \
  --request POST \
  --header 'content-type: application/json' \
  --data '{"request_id":"00000000-0000-4000-8000-000000000102","name":"follower-only","color":"c2410c","description":"Acknowledged by follower fsync"}' \
  "${node_b_origin}/${repository_path}/labels")"
jq --exit-status \
  '.id == 1 and .name == "follower-only"' \
  <<<"$fleet_only_response" >/dev/null
control_fleet_only="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
jq --exit-status --argjson sequence_before "$sequence_before" \
  '.root.commit_sequence == $sequence_before' <<<"$control_fleet_only" >/dev/null
node_fleet_only="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells node \
  --session "$session_before" --json)"
jq --exit-status \
  '.live == true and .advertisement.log.state == "open" and
   .advertisement.log.active == true and
   (.advertisement.log.member_nodes | length) == 2' \
  <<<"$node_fleet_only" >/dev/null
metrics_owner_fleet_only="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells metrics)"
metrics_follower_fleet_only="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells metrics)"
owner_uncovered_bytes="$(awk \
  '$1 == "crab_cell_node_log_uncovered_bytes" { print $2 }' \
  <<<"$metrics_owner_fleet_only")"
follower_retained_bytes="$(awk \
  '$1 == "crab_cell_follower_retained_bytes" { print $2 }' \
  <<<"$metrics_follower_fleet_only")"
awk '$1 == "crab_cell_node_log_uncovered_bytes" && $2 + 0 > 0 { found = 1 }
     END { exit !found }' <<<"$metrics_owner_fleet_only"
awk '$1 == "crab_cell_follower_retained_bytes" && $2 + 0 > 0 { found = 1 }
     END { exit !found }' <<<"$metrics_follower_fleet_only"

# Keep node C as the deterministic surviving follower: node A remains in the
# log, but its signed advertisement must expire before the owner is killed.
"${compose[@]}" pause server >/dev/null
a_advertisement_expired=false
for _ in $(seq 1 45); do
  node_a_status="$("${compose[@]}" exec -T server-c crab-http-server \
    --config /etc/crab/server.toml cells node \
    --session "$session_a" --json 2>/dev/null || true)"
  if jq --exit-status '.live == false' <<<"$node_a_status" >/dev/null 2>&1; then
    a_advertisement_expired=true
    break
  fi
  sleep 1
done
if ! $a_advertisement_expired; then
  echo "Paused node A did not leave the live advertisement set." >&2
  exit 1
fi

owner_killed_ms="$(unix_millis)"
"${compose[@]}" kill --signal KILL server-b >/dev/null
"${compose[@]}" rm --force --stop server-b >/dev/null
"${compose[@]}" run --rm --no-deps --entrypoint aws bucket-init \
  --endpoint-url http://rustfs:9000 s3api delete-bucket-policy \
  --bucket crab-http-server >/dev/null
advertisement_expired=false
for _ in $(seq 1 45); do
  node_status="$("${compose[@]}" exec -T server-c crab-http-server \
    --config /etc/crab/server.toml cells node \
    --session "$session_before" --json)"
  if jq --exit-status '.live == false' <<<"$node_status" >/dev/null; then
    advertisement_expired=true
    advertisement_expired_ms="$(unix_millis)"
    break
  fi
  sleep 1
done
if ! $advertisement_expired; then
  echo "The killed owner's signed advertisement did not expire." >&2
  exit 1
fi

restored=""
restored_labels=""
for _ in $(seq 1 45); do
  candidate="$(curl --fail-with-body --silent --show-error --max-time 10 \
    "${node_c_origin}/${repository_path}/issues?state=all" || true)"
  label_candidate="$(curl --fail-with-body --silent --show-error --max-time 10 \
    "${node_c_origin}/${repository_path}/labels" || true)"
  if jq --exit-status \
    '.items | length == 1 and .[0].title == "Owner loss qualification"' \
    <<<"$candidate" >/dev/null 2>&1 \
    && jq --exit-status \
      '.items | length == 1 and .[0].name == "follower-only"' \
      <<<"$label_candidate" >/dev/null 2>&1; then
    restored="$candidate"
    restored_labels="$label_candidate"
    break
  fi
  sleep 1
done
if [ -z "$restored" ]; then
  echo "Node C did not recover the follower-proven commit." >&2
  exit 1
fi

control_after=""
for _ in $(seq 1 45); do
  candidate="$("${compose[@]}" exec -T server-c crab-http-server \
    --config /etc/crab/server.toml cells status --owner demo --name hello \
    2>/dev/null || true)"
  if jq --exit-status \
    --arg session_before "$session_before" \
    --argjson epoch_before "$epoch_before" \
    --argjson sequence_before "$sequence_before" \
    '.state == "serving" and .owner.endpoint == "https://localhost:8989/" and
     .owner.session != $session_before and .epoch > $epoch_before and
     .owner_lease.state == "live" and
     .owner_lease.expires_at_ms > .owner_lease.observed_at_ms and
     .recovery == null and
     .root.commit_sequence > $sequence_before' <<<"$candidate" >/dev/null 2>&1; then
    control_after="$candidate"
    break
  fi
  sleep 1
done
if [ -z "$control_after" ]; then
  echo "Node C did not publish a serving status after owner takeover." >&2
  exit 1
fi
recovery_sealed_ms="$(unix_millis)"
first_served_check="$(curl --fail-with-body --silent --show-error \
  "${node_c_origin}/${repository_path}/issues?state=all")"
jq --exit-status \
  '.items | length == 1 and .[0].title == "Owner loss qualification"' \
  <<<"$first_served_check" >/dev/null
first_served_ms="$(unix_millis)"
session_after="$(jq --raw-output '.owner.session' <<<"$control_after")"
epoch_after="$(jq --raw-output '.epoch' <<<"$control_after")"
root_after_state="$(jq --compact-output '.root' <<<"$control_after")"
metrics_first=""
for _ in $(seq 1 45); do
  candidate_metrics="$("${compose[@]}" exec -T server-c crab-http-server \
    --config /etc/crab/server.toml cells metrics)"
  if [ "$(metric_counter "$candidate_metrics" candidate_count)" -gt \
    "$(metric_counter "$metrics_c" candidate_count)" ]; then
    metrics_first="$candidate_metrics"
    break
  fi
  sleep 1
done
if [ -z "$metrics_first" ]; then
  echo "Node C did not export recovery work after the first owner loss." >&2
  exit 1
fi
work_first="$(recovery_work_evidence "$metrics_c" "$metrics_first")"

for _ in $(seq 1 6); do
  assert_json_eventually \
    "$cluster_origin" \
    "${repository_path}/issues?state=all" \
    '.items | length == 1 and .[0].title == "Owner loss qualification"' \
    "Cluster proxy did not expose the recovered issue."
  assert_json_eventually \
    "$cluster_origin" \
    "${repository_path}/labels" \
    '.items | length == 1 and .[0].name == "follower-only"' \
    "Cluster proxy did not expose the recovered label."
done

continued="$(curl --fail-with-body --silent --show-error \
  --request POST \
  --header 'content-type: application/json' \
  --data '{"request_id":"00000000-0000-4000-8000-000000000103","title":"Recovered owner","body":"Published by node C"}' \
  "${node_c_origin}/${repository_path}/issues")"
jq --exit-status '.number == 2 and .title == "Recovered owner"' \
  <<<"$continued" >/dev/null

control_continued="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
sequence_continued="$(jq --raw-output '.root.commit_sequence' <<<"$control_continued")"
root_continued_state="$(jq --compact-output '.root' <<<"$control_continued")"
jq --exit-status \
  --arg session_after "$session_after" \
  --argjson sequence_before "$sequence_before" \
  '.owner.session == $session_after and
   .owner_lease.state == "live" and
   .owner_lease.expires_at_ms > .owner_lease.observed_at_ms and
   .recovery == null and
   .root.commit_sequence > $sequence_before' \
  <<<"$control_continued" >/dev/null

"${compose[@]}" unpause server >/dev/null
"${compose[@]}" up --detach --no-build server server-b >/dev/null
wait_for_healthy server-b
assert_json_eventually \
  "$node_b_origin" \
  "${repository_path}/issues?state=all" \
  '.items | length == 2 and .[0].title == "Recovered owner" and
   .[1].title == "Owner loss qualification"' \
  "Node B did not expose the recovered issues after rejoining."
assert_json_eventually \
  "$node_b_origin" \
  "${repository_path}/labels" \
  '.items | length == 1 and .[0].name == "follower-only"' \
  "Node B did not expose the recovered label after rejoining."
control_after_rejoin=""
for _ in $(seq 1 45); do
  candidate="$("${compose[@]}" exec -T server-b crab-http-server \
    --config /etc/crab/server.toml cells status --owner demo --name hello \
    2>/dev/null || true)"
  # The owner may publish a later monotonic root while the follower rejoins.
  if jq --exit-status \
    --arg session_after "$session_after" \
    --argjson epoch_after "$epoch_after" \
    --argjson sequence_continued "$sequence_continued" \
    '.state == "serving" and .owner.session == $session_after and
     .owner.endpoint == "https://localhost:8989/" and .epoch == $epoch_after and
     .owner_lease.state == "live" and
     .owner_lease.expires_at_ms > .owner_lease.observed_at_ms and
     .recovery == null and
     .root.commit_sequence >= $sequence_continued' <<<"$candidate" >/dev/null 2>&1; then
    control_after_rejoin="$candidate"
    break
  fi
  sleep 1
done
if [ -z "$control_after_rejoin" ]; then
  echo "Node B did not publish the recovered serving status after rejoining." >&2
  exit 1
fi

node_before_follower_loss="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells node \
  --session "$session_after" --json)"
node_b_id="$("${compose[@]}" exec -T server-b /bin/sh -ec \
  'cat /var/lib/crab/cells/node-id')"
if [[ ! "$node_b_id" =~ ^[0-9a-f]{32}$ ]]; then
  echo "Rejoined node B did not expose a canonical local node identity." >&2
  exit 1
fi
metrics_second_before="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells metrics)"
log_epoch_before_follower_loss="$(jq --raw-output \
  '.advertisement.log.epoch' <<<"$node_before_follower_loss")"
jq --exit-status \
  --arg node_b "$node_b_id" \
  '.live == true and .advertisement.log.state == "open" and
   any(.advertisement.log.member_nodes[]; . != $node_b)' \
  <<<"$node_before_follower_loss" >/dev/null

# Pausing nodes A and D keeps the shared Compose network namespace alive while
# the old followers, heartbeats, and follower endpoints are unavailable.
"${compose[@]}" pause server >/dev/null
"${compose[@]}" pause server-d >/dev/null
sleep 12
after_follower_loss="$(curl --fail-with-body --silent --show-error \
  --request POST \
  --header 'content-type: application/json' \
  --data '{"request_id":"00000000-0000-4000-8000-000000000104","title":"Follower lost","body":"Published through object coverage before re-enrollment"}' \
  "${node_c_origin}/${repository_path}/issues")"
jq --exit-status '.number == 3 and .title == "Follower lost"' \
  <<<"$after_follower_loss" >/dev/null

reenrolled=false
for _ in $(seq 1 75); do
  node_after_follower_loss="$("${compose[@]}" exec -T server-c crab-http-server \
    --config /etc/crab/server.toml cells node \
    --session "$session_after" --json)"
  if jq --exit-status \
    --argjson epoch "$log_epoch_before_follower_loss" \
    --arg node_b "$node_b_id" \
    '.live == true and .advertisement.log.state == "open" and
     .advertisement.log.epoch > $epoch and
     .advertisement.log.member_nodes == [$node_b]' \
    <<<"$node_after_follower_loss" >/dev/null; then
    reenrolled=true
    break
  fi
  sleep 1
done
if ! $reenrolled; then
  echo "Node C did not replace the expired follower." >&2
  exit 1
fi

"${compose[@]}" run --rm --no-deps --entrypoint aws bucket-init \
  --endpoint-url http://rustfs:9000 s3api put-bucket-policy \
  --bucket crab-http-server --policy "$deny_cell_objects" >/dev/null
replacement_fleet_response="$(curl --fail-with-body --silent --show-error \
  --max-time 15 \
  --request POST \
  --header 'content-type: application/json' \
  --data '{"request_id":"00000000-0000-4000-8000-000000000105","name":"replacement-follower-only","color":"0369a1","description":"Acknowledged by the replacement follower"}' \
  "${node_c_origin}/${repository_path}/labels")"
jq --exit-status \
  '.id == 2 and .name == "replacement-follower-only"' \
  <<<"$replacement_fleet_response" >/dev/null
control_before_second_loss="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
root_before_second_loss="$(jq --compact-output '.root' <<<"$control_before_second_loss")"
node_before_second_loss="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells node --session "$session_after" --json)"
jq --exit-status \
  --arg node_b "$node_b_id" \
  '.live == true and .advertisement.log.state == "open" and
   .advertisement.log.member_nodes == [$node_b]' \
  <<<"$node_before_second_loss" >/dev/null

second_owner_killed_ms="$(unix_millis)"
"${compose[@]}" kill --signal KILL server-c >/dev/null
"${compose[@]}" rm --force --stop server-c >/dev/null
"${compose[@]}" run --rm --no-deps --entrypoint aws bucket-init \
  --endpoint-url http://rustfs:9000 s3api delete-bucket-policy \
  --bucket crab-http-server >/dev/null

second_advertisement_expired=false
for _ in $(seq 1 60); do
  node_status="$("${compose[@]}" exec -T server-b crab-http-server \
    --config /etc/crab/server.toml cells node \
    --session "$session_after" --json 2>/dev/null || true)"
  if jq --exit-status '.live == false' <<<"$node_status" >/dev/null 2>&1; then
    second_advertisement_expired=true
    second_advertisement_expired_ms="$(unix_millis)"
    break
  fi
  sleep 1
done
if ! $second_advertisement_expired; then
  echo "The second killed owner's signed advertisement did not expire." >&2
  exit 1
fi

second_restored_labels=""
for _ in $(seq 1 60); do
  candidate="$(curl --fail-with-body --silent --show-error --max-time 10 \
    "${node_b_origin}/${repository_path}/labels" || true)"
  if jq --exit-status \
    '.items | (length == 2 and
     (map(.name) | sort == ["follower-only", "replacement-follower-only"]))' \
    <<<"$candidate" >/dev/null 2>&1; then
    second_restored_labels="$candidate"
    break
  fi
  sleep 1
done
if [ -z "$second_restored_labels" ]; then
  echo "Node B did not recover the replacement-follower commit." >&2
  exit 1
fi
assert_json_eventually \
  "$node_b_origin" \
  "${repository_path}/issues?state=all" \
  '.items | length == 3' \
  "Node B did not expose all recovered issues after the second owner loss."
control_after_second_loss=""
for _ in $(seq 1 45); do
  candidate="$("${compose[@]}" exec -T server-b crab-http-server \
    --config /etc/crab/server.toml cells status --owner demo --name hello \
    2>/dev/null || true)"
  if jq --exit-status \
    --arg session_after "$session_after" \
    --argjson epoch_after "$epoch_after" \
    '.state == "serving" and .owner.session != $session_after and
     .epoch > $epoch_after and .owner_lease.state == "live" and
     .recovery == null' <<<"$candidate" >/dev/null 2>&1; then
    control_after_second_loss="$candidate"
    break
  fi
  sleep 1
done
if [ -z "$control_after_second_loss" ]; then
  echo "Node B did not publish a serving status after the second owner loss." >&2
  exit 1
fi
second_recovery_sealed_ms="$(unix_millis)"
second_first_served_check="$(curl --fail-with-body --silent --show-error \
  "${node_b_origin}/${repository_path}/issues?state=all")"
jq --exit-status '.items | length == 3' <<<"$second_first_served_check" >/dev/null
second_first_served_ms="$(unix_millis)"
session_after_second_loss="$(jq --raw-output '.owner.session' \
  <<<"$control_after_second_loss")"
epoch_after_second_loss="$(jq --raw-output '.epoch' \
  <<<"$control_after_second_loss")"
root_after_second_loss="$(jq --compact-output '.root' \
  <<<"$control_after_second_loss")"
metrics_second=""
for _ in $(seq 1 45); do
  candidate_metrics="$("${compose[@]}" exec -T server-b crab-http-server \
    --config /etc/crab/server.toml cells metrics)"
  if [ "$(metric_counter "$candidate_metrics" candidate_count)" -gt \
    "$(metric_counter "$metrics_second_before" candidate_count)" ]; then
    metrics_second="$candidate_metrics"
    break
  fi
  sleep 1
done
if [ -z "$metrics_second" ]; then
  echo "Node B did not export recovery work after the second owner loss." >&2
  exit 1
fi
work_second="$(recovery_work_evidence "$metrics_second_before" "$metrics_second")"

service_cli() {
  local service="$1"
  shift
  case "$service" in
    server|server-b|server-c|server-d)
      "${compose[@]}" exec -T "$service" crab-http-server \
        --config /etc/crab/server.toml "$@"
      ;;
    *)
      echo "unknown cluster service: $service" >&2
      return 2
      ;;
  esac
}

service_origin() {
  case "$1" in
    server) printf '%s\n' "$node_a_origin" ;;
    server-b) printf '%s\n' "$node_b_origin" ;;
    server-c) printf '%s\n' "$node_c_origin" ;;
    server-d) printf '%s\n' "$node_d_origin" ;;
    *) return 2 ;;
  esac
}

service_session() {
  node_session "$1"
}

stop_fallback_member() {
  case "$1" in
    server)
      # Pausing A preserves the shared network namespace while removing its
      # heartbeat and follower endpoint from the live fleet.
      "${compose[@]}" pause server >/dev/null
      ;;
    server-c|server-d)
      "${compose[@]}" kill --signal KILL "$1" >/dev/null
      "${compose[@]}" rm --force --stop "$1" >/dev/null
      ;;
    *)
      echo "unknown fallback member service: $1" >&2
      return 2
      ;;
  esac
}

# A fourth process is kept outside the current log when possible. The owner
# first publishes an object-covered mutation, then every original member is
# stopped before the owner is killed. Recovery must therefore use the bounded
# any-node path and still restore exact data from RustFS.
"${compose[@]}" unpause server >/dev/null
"${compose[@]}" unpause server-d >/dev/null
"${compose[@]}" up --detach --no-build server-c >/dev/null
wait_for_healthy server
wait_for_healthy server-c
wait_for_healthy server-d
session_a_fallback="$(service_session server)"
session_c_fallback="$(service_session server-c)"
session_d_fallback="$(service_session server-d)"
node_a_fallback="$(service_cli server cells node --session "$session_a_fallback" --json)"
node_c_fallback="$(service_cli server-c cells node --session "$session_c_fallback" --json)"
node_d_fallback="$(service_cli server-d cells node --session "$session_d_fallback" --json)"
node_b_before_fallback="$(service_cli server-b cells node \
  --session "$session_after_second_loss" --json)"
fallback_members="$(jq -c '.advertisement.log.member_nodes' <<<"$node_b_before_fallback")"
jq --exit-status \
  '.live == true and .advertisement.log.state == "open" and
   .advertisement.log.active == true and
   (.advertisement.log.member_nodes | length > 0)' \
  <<<"$node_b_before_fallback" >/dev/null

fallback_response="$(curl --fail-with-body --silent --show-error \
  --max-time 15 \
  --request POST \
  --header 'content-type: application/json' \
  --data '{"request_id":"00000000-0000-4000-8000-000000000106","name":"fallback-covered","color":"7c3aed","description":"Object-covered fallback recovery"}' \
  "${node_b_origin}/${repository_path}/labels")"
jq --exit-status \
  '.id == 3 and .name == "fallback-covered"' \
  <<<"$fallback_response" >/dev/null
control_before_fallback="$(service_cli server-b cells status --owner demo --name hello)"
root_before_fallback="$(jq --compact-output '.root' <<<"$control_before_fallback")"
fallback_owner_metrics_before="$(service_cli server-b cells metrics)"
fallback_object_covered=false
for _ in $(seq 1 60); do
  fallback_owner_metrics="$(service_cli server-b cells metrics)"
  fallback_uncovered_bytes="$(awk \
    '$1 == "crab_cell_node_log_uncovered_bytes" { print $2 }' \
    <<<"$fallback_owner_metrics")"
  if [ "${fallback_uncovered_bytes:-1}" = 0 ]; then
    fallback_object_covered=true
    break
  fi
  sleep 1
done
if ! $fallback_object_covered; then
  echo "The fallback mutation did not reach object coverage before member loss." >&2
  exit 1
fi

fallback_candidate_service=""
fallback_candidate_session=""
fallback_candidate_node=""
fallback_candidate_record=""
for candidate in server server-c server-d; do
  case "$candidate" in
    server) candidate_json="$node_a_fallback" ;;
    server-c) candidate_json="$node_c_fallback" ;;
    server-d) candidate_json="$node_d_fallback" ;;
  esac
  candidate_node="$(jq -r '.advertisement.node' <<<"$candidate_json")"
  if ! jq --exit-status --arg node "$candidate_node" \
    'any(.[]; . == $node)' <<<"$fallback_members" >/dev/null; then
    fallback_candidate_service="$candidate"
    fallback_candidate_session="$(jq -r '.session' <<<"$candidate_json")"
    fallback_candidate_node="$candidate_node"
    fallback_candidate_record="$candidate_json"
    break
  fi
done
if [ -z "$fallback_candidate_service" ]; then
  echo "No live non-member fallback candidate remained." >&2
  exit 1
fi
fallback_metrics_before="$(service_cli "$fallback_candidate_service" cells metrics)"

for member_service in server server-c server-d; do
  case "$member_service" in
    server) member_node_json="$node_a_fallback" ;;
    server-c) member_node_json="$node_c_fallback" ;;
    server-d) member_node_json="$node_d_fallback" ;;
  esac
  member_node="$(jq -r '.advertisement.node' <<<"$member_node_json")"
  if jq --exit-status --arg node "$member_node" \
    'any(.[]; . == $node)' <<<"$fallback_members" >/dev/null; then
    if [ "$member_service" = "$fallback_candidate_service" ]; then
      echo "fallback candidate is also an original follower" >&2
      exit 1
    fi
    stop_fallback_member "$member_service"
  fi
done

fallback_origin="$(service_origin "$fallback_candidate_service")"
fallback_owner_killed_ms="$(unix_millis)"
"${compose[@]}" kill --signal KILL server-b >/dev/null
"${compose[@]}" rm --force --stop server-b >/dev/null
"${compose[@]}" run --rm --no-deps --entrypoint aws bucket-init \
  --endpoint-url http://rustfs:9000 s3api delete-bucket-policy \
  --bucket crab-http-server >/dev/null

fallback_advertisement_expired=false
for _ in $(seq 1 60); do
  fallback_node_status="$(service_cli "$fallback_candidate_service" cells node \
    --session "$session_after_second_loss" --json 2>/dev/null || true)"
  if jq --exit-status '.live == false' <<<"$fallback_node_status" >/dev/null 2>&1; then
    fallback_advertisement_expired=true
    fallback_advertisement_expired_ms="$(unix_millis)"
    break
  fi
  sleep 1
done
if ! $fallback_advertisement_expired; then
  echo "The fallback owner's signed advertisement did not expire." >&2
  exit 1
fi

fallback_restored_labels=""
for _ in $(seq 1 75); do
  candidate_labels="$(curl --fail-with-body --silent --show-error --max-time 10 \
    "${fallback_origin}/${repository_path}/labels" || true)"
  if jq --exit-status \
    '.items | (length == 3 and
     (map(.name) | sort == ["fallback-covered", "follower-only", "replacement-follower-only"]))' \
    <<<"$candidate_labels" >/dev/null 2>&1; then
    fallback_restored_labels="$candidate_labels"
    break
  fi
  sleep 1
done
if [ -z "$fallback_restored_labels" ]; then
  echo "The non-member fallback candidate did not restore the object-covered label." >&2
  exit 1
fi

fallback_control_after=""
for _ in $(seq 1 75); do
  candidate_control="$(service_cli "$fallback_candidate_service" cells status \
    --owner demo --name hello 2>/dev/null || true)"
  if jq --exit-status \
    --arg failed_session "$session_after_second_loss" \
    '.state == "serving" and .owner.session != $failed_session and
     .owner_lease.state == "live" and
     .owner_lease.expires_at_ms > .owner_lease.observed_at_ms and
     .recovery == null and
     .root.commit_sequence > 0' <<<"$candidate_control" >/dev/null 2>&1; then
    fallback_control_after="$candidate_control"
    break
  fi
  sleep 1
done
if [ -z "$fallback_control_after" ]; then
  echo "The non-member fallback candidate did not publish serving status." >&2
  exit 1
fi
fallback_recovery_sealed_ms="$(unix_millis)"
fallback_first_served_check="$(curl --fail-with-body --silent --show-error \
  "${fallback_origin}/${repository_path}/labels")"
jq --exit-status \
  '.items | length == 3' <<<"$fallback_first_served_check" >/dev/null
fallback_first_served_ms="$(unix_millis)"
fallback_session_after="$(jq --raw-output '.owner.session' <<<"$fallback_control_after")"
fallback_epoch_before="$(jq --raw-output '.epoch' <<<"$control_before_fallback")"
fallback_epoch_after="$(jq --raw-output '.epoch' <<<"$fallback_control_after")"
fallback_root_after="$(jq --compact-output '.root' <<<"$fallback_control_after")"
fallback_node_log_before="$(jq -c '.advertisement.log' <<<"$node_b_before_fallback")"
fallback_metrics_after="$(service_cli "$fallback_candidate_service" cells metrics)"
fallback_work="$(recovery_work_evidence \
  "$fallback_metrics_before" "$fallback_metrics_after")"

failed_node_first="$(node_id_for_session "$session_before")"
successor_node_first="$(node_id_for_session "$session_after")"
first_failed_log_members="$(jq -c '.advertisement.log.member_nodes' <<<"$node_before")"
second_failed_log_members="$(jq -c '.advertisement.log.member_nodes' <<<"$node_before_second_loss")"
selection="$(jq -n \
  --arg failed_session "$session_before" \
  --arg successor_session "$session_after" \
  --arg failed_node "$failed_node_first" \
  --arg successor_node "$successor_node_first" \
  --arg second_failed_session "$session_after" \
  --arg second_successor_session "$session_after_second_loss" \
  --arg second_failed_node "$node_c_id" \
  --arg second_successor_node "$node_b_id" \
  --arg fallback_failed_session "$session_after_second_loss" \
  --arg fallback_successor_session "$fallback_session_after" \
  --arg fallback_failed_node "$node_b_id" \
  --arg fallback_successor_node "$fallback_candidate_node" \
  --argjson failed_log_members "$first_failed_log_members" \
  --argjson second_failed_log_members "$second_failed_log_members" \
  --argjson fallback_failed_log_members "$fallback_members" \
  '{
    owner_loss: {
      failed_session: $failed_session,
      successor_session: $successor_session,
      failed_node: $failed_node,
      successor_node: $successor_node,
      failed_log_members: $failed_log_members,
      selected_original_follower: true,
      terminal_result: "succeeded"
    },
    second_owner_loss: {
      failed_session: $second_failed_session,
      successor_session: $second_successor_session,
      failed_node: $second_failed_node,
      successor_node: $second_successor_node,
      failed_log_members: $second_failed_log_members,
      selected_original_follower: true,
      terminal_result: "succeeded"
    },
    fallback: {
      failed_session: $fallback_failed_session,
      successor_session: $fallback_successor_session,
      failed_node: $fallback_failed_node,
      successor_node: $fallback_successor_node,
      failed_log_members: $fallback_failed_log_members,
      selected_original_follower: false,
      terminal_result: "succeeded"
    }
  }')"
receipt_path="${CRAB_HTTP_CLUSTER_RECEIPT_PATH:-${TMPDIR:-/tmp}/crab-http-cluster-receipt-${project}.json}"
jq --null-input \
  --arg project "$project" \
  --arg source_revision "$source_revision" \
  --arg qualified_image_ref "$qualified_image_ref" \
  --arg qualified_image_digest "$qualified_image_digest" \
  --arg session_before "$session_before" \
  --arg session_after "$session_after" \
  --argjson owner_killed_ms "$owner_killed_ms" \
  --argjson advertisement_expired_ms "$advertisement_expired_ms" \
  --argjson recovery_sealed_ms "$recovery_sealed_ms" \
  --argjson first_served_ms "$first_served_ms" \
  --argjson root_before "$root_before_state" \
  --argjson root_after_restore "$root_after_state" \
  --argjson root_continued "$root_continued_state" \
  --argjson epoch_before "$epoch_before" \
  --argjson epoch_after "$epoch_after" \
  --argjson capacity_a "$capacity_a" \
  --argjson capacity_b "$capacity_b" \
  --argjson capacity_c "$capacity_c" \
  --argjson capacity_d "$capacity_d" \
  --argjson disk_probe_a "$disk_probe_a" \
  --argjson disk_probe_b "$disk_probe_b" \
  --argjson disk_probe_c "$disk_probe_c" \
  --argjson disk_probe_d "$disk_probe_d" \
  --argjson disk_probe_tolerance_bytes "$disk_probe_tolerance_bytes" \
  --arg session_a "$session_a" \
  --arg session_b "$session_b" \
  --arg session_c "$session_c" \
  --arg session_d "$session_d" \
  --arg metrics_a "$metrics_a" \
  --arg metrics_b "$metrics_b" \
  --arg metrics_c "$metrics_c" \
  --arg metrics_d "$metrics_d" \
  --argjson node_a "$node_a" \
  --argjson node_b "$node_b" \
  --argjson node_c "$node_c" \
  --argjson node_d "$node_d" \
  --argjson node_before "$node_before" \
  --argjson node_fleet_only "$node_fleet_only" \
  --argjson fleet_only_response "$fleet_only_response" \
  --argjson restored_labels "$restored_labels" \
  --argjson control_fleet_only "$control_fleet_only" \
  --argjson owner_uncovered_bytes "$owner_uncovered_bytes" \
  --argjson follower_retained_bytes "$follower_retained_bytes" \
  --argjson node_before_follower_loss "$node_before_follower_loss" \
  --argjson node_after_follower_loss "$node_after_follower_loss" \
  --argjson after_follower_loss "$after_follower_loss" \
  --argjson replacement_fleet_response "$replacement_fleet_response" \
  --argjson second_restored_labels "$second_restored_labels" \
  --arg session_after_second_loss "$session_after_second_loss" \
  --argjson second_owner_killed_ms "$second_owner_killed_ms" \
  --argjson second_advertisement_expired_ms "$second_advertisement_expired_ms" \
  --argjson second_recovery_sealed_ms "$second_recovery_sealed_ms" \
  --argjson second_first_served_ms "$second_first_served_ms" \
  --argjson epoch_after_second_loss "$epoch_after_second_loss" \
  --argjson root_before_second_loss "$root_before_second_loss" \
  --argjson root_after_second_loss "$root_after_second_loss" \
  --arg fallback_session_before "$session_after_second_loss" \
  --arg fallback_session_after "$fallback_session_after" \
  --arg fallback_candidate_service "$fallback_candidate_service" \
  --arg fallback_candidate_node "$fallback_candidate_node" \
  --argjson fallback_owner_killed_ms "$fallback_owner_killed_ms" \
  --argjson fallback_advertisement_expired_ms "$fallback_advertisement_expired_ms" \
  --argjson fallback_recovery_sealed_ms "$fallback_recovery_sealed_ms" \
  --argjson fallback_first_served_ms "$fallback_first_served_ms" \
  --argjson fallback_epoch_before "$fallback_epoch_before" \
  --argjson fallback_epoch_after "$fallback_epoch_after" \
  --argjson fallback_root_before "$root_before_fallback" \
  --argjson fallback_root_after "$fallback_root_after" \
  --argjson fallback_node_log_before "$fallback_node_log_before" \
  --argjson fallback_response "$fallback_response" \
  --argjson fallback_restored_labels "$fallback_restored_labels" \
  --argjson fallback_control_before "$control_before_fallback" \
  --argjson fallback_control_after "$fallback_control_after" \
  --arg fallback_candidate_session "$fallback_candidate_session" \
  --argjson fallback_candidate_record "$fallback_candidate_record" \
  --argjson fallback_work "$fallback_work" \
  --argjson selection "$selection" \
  --argjson work_first "$work_first" \
  --argjson work_second "$work_second" \
  '{
    version: 6,
    source_revision: $source_revision,
    image: {
      reference: $qualified_image_ref,
      digest: $qualified_image_digest
    },
    project: $project,
    owner_loss: {
      session_before: $session_before,
      session_after: $session_after,
      epoch_before: $epoch_before,
      epoch_after: $epoch_after,
      timing: {
        owner_killed_ms: $owner_killed_ms,
        advertisement_expired_ms: $advertisement_expired_ms,
        recovery_sealed_ms: $recovery_sealed_ms,
        first_served_ms: $first_served_ms
      },
      root_before: $root_before,
      root_after_restore: $root_after_restore,
      root_continued: $root_continued
    },
    fleet_only_commit: {
      node_log_before: $node_before.advertisement.log,
      node_log_after: $node_fleet_only.advertisement.log,
      response: $fleet_only_response,
      restored_labels: $restored_labels,
      control_before_owner_loss: $control_fleet_only,
      owner_uncovered_bytes: $owner_uncovered_bytes,
      follower_retained_bytes: $follower_retained_bytes,
      immutable_object_put_rejected: true,
      owner_disk_removed_before_policy_restore: true
    },
    follower_replacement: {
      node_log_before: $node_before_follower_loss.advertisement.log,
      node_log_after: $node_after_follower_loss.advertisement.log,
      object_covered_response: $after_follower_loss,
      fleet_only_response: $replacement_fleet_response
    },
    second_owner_loss: {
      session_before: $session_after,
      session_after: $session_after_second_loss,
      epoch_before: $epoch_after,
      epoch_after: $epoch_after_second_loss,
      timing: {
        owner_killed_ms: $second_owner_killed_ms,
        advertisement_expired_ms: $second_advertisement_expired_ms,
        recovery_sealed_ms: $second_recovery_sealed_ms,
        first_served_ms: $second_first_served_ms
      },
      root_before: $root_before_second_loss,
      root_after: $root_after_second_loss,
      restored_labels: $second_restored_labels
    },
    fallback_owner_loss: {
      session_before: $fallback_session_before,
      session_after: $fallback_session_after,
      candidate_service: $fallback_candidate_service,
      candidate_session: $fallback_candidate_session,
      candidate_node: $fallback_candidate_node,
      candidate_record: $fallback_candidate_record,
      epoch_before: $fallback_epoch_before,
      epoch_after: $fallback_epoch_after,
      timing: {
        owner_killed_ms: $fallback_owner_killed_ms,
        advertisement_expired_ms: $fallback_advertisement_expired_ms,
        recovery_sealed_ms: $fallback_recovery_sealed_ms,
        first_served_ms: $fallback_first_served_ms
      },
      root_before: $fallback_root_before,
      root_after: $fallback_root_after,
      node_log_before: $fallback_node_log_before,
      response: $fallback_response,
      restored_labels: $fallback_restored_labels,
      control_before_owner_loss: $fallback_control_before,
      control_after: $fallback_control_after
    },
    capacity: {node_a: $capacity_a, node_b: $capacity_b, node_c: $capacity_c, node_d: $capacity_d},
    measured_disk: {
      node_a_bytes: $disk_probe_a,
      node_b_bytes: $disk_probe_b,
      node_c_bytes: $disk_probe_c,
      node_d_bytes: $disk_probe_d,
      tolerance_bytes: $disk_probe_tolerance_bytes
    },
    placement: {
      node_a_session: $session_a,
      node_b_session: $session_b,
      node_c_session: $session_c,
      node_d_session: $session_d,
      node_a: $node_a,
      node_b: $node_b,
      node_c: $node_c,
      node_d: $node_d
    },
    metrics: {node_a: $metrics_a, node_b: $metrics_b, node_c: $metrics_c, node_d: $metrics_d},
    capacity_metric_parity: {
      local_disk: true,
      active_cells: true,
      measured_local_disk: true,
      signed_placement: true
    },
    selection: $selection,
    work: {
      owner_loss: $work_first,
      second_owner_loss: $work_second,
      fallback: $fallback_work
    }
  }' > "$receipt_path"
if [ "${CRAB_HTTP_CLUSTER_VALIDATE:-false}" = true ]; then
  receipt_mode=source-only
  if [ "$qualified_image_ref" != source-only ]; then
    receipt_mode=release
  fi
  CARGO_TARGET_DIR="${CRAB_HTTP_CLUSTER_CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/crab-http-cluster-target}" \
    cargo run --quiet --locked -p crab-cell-runtime --bin qualification_receipt -- \
      validate-cluster "$receipt_path" "$source_revision" "$qualified_image_digest" "$receipt_mode"
fi
cat "$receipt_path"
