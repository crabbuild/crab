#!/usr/bin/env bash
set -euo pipefail

bucket="${1:?usage: hash_backup_restore_objects.sh bucket source-prefix restored-prefix keys-file}"
source_prefix="${2:?source prefix is required}"
restored_prefix="${3:?restored prefix is required}"
keys_file="${4:?keys file is required}"
endpoint="${AWS_ENDPOINT_URL_S3:?AWS_ENDPOINT_URL_S3 is required}"
if [ ! -s "$keys_file" ]; then
  echo "Object key manifest is missing or empty: ${keys_file}" >&2
  exit 1
fi

hash_object() {
  local uri="$1"
  local digest
  if ! digest="$(
    aws --endpoint-url "$endpoint" s3 cp "$uri" - --only-show-errors \
      | sha256sum | cut --delimiter=' ' --fields=1
  )"; then
    echo "Could not hash object ${uri}." >&2
    return 1
  fi
  if [[ ! "$digest" =~ ^[0-9a-f]{64}$ ]]; then
    echo "Object hash was invalid for ${uri}." >&2
    return 1
  fi
  printf '%s' "$digest"
}

verify_object() {
  local key="$1"
  local source_digest
  local restored_digest
  if [ -z "$key" ]; then
    echo "Backup manifest contains an empty object key." >&2
    return 1
  fi
  source_digest="$(hash_object "s3://${bucket}/${source_prefix}/${key}")"
  restored_digest="$(hash_object "s3://${bucket}/${restored_prefix}/${key}")"
  if [ "$source_digest" != "$restored_digest" ]; then
    echo "Restored object differs from source: ${key}" >&2
    return 1
  fi
  printf 'verified\n'
}

export bucket source_prefix restored_prefix endpoint
export -f hash_object verify_object
verified="$(
  # The child shell expands the positional key bound by xargs.
  # shellcheck disable=SC2016
  xargs --delimiter=$'\n' --max-args=1 --max-procs=8 \
    /bin/bash -euo pipefail -c \
      'verify_object "$1" || exit 255' _ \
    < "$keys_file" \
    | wc --lines | tr --delete '[:space:]'
)"

printf '%s\n' "$verified"
