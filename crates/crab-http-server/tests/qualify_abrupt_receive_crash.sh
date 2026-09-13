#!/usr/bin/env bash
set -euo pipefail

compose_file="${1:?usage: qualify_abrupt_receive_crash.sh compose-file}"
origin="http://127.0.0.1:${CRAB_HTTP_SERVER_PORT:-8788}"
remote="${origin}/git/demo/hello.git"
work_root="${RUNNER_TEMP:?RUNNER_TEMP must name disposable qualification storage}"
work_dir="$(mktemp -d "${work_root}/crab-http-server-crash.XXXXXX")"
compose=(docker compose --file "$compose_file")
server_id="$("${compose[@]}" ps --quiet server)"
rustfs_id="$("${compose[@]}" ps --quiet rustfs)"
push_pid=""
rustfs_paused=false

if [ -z "$server_id" ] || [ -z "$rustfs_id" ]; then
  echo "The Compose server and RustFS services must be running." >&2
  exit 1
fi

cleanup() {
  if $rustfs_paused; then
    docker unpause "$rustfs_id" >/dev/null 2>&1 || true
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
    of="${work_dir}/source/crash-payload-${file_number}.bin" \
    bs=1048576 count=8 status=none
done
git -C "${work_dir}/source" add .
git -C "${work_dir}/source" commit -m "qualify abrupt receive crash"

old_oid="$(git -C "${work_dir}/source" rev-parse HEAD^)"
new_oid="$(git -C "${work_dir}/source" rev-parse HEAD)"
pack_root=/data/crab-http-server/repositories/demo/hello/packs
baseline="$(docker exec "$rustfs_id" sh -c \
  "find '$pack_root' -type f | wc -l")"

GIT_TERMINAL_PROMPT=0 git -C "${work_dir}/source" push origin main \
  >"${work_dir}/push.log" 2>&1 &
push_pid=$!
observed_pack=false
for _attempt in $(seq 1 1200); do
  current="$(docker exec "$rustfs_id" sh -c \
    "find '$pack_root' -type f | wc -l")"
  if [ "$current" -gt "$baseline" ]; then
    observed_pack=true
    break
  fi
  if ! kill -0 "$push_pid" 2>/dev/null; then
    break
  fi
  sleep 0.05
done
if ! $observed_pack; then
  sed -n '1,160p' "${work_dir}/push.log"
  echo "No in-flight immutable pack appeared before the push stopped." >&2
  exit 1
fi

# Stop storage at an observed publication boundary, then remove the process
# without allowing Crab's cooperative cancellation or drain path to run.
docker pause "$rustfs_id" >/dev/null
rustfs_paused=true
docker kill --signal KILL "$server_id" >/dev/null
test "$(docker inspect "$server_id" --format '{{.State.ExitCode}}')" = 137
docker unpause "$rustfs_id" >/dev/null
rustfs_paused=false
for _attempt in $(seq 1 60); do
  health="$(docker inspect "$rustfs_id" \
    --format '{{if .State.Health}}{{.State.Health.Status}}{{end}}')"
  if [ "$health" = healthy ]; then
    break
  fi
  sleep 1
done
test "$(docker inspect "$rustfs_id" --format '{{.State.Health.Status}}')" = healthy

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
  --wait --wait-timeout 120 server proxy
replacement_server_id="$("${compose[@]}" ps --quiet server)"
test -n "$replacement_server_id"

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
  cmp "${work_dir}/source/crash-payload-${file_number}.bin" \
    "${verify_dir}/crash-payload-${file_number}.bin"
done
test "$(docker inspect "$replacement_server_id" --format '{{.RestartCount}}')" = 0
test "$(docker inspect "$replacement_server_id" --format '{{.State.Running}}')" = true

printf 'Abrupt receive crash recovered old=%s new=%s observed=%s\n' \
  "$old_oid" "$new_oid" "$remote_after_crash"
