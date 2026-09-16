#!/usr/bin/env bash
set -euo pipefail

compose_file="${1:?usage: qualify_abrupt_receive_crash.sh compose-file}"
origin="http://127.0.0.1:${CRAB_HTTP_SERVER_PORT:-8788}"
remote="${origin}/git/demo/hello.git"
work_root="${RUNNER_TEMP:?RUNNER_TEMP must name disposable qualification storage}"
work_dir="$(mktemp -d "${work_root}/crab-http-server-crash.XXXXXX")"
compose=(docker compose --file "$compose_file")
server_id="$("${compose[@]}" ps --quiet server)"
push_pid=""
server_paused=false

if [ -z "$server_id" ]; then
  echo "The Compose server service must be running." >&2
  exit 1
fi

cleanup() {
  if $server_paused; then
    docker unpause "$server_id" >/dev/null 2>&1 || true
  fi
  if [ -n "$push_pid" ] && kill -0 "$push_pid" 2>/dev/null; then
    kill "$push_pid" >/dev/null 2>&1 || true
    wait "$push_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

GIT_TERMINAL_PROMPT=0 git clone "$remote" "${work_dir}/source"
git -C "${work_dir}/source" config user.name "Crab qualification"
git -C "${work_dir}/source" config user.email "qualification@example.invalid"
if ! git -C "${work_dir}/source" rev-parse --verify HEAD >/dev/null 2>&1; then
  printf 'abrupt receive crash qualification\n' > "${work_dir}/source/README.md"
  git -C "${work_dir}/source" add README.md
  git -C "${work_dir}/source" commit -m "seed crash qualification"
  GIT_TERMINAL_PROMPT=0 git -C "${work_dir}/source" push --set-upstream origin main
fi
for file_number in $(seq -w 1 16); do
  dd if=/dev/urandom \
    of="${work_dir}/source/crash-payload-${file_number}.raw" \
    bs=1048576 count=8 status=none
done
git -C "${work_dir}/source" add .
git -C "${work_dir}/source" commit -m "qualify abrupt receive crash"

old_oid="$(git -C "${work_dir}/source" rev-parse HEAD^)"
new_oid="$(git -C "${work_dir}/source" rev-parse HEAD)"
GIT_TERMINAL_PROMPT=0 git -C "${work_dir}/source" push origin main \
  >"${work_dir}/push.log" 2>&1 &
push_pid=$!
observed_staging=false
for _attempt in $(seq 1 1200); do
  if docker exec "$server_id" sh -c \
    "find /var/lib/crab/cells -path '*/transfers/transfer-*/*' -type f -size +1M -print -quit" \
    | grep -q .; then
    observed_staging=true
    break
  fi
  if ! kill -0 "$push_pid" 2>/dev/null; then
    break
  fi
  sleep 0.05
done
if ! $observed_staging; then
  sed -n '1,160p' "${work_dir}/push.log"
  echo "No in-flight staged Git pack appeared before the push stopped." >&2
  exit 1
fi

# Freeze the process while the receive pack is still incomplete, then remove it
# without allowing Crab's cooperative cancellation or drain path to run.
docker pause "$server_id" >/dev/null
server_paused=true
docker kill --signal KILL "$server_id" >/dev/null
server_paused=false
test "$(docker inspect "$server_id" --format '{{.State.ExitCode}}')" = 137

set +e
wait "$push_pid"
push_status=$?
set -e
push_pid=""
if [ "$push_status" -eq 0 ]; then
  echo "The push completed before the forced process loss." >&2
  exit 1
fi

# Compose does not replace a manually killed container. Recreate the Crab and
# shared-network proxy containers to model an orchestrator starting a fresh pod.
"${compose[@]}" up --detach --no-build --force-recreate \
  server proxy
replacement_server_id="$("${compose[@]}" ps --quiet server)"
test -n "$replacement_server_id"

# Compose's --wait exits as soon as the proxy becomes unhealthy, before the
# publication lease recovery budget expires. Keep probing the real public data
# path so a recovering repository can become healthy without weakening the
# post-crash readiness assertion.
proxy_ready=false
for recovery_attempt in $(seq 1 36); do
  if curl --fail --silent --show-error --max-time 5 \
    "${origin}/api/repos" >/dev/null; then
    proxy_ready=true
    break
  fi
  if [ "$recovery_attempt" -lt 36 ]; then
    sleep 10
  fi
done
if ! $proxy_ready; then
  "${compose[@]}" ps --all >&2 || true
  "${compose[@]}" logs --no-color server proxy >&2 || true
  echo "The recreated public data path did not recover." >&2
  exit 1
fi

remote_after_crash="$(git ls-remote "$remote" refs/heads/main | cut -f1)"
if [ "$remote_after_crash" != "$old_oid" ] && [ "$remote_after_crash" != "$new_oid" ]; then
  echo "Abrupt restart exposed unexpected ref $remote_after_crash." >&2
  exit 1
fi
retry_succeeded=false
for retry_attempt in $(seq 1 36); do
  if GIT_TERMINAL_PROMPT=0 git -C "${work_dir}/source" push origin main; then
    retry_succeeded=true
    break
  fi
  if [ "$retry_attempt" -lt 36 ]; then
    sleep 10
  fi
done
if ! $retry_succeeded; then
  echo "The push did not recover within the five-minute publication lease plus grace." >&2
  exit 1
fi

test "$(git ls-remote "$remote" refs/heads/main | cut -f1)" = "$new_oid"
verify_dir=""
for clone_attempt in $(seq 1 36); do
  candidate="${work_dir}/verify-${clone_attempt}"
  if GIT_TERMINAL_PROMPT=0 git clone "$remote" "$candidate"; then
    verify_dir="$candidate"
    break
  fi
  if [ "$clone_attempt" -lt 36 ]; then
    sleep 10
  fi
done
if [ -z "$verify_dir" ]; then
  echo "The committed ref did not become readable within the recovery budget." >&2
  exit 1
fi
test "$(git -C "$verify_dir" rev-parse HEAD)" = "$new_oid"
for file_number in $(seq -w 1 16); do
  cmp "${work_dir}/source/crash-payload-${file_number}.raw" \
    "${verify_dir}/crash-payload-${file_number}.raw"
done
test "$(docker inspect "$replacement_server_id" --format '{{.RestartCount}}')" = 0
test "$(docker inspect "$replacement_server_id" --format '{{.State.Running}}')" = true

printf 'Abrupt receive crash recovered old=%s new=%s observed=%s\n' \
  "$old_oid" "$new_oid" "$remote_after_crash"
