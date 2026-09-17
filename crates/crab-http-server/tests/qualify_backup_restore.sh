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
source_bucket="crab-http-server"
restore_bucket="crab-http-server-restore-${suffix,,}"
restore_server="crab-http-server-restore-${suffix}"
restore_proxy="crab-http-server-restore-proxy-${suffix}"
global_probe="repositories/.crab/http-server/v1/backup-probe/${suffix}"

if [ -z "$server_id" ] || [ -z "$proxy_id" ] || [ -z "$rustfs_id" ]; then
  echo "The Compose server, proxy, and RustFS services must be running." >&2
  exit 1
fi

cleanup() {
  result=$?
  docker rm --force "$restore_proxy" "$restore_server" >/dev/null 2>&1 || true
  if declare -p aws_cli >/dev/null 2>&1; then
    "${aws_cli[@]}" s3 rm "s3://${source_bucket}/${global_probe}" \
      --only-show-errors >/dev/null 2>&1 || true
    "${aws_cli[@]}" s3 rm "s3://${restore_bucket}/" --recursive \
      --only-show-errors >/dev/null 2>&1 || true
    "${aws_cli[@]}" s3api delete-bucket --bucket "$restore_bucket" \
      >/dev/null 2>&1 || true
  fi
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
qualification_tag="backup-restore-${suffix,,}"
printf 'v2 authority and activation records must survive restore.\n' \
  > "${source_dir}/${qualification_tag}.txt"
git -C "$source_dir" add "${qualification_tag}.txt"
git -C "$source_dir" commit -m "qualify complete v2 restore"
git -C "$source_dir" tag --annotate "$qualification_tag" \
  --message "Complete v2 restore qualification"
GIT_TERMINAL_PROMPT=0 git -C "$source_dir" push --atomic origin \
  main "refs/tags/${qualification_tag}"
source_oid="$(git -C "$source_dir" rev-parse HEAD)"
source_tag_oid="$(git -C "$source_dir" rev-parse "refs/tags/${qualification_tag}")"

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
lfs_key="repositories/demo/hello/lfs/objects/${lfs_oid:0:2}/${lfs_oid:2:2}/${lfs_oid}"
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
printf 'The root-scoped shared namespace must survive restore.\n' > "${work_dir}/global-probe"
"${aws_cli[@]}" s3 cp "${work_dir}/global-probe" \
  "s3://${source_bucket}/${global_probe}" --only-show-errors
"${aws_cli[@]}" s3api create-bucket --bucket "$restore_bucket" >/dev/null
"${aws_cli[@]}" s3 cp "s3://${source_bucket}/repositories/" \
  "s3://${restore_bucket}/repositories/" --recursive --only-show-errors
"${aws_cli[@]}" s3api list-objects-v2 --bucket "$source_bucket" \
  --prefix repositories/ --output json > "${work_dir}/source-objects.json"
"${aws_cli[@]}" s3api list-objects-v2 --bucket "$restore_bucket" \
  --prefix repositories/ --output json > "${work_dir}/restored-objects.json"
printf 'Copied complete configured storage root into isolated bucket %s\n' \
  "$restore_bucket"

jq --exit-status '(.IsTruncated // false) == false' \
  "${work_dir}/source-objects.json" "${work_dir}/restored-objects.json" >/dev/null
jq \
  '[.Contents[]? | {key: .Key, size: .Size}] | sort_by(.key)' \
  "${work_dir}/source-objects.json" > "${work_dir}/source-manifest.json"
jq \
  '[.Contents[]? | {key: .Key, size: .Size}] | sort_by(.key)' \
  "${work_dir}/restored-objects.json" > "${work_dir}/restored-manifest.json"
cmp "${work_dir}/source-manifest.json" "${work_dir}/restored-manifest.json"

jq --exit-status \
  'any(.[]; .key == "repositories/.crab/http-server/v1/catalog.json")' \
  "${work_dir}/source-manifest.json" >/dev/null
jq --exit-status --arg lfs_key "$lfs_key" \
  'any(.[]; .key == "repositories/demo/hello/v2/root") and
   ([.[] | select(.key | startswith("repositories/demo/hello/v2/refs/heads/"))] | length) >= 2 and
   any(.[]; .key | startswith("repositories/demo/hello/v2/capsules/")) and
   any(.[]; .key | startswith("repositories/demo/hello/v2/transactions/records/")) and
   any(.[]; .key | startswith("repositories/demo/hello/v2/transactions/committed/")) and
   any(.[]; .key == $lfs_key) and
   all(.[]; .key != "repositories/demo/hello/manifest" and
            .key != "repositories/demo/hello/layout")' \
  "${work_dir}/source-manifest.json" >/dev/null
jq --exit-status \
  'any(.[]; .key == "repositories/cells/v1/identity.json") and
   any(.[]; (.key | startswith("repositories/cells/v1/apps/")) and (.key | endswith("/control.json"))) and
   any(.[]; (.key | startswith("repositories/cells/v1/apps/")) and (.key | contains("/objects/")) and (.key | endswith(".root")))' \
  "${work_dir}/source-manifest.json" >/dev/null
jq --exit-status --arg probe "$global_probe" \
  'any(.[]; .key == $probe)' "${work_dir}/source-manifest.json" >/dev/null
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
    "s3://${source_bucket}/${key}")"
  restored_digest="$(object_digest \
    "s3://${restore_bucket}/${key}")"
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

[cells]
data_dir = "/var/lib/crab/cells"
local_disk_limit_bytes = 34359738368
peer_advertise = "https://localhost:8789"
peer_certificate = "/run/secrets/crab-peer/peer.crt"
peer_private_key = "/run/secrets/crab-peer/peer.key"
peer_ca = "/run/secrets/crab-peer/ca.crt"

[storage]
url = "s3://${restore_bucket}/repositories"
EOF
chmod 0644 "${work_dir}/restore.server.toml"

network_name="$(docker inspect "$rustfs_id" \
  --format '{{json .NetworkSettings.Networks}}' | jq --raw-output 'keys[0]')"
server_image="$(docker inspect "$server_id" --format '{{.Config.Image}}')"
proxy_image="$(docker inspect "$proxy_id" --format '{{.Config.Image}}')"
peer_identity_volume="$(docker inspect "$server_id" \
  --format '{{range .Mounts}}{{if eq .Destination "/run/secrets/crab-peer"}}{{.Name}}{{end}}{{end}}')"
if [ -z "$peer_identity_volume" ]; then
  echo "The Compose server peer identity volume could not be resolved." >&2
  exit 1
fi

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
  --tmpfs /var/lib/crab/cells:rw,noexec,nosuid,nodev,size=32g,uid=10001,gid=10001,mode=0700 \
  --volume "${work_dir}/restore.server.toml:/etc/crab/server.toml:ro" \
  --volume "${peer_identity_volume}:/run/secrets/crab-peer:ro" \
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
test "$(git -C "$restored_repository" rev-parse "refs/tags/${qualification_tag}")" \
  = "$source_tag_oid"
git -C "$restored_repository" fsck --strict
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
"${aws_cli[@]}" s3 rm "s3://${source_bucket}/${global_probe}" --only-show-errors
"${aws_cli[@]}" s3 rm "s3://${restore_bucket}/" --recursive --only-show-errors
"${aws_cli[@]}" s3api delete-bucket --bucket "$restore_bucket" >/dev/null
"${compose[@]}" up --detach --no-build --wait --wait-timeout 120 server proxy
source_stopped=false
trap - EXIT

printf 'Complete-root v2 restore qualified objects=%s git=%s tag=%s issue=1 lfs=%s\n' \
  "$object_count" "$source_oid" "$source_tag_oid" "$lfs_oid"
