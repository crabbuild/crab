#!/bin/sh
set -eu

case "${1:-}" in
  single) selected=rustfs_single_worker_reports_sparse_read_interference ;;
  paired) selected=rustfs_sparse_reads_report_worker_interference ;;
  *) printf 'usage: run-worker-profile.sh single|paired\n' >&2; exit 2 ;;
esac

binary=
for candidate in /target/release/deps/crab_cell_runtime-*; do
  if [ -f "$candidate" ] && [ -x "$candidate" ]; then
    if [ -n "$binary" ]; then
      printf 'multiple runtime test binaries; use a fresh qualification directory\n' >&2
      exit 1
    fi
    binary=$candidate
  fi
done
test -n "$binary"
sha256sum "$binary" > "/evidence/$1-binary.sha256"

snapshot() {
  for counter in cpu.max cpu.stat memory.max memory.swap.max memory.peak memory.events; do
    printf '%s\n' "$counter"
    cat "/sys/fs/cgroup/$counter"
  done
}
snapshot > "/evidence/$1-kernel-before.txt"
read -r quota period < /sys/fs/cgroup/cpu.max
test "$quota" -eq "$period"
test "$(cat /sys/fs/cgroup/memory.max)" -eq 1073741824
test "$(cat /sys/fs/cgroup/memory.swap.max)" -eq 0

set +e
"$binary" --ignored --exact "cell::worker::tests::$selected" --nocapture \
  > "/evidence/$1.log" 2>&1
result=$?
set -e
snapshot > "/evidence/$1-kernel-after.txt"
cat "/evidence/$1.log"
test "$result" -eq 0
grep -Eq 'test result: ok\. 1 passed; 0 failed;' "/evidence/$1.log"
grep -q 'worker-interference {' "/evidence/$1.log"
