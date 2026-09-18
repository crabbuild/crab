#!/usr/bin/env bash
set -euo pipefail
set +x

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate_dir="$(cd "${script_dir}/.." && pwd)"
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
repository_path="api/repos/demo/hello"
failed=false

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
for capacity in "$capacity_a" "$capacity_b" "$capacity_c"; do
  jq --exit-status \
    '.version == 1 and .resources.memory_bytes > 0 and
     .resources.free_disk_bytes > 0 and .admission.active_cells > 0' \
    <<<"$capacity" >/dev/null
done

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

control_after="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
session_after="$(jq --raw-output '.owner.session' <<<"$control_after")"
epoch_after="$(jq --raw-output '.epoch' <<<"$control_after")"
root_after_state="$(jq --compact-output '.root' <<<"$control_after")"
jq --exit-status \
  --arg session_before "$session_before" \
  --argjson epoch_before "$epoch_before" \
  --argjson sequence_before "$sequence_before" \
  '.state == "serving" and .owner.endpoint == "https://localhost:8989/" and
   .owner.session != $session_before and .epoch > $epoch_before and
   .owner_lease.state == "live" and
   .owner_lease.expires_at_ms > .owner_lease.observed_at_ms and
   .recovery == null and
   .root.commit_sequence > $sequence_before' <<<"$control_after" >/dev/null

for _ in $(seq 1 6); do
  curl --fail --silent --show-error \
    "${cluster_origin}/${repository_path}/issues?state=all" \
    | jq --exit-status \
      '.items | length == 1 and .[0].title == "Owner loss qualification"' \
      >/dev/null
  curl --fail --silent --show-error \
    "${cluster_origin}/${repository_path}/labels" \
    | jq --exit-status \
      '.items | length == 1 and .[0].name == "follower-only"' \
      >/dev/null
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

"${compose[@]}" up --detach --no-build server server-b >/dev/null
wait_for_healthy server-b
curl --fail --silent --show-error \
  "${node_b_origin}/${repository_path}/issues?state=all" \
  | jq --exit-status \
    '.items | length == 2 and .[0].title == "Recovered owner" and
     .[1].title == "Owner loss qualification"' >/dev/null
curl --fail --silent --show-error \
  "${node_b_origin}/${repository_path}/labels" \
  | jq --exit-status \
    '.items | length == 1 and .[0].name == "follower-only"' >/dev/null
control_after_rejoin="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
jq --exit-status \
  --arg session_after "$session_after" \
  --argjson epoch_after "$epoch_after" \
  --argjson sequence_continued "$sequence_continued" \
  '.state == "serving" and .owner.session == $session_after and
   .owner.endpoint == "https://localhost:8989/" and .epoch == $epoch_after and
   .owner_lease.state == "live" and
   .owner_lease.expires_at_ms > .owner_lease.observed_at_ms and
   .recovery == null and
   .root.commit_sequence == $sequence_continued' \
  <<<"$control_after_rejoin" >/dev/null

node_before_follower_loss="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells node \
  --session "$session_after" --json)"
node_b_id="$(jq --raw-output '.advertisement.node' <<<"$node_before")"
log_epoch_before_follower_loss="$(jq --raw-output \
  '.advertisement.log.epoch' <<<"$node_before_follower_loss")"
jq --exit-status \
  --arg node_b "$node_b_id" \
  '.live == true and .advertisement.log.state == "open" and
   any(.advertisement.log.member_nodes[]; . != $node_b)' \
  <<<"$node_before_follower_loss" >/dev/null

# Pausing node A keeps the shared Compose network namespace alive while its
# process, heartbeats, and follower endpoint are unavailable.
"${compose[@]}" pause server >/dev/null
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

"${compose[@]}" kill --signal KILL server-c >/dev/null
"${compose[@]}" rm --force --stop server-c >/dev/null
"${compose[@]}" run --rm --no-deps --entrypoint aws bucket-init \
  --endpoint-url http://rustfs:9000 s3api delete-bucket-policy \
  --bucket crab-http-server >/dev/null

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
curl --fail --silent --show-error \
  "${node_b_origin}/${repository_path}/issues?state=all" \
  | jq --exit-status '.items | length == 3' >/dev/null
control_after_second_loss="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
session_after_second_loss="$(jq --raw-output '.owner.session' \
  <<<"$control_after_second_loss")"
epoch_after_second_loss="$(jq --raw-output '.epoch' \
  <<<"$control_after_second_loss")"
root_after_second_loss="$(jq --compact-output '.root' \
  <<<"$control_after_second_loss")"
jq --exit-status \
  --arg session_after "$session_after" \
  --argjson epoch_after "$epoch_after" \
  '.state == "serving" and .owner.session != $session_after and
   .epoch > $epoch_after and .owner_lease.state == "live" and
   .recovery == null' <<<"$control_after_second_loss" >/dev/null

jq --null-input \
  --arg project "$project" \
  --arg session_before "$session_before" \
  --arg session_after "$session_after" \
  --argjson root_before "$root_before_state" \
  --argjson root_after_restore "$root_after_state" \
  --argjson root_continued "$root_continued_state" \
  --argjson epoch_before "$epoch_before" \
  --argjson epoch_after "$epoch_after" \
  --argjson capacity_a "$capacity_a" \
  --argjson capacity_b "$capacity_b" \
  --argjson capacity_c "$capacity_c" \
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
  --argjson epoch_after_second_loss "$epoch_after_second_loss" \
  --argjson root_before_second_loss "$root_before_second_loss" \
  --argjson root_after_second_loss "$root_after_second_loss" \
  '{
    version: 5,
    project: $project,
    owner_loss: {
      session_before: $session_before,
      session_after: $session_after,
      epoch_before: $epoch_before,
      epoch_after: $epoch_after,
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
      root_before: $root_before_second_loss,
      root_after: $root_after_second_loss,
      restored_labels: $second_restored_labels
    },
    capacity: {node_a: $capacity_a, node_b: $capacity_b, node_c: $capacity_c}
  }'
