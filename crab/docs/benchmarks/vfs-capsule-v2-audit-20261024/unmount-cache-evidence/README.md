# NFS cache handoff checkpoint

Source parent `b1d94af8e3a`, base `9cc0fdab60a`. Adjacent checkpoint JSON binds
source, installed binaries and retained inputs/outputs by SHA-256.

The cache-lease CLI regression fails before the fix. The previous checkpoint's
original NFS mode cycle records immediate remount failing with cache-already-in-use.
The helper now reports actual cache identity on accepted shutdown. Completion
requires kernel detachment plus acquisition of that exclusive cache lease,
retained through owner-conditional registry cleanup. Stale cleanup also preserves
replacement registry owners. NFS helpers never write the FUSE PID file.

Installed fault probes show the previous CLI terminating the test owner and
masking a rejected shutdown error, for both single and batch unmount. Final CLI
keeps the owner and registry intact and reports the original error. Explicitly
dead helpers retain stale cleanup. No retry follows an uncertain live exchange.

Final qualification: 638 Linux VFS tests, strict combined all-target VFS Clippy,
FUSE-only check; 16 CLI unmount tests on Linux and macOS. macOS also passes 12
control, 14 cache and 20 registry tests and strict VFS Clippy. The earlier Linux
pass has all 16 native probes passing before enum boxing; engine lifetime code
is unchanged. That pass's Clippy large-enum warning was repaired by boxing the
NFS context. A failed new macOS fixture used an unregistered path alias; the
canonical fixture passes. Neither failed attempt is counted as product proof.

Final Make installation has separate NFS-only main/NFS helper and combined FUSE
helper. Both backends complete ten immediate writable/read-only cycles against
RustFS. Every unmount return is followed immediately by nonblocking cache-lease
acquisition, with no wait or retry: 40 mounted transitions and 40 lease assertions.
Writable bytes survive and read-only diff remains rejected. These are debug
correctness probes, not optimized performance qualification.

A separate FUSE batch diagnostic fails: unmount --all returns success with No
active mounts while the coordinator mount remains mounted. Its finally block
uses single unmount to clean up. This pre-existing discovery gap is the next
repair; do not count it as passing or claim all-mode readiness.

The initial GNU-linker install was stopped after the final cleanup edit invalidated
its source snapshot. Only install-final-linux.txt is completed installation proof.
The final install uses the existing bundled linker; no dependency override or
repository target cleanup. Make feature slices retain 14 existing CLI warnings.
The broad CLI expected-format assertion patch remains unapplied. Linux source
binds cannot resolve the host Git worktree pointer, so ELF version text has unknown
Git SHA; source and binary hashes bind the evidence instead.

Unix enforces the cache lease. The inherited non-Unix lock implementation remains
a no-op and Windows handoff is unqualified. Native NFS child-task completion,
daemon/private owners, export generation ownership, remaining pack-v1 removal
and full data-plane performance qualification remain open.
