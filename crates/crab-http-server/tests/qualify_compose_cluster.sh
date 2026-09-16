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

up_mode=(--no-build)
if [ "${CRAB_HTTP_CLUSTER_BUILD:-true}" = true ]; then
  up_mode=(--build)
fi
"${compose[@]}" config --quiet
"${compose[@]}" up --detach "${up_mode[@]}" --wait --wait-timeout 180

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

create_response="$(curl --fail-with-body --silent --show-error \
  --request POST \
  --header 'content-type: application/json' \
  --data '{"request_id":"00000000-0000-4000-8000-000000000101","title":"Owner loss qualification","body":"Created through node B"}' \
  "${node_b_origin}/${repository_path}/issues")"
jq --exit-status \
  '.number == 1 and .title == "Owner loss qualification"' \
  <<<"$create_response" >/dev/null

control_before="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
session_before="$(jq --raw-output '.owner.session' <<<"$control_before")"
epoch_before="$(jq --raw-output '.epoch' <<<"$control_before")"
sequence_before="$(jq --raw-output '.root.commit_sequence' <<<"$control_before")"
root_before="$(jq --raw-output '.root.digest' <<<"$control_before")"
root_before_state="$(jq --compact-output '.root' <<<"$control_before")"
jq --exit-status \
  '.state == "serving" and .owner.endpoint == "https://localhost:8889/" and
   .root.commit_sequence >= 1' <<<"$control_before" >/dev/null

for origin in "$node_a_origin" "$node_c_origin" "$cluster_origin"; do
  curl --fail --silent --show-error \
    "${origin}/${repository_path}/issues?state=all" \
    | jq --exit-status \
      '.items | length == 1 and .[0].title == "Owner loss qualification"' \
      >/dev/null
done

"${compose[@]}" kill --signal KILL server-b >/dev/null
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

restored="$(curl --fail-with-body --silent --show-error \
  --max-time 90 "${node_c_origin}/${repository_path}/issues?state=all")"
jq --exit-status \
  '.items | length == 1 and .[0].title == "Owner loss qualification"' \
  <<<"$restored" >/dev/null

control_after="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
session_after="$(jq --raw-output '.owner.session' <<<"$control_after")"
epoch_after="$(jq --raw-output '.epoch' <<<"$control_after")"
sequence_after_restore="$(jq --raw-output '.root.commit_sequence' <<<"$control_after")"
jq --exit-status \
  --arg session_before "$session_before" \
  --argjson epoch_before "$epoch_before" \
  --argjson root_before "$root_before_state" \
  '.state == "serving" and .owner.endpoint == "https://localhost:8989/" and
   .owner.session != $session_before and .epoch > $epoch_before and
   .root == $root_before' <<<"$control_after" >/dev/null

for _ in $(seq 1 6); do
  curl --fail --silent --show-error \
    "${cluster_origin}/${repository_path}/issues?state=all" \
    | jq --exit-status \
      '.items | length == 1 and .[0].title == "Owner loss qualification"' \
      >/dev/null
done

continued="$(curl --fail-with-body --silent --show-error \
  --request POST \
  --header 'content-type: application/json' \
  --data '{"request_id":"00000000-0000-4000-8000-000000000102","title":"Recovered owner","body":"Published by node C"}' \
  "${node_c_origin}/${repository_path}/issues")"
jq --exit-status '.number == 2 and .title == "Recovered owner"' \
  <<<"$continued" >/dev/null

control_continued="$("${compose[@]}" exec -T server-c crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
sequence_continued="$(jq --raw-output '.root.commit_sequence' <<<"$control_continued")"
jq --exit-status \
  --arg session_after "$session_after" \
  --argjson sequence_before "$sequence_before" \
  '.owner.session == $session_after and
   .root.commit_sequence > $sequence_before' \
  <<<"$control_continued" >/dev/null

"${compose[@]}" start server-b >/dev/null
server_b_healthy=false
for _ in $(seq 1 90); do
  server_b_container="$("${compose[@]}" ps --quiet server-b)"
  if [ -n "$server_b_container" ] &&
    [ "$(docker inspect --format '{{.State.Health.Status}}' "$server_b_container")" = healthy ]; then
    server_b_healthy=true
    break
  fi
  sleep 1
done
if ! $server_b_healthy; then
  echo "Node B did not become healthy after restart." >&2
  exit 1
fi
curl --fail --silent --show-error \
  "${node_b_origin}/${repository_path}/issues?state=all" \
  | jq --exit-status \
    '.items | length == 2 and .[0].title == "Recovered owner" and
     .[1].title == "Owner loss qualification"' >/dev/null
control_after_rejoin="$("${compose[@]}" exec -T server-b crab-http-server \
  --config /etc/crab/server.toml cells status --owner demo --name hello)"
jq --exit-status \
  --arg session_after "$session_after" \
  --argjson epoch_after "$epoch_after" \
  --argjson sequence_continued "$sequence_continued" \
  '.state == "serving" and .owner.session == $session_after and
   .owner.endpoint == "https://localhost:8989/" and .epoch == $epoch_after and
   .root.commit_sequence == $sequence_continued' \
  <<<"$control_after_rejoin" >/dev/null

jq --null-input \
  --arg project "$project" \
  --arg session_before "$session_before" \
  --arg session_after "$session_after" \
  --arg root "$root_before" \
  --argjson epoch_before "$epoch_before" \
  --argjson epoch_after "$epoch_after" \
  --argjson sequence_before "$sequence_before" \
  --argjson sequence_after_restore "$sequence_after_restore" \
  --argjson sequence_continued "$sequence_continued" \
  --argjson capacity_a "$capacity_a" \
  --argjson capacity_b "$capacity_b" \
  --argjson capacity_c "$capacity_c" \
  '{
    version: 1,
    project: $project,
    owner_loss: {
      session_before: $session_before,
      session_after: $session_after,
      epoch_before: $epoch_before,
      epoch_after: $epoch_after,
      root_digest: $root,
      sequence_before: $sequence_before,
      sequence_after_restore: $sequence_after_restore,
      sequence_continued: $sequence_continued
    },
    capacity: {node_a: $capacity_a, node_b: $capacity_b, node_c: $capacity_c}
  }'
