#!/usr/bin/env bash
set -euo pipefail

compose_file="${1:?usage: qualify_backup_restore.sh compose-file}"
source_origin="http://127.0.0.1:${CRAB_HTTP_SERVER_PORT:-8788}"
restore_port="${CRAB_HTTP_SERVER_RESTORE_PORT:-18789}"
restore_origin="http://127.0.0.1:${restore_port}"
work_root="${RUNNER_TEMP:?RUNNER_TEMP must name disposable qualification storage}"
work_dir="$(mktemp -d "${work_root}/crab-http-server-restore.XXXXXX")"
chmod 0755 "$work_dir"
deploy_dir="$(cd "$(dirname "$compose_file")" && pwd)"
compose=(docker compose --file "$compose_file")
server_id="$("${compose[@]}" ps --quiet server)"
proxy_id="$("${compose[@]}" ps --quiet proxy)"
rustfs_id="$("${compose[@]}" ps --quiet rustfs)"
source_stopped=false
suffix="${work_dir##*.}"
restore_prefix="restore-${suffix}"
restore_server="crab-http-server-restore-${suffix}"
restore_proxy="crab-http-server-restore-proxy-${suffix}"

if [ -z "$server_id" ] || [ -z "$proxy_id" ] || [ -z "$rustfs_id" ]; then
  echo "The Compose server, proxy, and RustFS services must be running." >&2
  exit 1
fi

cleanup() {
  result=$?
  docker rm --force "$restore_proxy" "$restore_server" >/dev/null 2>&1 || true
  if $source_stopped; then
    "${compose[@]}" up --detach --no-build --wait --wait-timeout 120 \
      server proxy >/dev/null 2>&1 || true
  fi
  exit "$result"
}
trap cleanup EXIT

source_dir="${work_dir}/source-repository"
GIT_TERMINAL_PROMPT=0 git clone "${source_origin}/git/demo/hello.git" "$source_dir"
git -C "$source_dir" config user.name "Crab qualification"
git -C "$source_dir" config user.email "qualification@example.invalid"
if ! git -C "$source_dir" rev-parse --verify HEAD >/dev/null 2>&1; then
  printf 'backup and restore qualification\n' > "${source_dir}/README.md"
  git -C "$source_dir" add README.md
  git -C "$source_dir" commit -m "seed restore qualification"
  GIT_TERMINAL_PROMPT=0 git -C "$source_dir" push --set-upstream origin main
fi
source_oid="$(git -C "$source_dir" rev-parse HEAD)"

issue_request='01931b9e-4b3c-7b2a-b9f0-0123456789ab'
issue_title='Restore qualification'
issue_body='This issue must survive a complete-root restore.'
curl --fail --silent --show-error \
  --header 'content-type: application/json' \
  --data "{\"request_id\":\"${issue_request}\",\"title\":\"${issue_title}\",\"body\":\"${issue_body}\"}" \
  --output "${work_dir}/source-issue.json" \
  "${source_origin}/api/repos/demo/hello/issues"
jq --exit-status \
  --arg title "$issue_title" --arg body "$issue_body" \
  '.number == 1 and .title == $title and .body == $body' \
  "${work_dir}/source-issue.json" >/dev/null

printf 'LFS bytes must survive a complete-root restore.\n' > "${work_dir}/lfs-source"
lfs_oid="$(shasum -a 256 "${work_dir}/lfs-source" | awk '{print $1}')"
lfs_size="$(wc -c < "${work_dir}/lfs-source" | tr -d '[:space:]')"
lfs_path="/git/demo/hello.git/info/lfs/objects/${lfs_oid}?size=${lfs_size}"
curl --fail --silent --show-error --request PUT \
  --data-binary "@${work_dir}/lfs-source" \
  --output /dev/null \
  "${source_origin}${lfs_path}"
curl --fail --silent --show-error --output "${work_dir}/lfs-source-read" \
  "${source_origin}${lfs_path}"
cmp "${work_dir}/lfs-source" "${work_dir}/lfs-source-read"

"${compose[@]}" stop proxy
"${compose[@]}" stop --timeout 630 server
source_stopped=true

aws_cli=(
  "${compose[@]}" run --rm --no-deps
  --interactive=false --no-TTY
  --entrypoint aws bucket-init
  --endpoint-url http://rustfs:9000
)
"${aws_cli[@]}" s3 cp s3://crab-http-server/repositories/ \
  "s3://crab-http-server/${restore_prefix}/" --recursive --only-show-errors
"${aws_cli[@]}" s3api list-objects-v2 --bucket crab-http-server \
  --prefix repositories/ --output json > "${work_dir}/source-objects.json"
"${aws_cli[@]}" s3api list-objects-v2 --bucket crab-http-server \
  --prefix "${restore_prefix}/" --output json > "${work_dir}/restored-objects.json"
printf 'Copied complete storage root into isolated prefix %s\n' "$restore_prefix"

jq --exit-status '(.IsTruncated // false) == false' \
  "${work_dir}/source-objects.json" "${work_dir}/restored-objects.json" >/dev/null
jq --arg prefix 'repositories/' \
  '[.Contents[]? | {key: (.Key | ltrimstr($prefix)), size: .Size}] | sort_by(.key)' \
  "${work_dir}/source-objects.json" > "${work_dir}/source-manifest.json"
jq --arg prefix "${restore_prefix}/" \
  '[.Contents[]? | {key: (.Key | ltrimstr($prefix)), size: .Size}] | sort_by(.key)' \
  "${work_dir}/restored-objects.json" > "${work_dir}/restored-manifest.json"
cmp "${work_dir}/source-manifest.json" "${work_dir}/restored-manifest.json"

jq --exit-status \
  'any(.[]; .key == ".crab/http-server/v1/catalog.json")' \
  "${work_dir}/source-manifest.json" >/dev/null
jq --exit-status \
  'any(.[]; .key | startswith("demo/hello/app/v1/issues/"))' \
  "${work_dir}/source-manifest.json" >/dev/null
object_count="$(jq 'length' "${work_dir}/source-manifest.json")"
test "$object_count" -gt 0
object_digest() {
  local object_uri="$1"
  local digest
  if ! digest="$(
    "${aws_cli[@]}" s3 cp "$object_uri" - --only-show-errors \
      | shasum -a 256 | awk '{print $1}'
  )"; then
    echo "Could not hash object ${object_uri}." >&2
    return 1
  fi
  if [[ ! "$digest" =~ ^[0-9a-f]{64}$ ]]; then
    echo "Object hash was invalid for ${object_uri}." >&2
    return 1
  fi
  printf '%s' "$digest"
}
verified_objects=0
while IFS= read -r -d '' key; do
  source_digest="$(object_digest \
    "s3://crab-http-server/repositories/${key}")"
  restored_digest="$(object_digest \
    "s3://crab-http-server/${restore_prefix}/${key}")"
  if [ "$source_digest" != "$restored_digest" ]; then
    echo "Restored object differs from source: ${key}" >&2
    exit 1
  fi
  verified_objects=$((verified_objects + 1))
done < <(jq --join-output '.[] | .key, "\u0000"' \
  "${work_dir}/source-manifest.json")
test "$verified_objects" -eq "$object_count"
printf 'Verified %s restored object bodies byte-for-byte\n' "$verified_objects"

cat > "${work_dir}/restore.server.toml" <<EOF
listen = "127.0.0.1:8788"
management_listen = "127.0.0.1:8789"

[storage]
url = "s3://crab-http-server/${restore_prefix}"
EOF
chmod 0644 "${work_dir}/restore.server.toml"

network_name="$(docker inspect "$rustfs_id" \
  --format '{{json .NetworkSettings.Networks}}' | jq --raw-output 'keys[0]')"
server_image="$(docker inspect "$server_id" --format '{{.Config.Image}}')"
proxy_image="$(docker inspect "$proxy_id" --format '{{.Config.Image}}')"

docker run --detach --name "$restore_server" \
  --network "$network_name" \
  --publish "127.0.0.1:${restore_port}:8080" \
  --env AWS_ACCESS_KEY_ID=crab-local-access \
  --env AWS_SECRET_ACCESS_KEY=crab-local-secret-key \
  --env AWS_DEFAULT_REGION=us-east-1 \
  --env AWS_ENDPOINT_URL_S3=http://rustfs:9000 \
  --env AWS_ALLOW_HTTP=true \
  --env AWS_VIRTUAL_HOSTED_STYLE_REQUEST=false \
  --env AWS_EC2_METADATA_DISABLED=true \
  --read-only --cap-drop ALL --security-opt no-new-privileges:true \
  --tmpfs /var/lib/crab/tmp:rw,noexec,nosuid,nodev,size=2g,uid=10001,gid=10001,mode=0700 \
  --volume "${work_dir}/restore.server.toml:/etc/crab/server.toml:ro" \
  "$server_image" >/dev/null
docker run --detach --name "$restore_proxy" \
  --network "container:${restore_server}" \
  --user 65534:65534 \
  --read-only --cap-drop ALL --cap-add NET_BIND_SERVICE \
  --security-opt no-new-privileges:true \
  --tmpfs /config:rw,noexec,nosuid,nodev,size=8m,uid=65534,gid=65534,mode=0700 \
  --tmpfs /data:rw,noexec,nosuid,nodev,size=8m,uid=65534,gid=65534,mode=0700 \
  --volume "${deploy_dir}/compose.Caddyfile:/etc/caddy/Caddyfile:ro" \
  "$proxy_image" >/dev/null

restore_ready=false
for _attempt in $(seq 1 120); do
  if curl --fail --silent --show-error \
    --output "${work_dir}/restored-repositories.json" \
    "${restore_origin}/api/repos" 2>/dev/null; then
    restore_ready=true
    break
  fi
  sleep 1
done
if ! $restore_ready; then
  docker logs "$restore_server" >&2 || true
  docker logs "$restore_proxy" >&2 || true
  echo "The isolated restored server did not become ready." >&2
  exit 1
fi

jq --exit-status \
  '.repositories | map(select(.owner == "demo" and .name == "hello")) | length == 1' \
  "${work_dir}/restored-repositories.json" >/dev/null
docker exec "$restore_server" crab-http-server \
  --config /etc/crab/server.toml healthcheck
docker exec "$restore_server" crab-http-server \
  --config /etc/crab/server.toml repository list \
  | jq --exit-status \
    '.repositories | map(select(.owner == "demo" and .name == "hello")) | length == 1' \
    >/dev/null

restored_repository=""
for clone_attempt in $(seq 1 12); do
  candidate="${work_dir}/restored-repository-${clone_attempt}"
  if GIT_TERMINAL_PROMPT=0 git clone \
    "${restore_origin}/git/demo/hello.git" "$candidate"; then
    restored_repository="$candidate"
    break
  fi
  if [ "$clone_attempt" -lt 12 ]; then
    sleep 10
  fi
done
if [ -z "$restored_repository" ]; then
  echo "The restored repository did not become readable within two minutes." >&2
  exit 1
fi
test "$(git -C "$restored_repository" rev-parse HEAD)" = "$source_oid"
curl --fail --silent --show-error \
  --output "${work_dir}/restored-issue.json" \
  "${restore_origin}/api/repos/demo/hello/issues/1"
jq --exit-status \
  --arg title "$issue_title" --arg body "$issue_body" \
  '.number == 1 and .title == $title and .body == $body' \
  "${work_dir}/restored-issue.json" >/dev/null
curl --fail --silent --show-error --output "${work_dir}/lfs-restored" \
  "${restore_origin}${lfs_path}"
cmp "${work_dir}/lfs-source" "${work_dir}/lfs-restored"

docker rm --force "$restore_proxy" "$restore_server" >/dev/null
"${compose[@]}" up --detach --no-build --wait --wait-timeout 120 server proxy
source_stopped=false
trap - EXIT

printf 'Complete-root restore qualified objects=%s git=%s issue=1 lfs=%s\n' \
  "$object_count" "$source_oid" "$lfs_oid"
