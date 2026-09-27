#!/bin/sh
set -eu
case "${1:-}" in
  node) role="node-${CRAB_CELL_PERF_PROCESS_NODE:?}"; selected=process_performance::fleet_process_role ;;
  driver) role=driver; selected=process_performance::reference_compose_fleet_end_to_end_performance ;;
  scale) role=driver; selected=process_scaling::reference_compose_reader_scaling ;;
  rollout) role=rollout; selected=public_host::rollout::three_node_host_rustfs_additive_code_rollout ;;
  entities) role=entities; selected=entities::hosts::entity_ledgers_are_isolated_across_three_rustfs_hosts ;;
  entity-node) role="node-${CRAB_CELL_PERF_PROCESS_NODE:?}"; selected=entities::process::entity_process_node ;;
  entity-scale) role=driver; selected=entities::process::driver::entity_process_scaling ;;
  *) printf 'usage: run.sh node|driver|scale|rollout|entities|entity-node|entity-scale\n' >&2; exit 2 ;;
esac
binary=
for candidate in /target/release/deps/reference_application-*; do
  if [ -f "$candidate" ] && [ -x "$candidate" ]; then
    test -z "$binary" || { printf 'multiple test binaries; use a fresh target\n' >&2; exit 1; }
    binary=$candidate
  fi
done
test -n "$binary"
test ! -e "/evidence/$role.log" || { printf 'use a fresh evidence directory for each run\n' >&2; exit 1; }
mkdir -p /evidence/control
sha256sum "$binary" > "/evidence/$role-binary.sha256"
snapshot() {
  for counter in cpu.max cpu.stat memory.max memory.swap.max memory.peak memory.events; do
    printf '%s\n' "$counter"
    cat "/sys/fs/cgroup/$counter"
  done
}
snapshot > "/evidence/$role-kernel-before.txt"
read -r quota period < /sys/fs/cgroup/cpu.max
test "$quota" -eq "$period"
test "$(cat /sys/fs/cgroup/memory.max)" -eq 1073741824
test "$(cat /sys/fs/cgroup/memory.swap.max)" -eq 0
set +e
"$binary" --ignored --exact "reference_application::$selected" --nocapture \
  > "/evidence/$role.log" 2>&1
result=$?
set -e
snapshot > "/evidence/$role-kernel-after.txt"
cat "/evidence/$role.log"
test "$result" -eq 0
# An obsolete exact selector runs zero tests and still exits successfully.
grep -Eq 'test result: ok\. 1 passed; 0 failed;' "/evidence/$role.log"
