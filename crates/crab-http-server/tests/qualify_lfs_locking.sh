#!/usr/bin/env bash
set -euo pipefail

base_url="${1:-http://127.0.0.1:8788}"
work_root="${RUNNER_TEMP:?RUNNER_TEMP must name disposable qualification storage}/crab-http-server-lfs-locking"
repository="${work_root}/client"
locks_url="${base_url}/git/demo/hello.git/info/lfs/locks"
lfs_url="${base_url}/git/demo/hello.git/info/lfs"

git lfs version
test ! -e "${work_root}"
mkdir -p "${repository}/models"
git -C "${repository}" init --initial-branch=main
git -C "${repository}" config user.name "Crab qualification"
git -C "${repository}" config user.email "qualification@example.invalid"
git -C "${repository}" lfs install --local
git -C "${repository}" remote add origin "${base_url}/git/demo/hello.git"

printf '%s\n' '*.bin filter=lfs diff=lfs merge=lfs -text lockable' \
  > "${repository}/.gitattributes"
printf '%s\n' 'first revision' > "${repository}/models/team.bin"
git -C "${repository}" add .gitattributes models/team.bin
git -C "${repository}" commit --message "Add lockable LFS object"
GIT_TERMINAL_PROMPT=0 git -C "${repository}" push --set-upstream origin main

git -C "${repository}" config "lfs.${lfs_url}.locksverify" true
git -C "${repository}" config --local --get-regexp \
  '^lfs\..*\.locksverify$' | grep --extended-regexp '[[:space:]]true$'

git -C "${repository}" lfs lock models/team.bin \
  | grep --fixed-strings 'Locked models/team.bin'
curl --fail --silent --show-error \
  --header 'Accept: application/vnd.git-lfs+json' \
  "${locks_url}?path=models%2Fteam.bin" \
  | jq --exit-status \
    '.locks | length == 1 and .[0].path == "models/team.bin" and .[0].owner.name == "operator"'

printf '%s\n' 'second revision' > "${repository}/models/team.bin"
git -C "${repository}" add models/team.bin
git -C "${repository}" commit --message "Update our locked LFS object"
GIT_TERMINAL_PROMPT=0 git -C "${repository}" push origin main

git -C "${repository}" lfs locks \
  | grep --fixed-strings 'models/team.bin'
git -C "${repository}" lfs unlock models/team.bin \
  | grep --fixed-strings 'Unlocked models/team.bin'
curl --fail --silent --show-error \
  --header 'Accept: application/vnd.git-lfs+json' \
  "${locks_url}" \
  | jq --exit-status '.locks == []'
