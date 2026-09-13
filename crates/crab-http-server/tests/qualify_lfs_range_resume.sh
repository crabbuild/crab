#!/usr/bin/env bash
set -euo pipefail

base_url="${1:-http://127.0.0.1:8788}"
work_root="${RUNNER_TEMP:?RUNNER_TEMP must name disposable qualification storage}/crab-http-server-lfs-range"
source_file="${work_root}/source.bin"
download_file="${work_root}/download.bin"
first_headers="${work_root}/first.headers"
resume_headers="${work_root}/resume.headers"
head_headers="${work_root}/head.headers"
fallback_file="${work_root}/fallback.bin"
error_body="${work_root}/range-error.json"

mkdir -p "${work_root}"
dd if=/dev/zero of="${source_file}" bs=1024 count=1024 status=none
size=$(wc -c < "${source_file}" | tr -d '[:space:]')
oid=$(openssl dgst -sha256 < "${source_file}" | awk '{ print $NF }')
object_url="${base_url}/git/demo/hello.git/info/lfs/objects/${oid}?size=${size}"
first_end=393215

curl --fail --silent --show-error \
  --request PUT --data-binary "@${source_file}" "${object_url}"
curl --fail --silent --show-error \
  --range "0-${first_end}" \
  --dump-header "${first_headers}" \
  --output "${download_file}" \
  "${object_url}"

test "$(wc -c < "${download_file}" | tr -d '[:space:]')" -eq "$((first_end + 1))"
grep --extended-regexp --ignore-case '^HTTP/[0-9.]+ 206([[:space:]]|$)' "${first_headers}"
grep --extended-regexp --ignore-case '^accept-ranges: bytes[[:space:]]*$' "${first_headers}"
grep --extended-regexp --ignore-case \
  "^content-range: bytes 0-${first_end}/${size}[[:space:]]*$" "${first_headers}"
grep --extended-regexp --ignore-case \
  "^etag: \"${oid}\"[[:space:]]*$" "${first_headers}"

curl --fail --silent --show-error \
  --continue-at - \
  --dump-header "${resume_headers}" \
  --output "${download_file}" \
  "${object_url}"

grep --extended-regexp --ignore-case '^HTTP/[0-9.]+ 206([[:space:]]|$)' "${resume_headers}"
grep --extended-regexp --ignore-case \
  "^content-range: bytes $((first_end + 1))-$((size - 1))/${size}[[:space:]]*$" \
  "${resume_headers}"
cmp "${source_file}" "${download_file}"

curl --fail --silent --show-error \
  --head --header 'Range: bytes=0-0' \
  --dump-header "${head_headers}" --output /dev/null \
  "${object_url}"
grep --extended-regexp --ignore-case '^HTTP/[0-9.]+ 200([[:space:]]|$)' "${head_headers}"
grep --extended-regexp --ignore-case '^accept-ranges: bytes[[:space:]]*$' "${head_headers}"
grep --extended-regexp --ignore-case "^content-length: ${size}[[:space:]]*$" "${head_headers}"
if grep --extended-regexp --ignore-case '^content-range:' "${head_headers}"; then
  echo 'HEAD unexpectedly applied the Range header' >&2
  exit 1
fi

status=$(curl --silent --show-error \
  --header "Range: bytes=${size}-" \
  --output "${error_body}" --write-out '%{http_code}' \
  "${object_url}")
test "${status}" = "416"
grep --fixed-strings '"message":"LFS byte range is not satisfiable"' "${error_body}"

status=$(curl --silent --show-error \
  --header 'Range: bytes=0-0,2-2' \
  --output "${fallback_file}" --write-out '%{http_code}' \
  "${object_url}")
test "${status}" = "200"
cmp "${source_file}" "${fallback_file}"
