# Strict batch owner discovery checkpoint

Source parent `9ce740639df`; base `9cc0fdab60a`. The adjacent checkpoint JSON
binds source and installed binaries; source-before-build.json matches the final
production/test sources byte-for-byte. No dependency/lockfile changes.

Before: a coordinator-only FUSE mount is omitted by unmount --all. The retained
checkpoint-25 FUSE-only diagnostic is supplemented here by a mixed native RustFS
probe on its installed roles: exit0/Unmounted1, NFS gone, FUSE still mounted.
The finally block explicitly cleans the remaining FUSE mount. Five initial CLI
regressions and direct VFS listing fail before the repair. Later registry cases
extend that fixture; the before output is not described as an eight-case run.

After: control_targets gathers registered helpers plus coordinator members through
one strict List validator shared with individual FUSE admission. Display status
metadata is separate; batch does not issue NFS status probes. Rejected/incomplete
inventory propagates. Missing/refused coordinator connections permit absence.
Lost known live owners, conflicting backends, changed registered identities and
unowned live processes cannot select PID cleanup. Stale cleanup requires the
same recorded dead owner. Daemon/private ownership and native admission racing
stale force cleanup remain separate gates.

Final checks: 639 Linux VFS tests; strict all-target VFS Clippy and FUSE-only check.
Linux/macOS each pass 24 CLI unmount tests, including eight new batch cases.
macOS additionally passes 13 VFS control tests and strict VFS Clippy. The broad
CLI expected-format assertion remains unapplied; no full CLI success claim.

Final Make-installed roles are NFS-only main/NFS helper and combined FUSE helper.
FUSE-only empty-registry unmount succeeds. Three mixed RustFS rounds unmount both
backends: initial writable, writable remount preserving bytes, and read-only
hiding saved overlay bytes. Each reports2, detaches both native mounts, clears
registered ownership and permits immediate nonblocking acquisition of both
cache leases: six assertions without retries/waits. Rejected single/batch NFS
controls preserve the owner PID/registry and original rejection.

A separate live-list comparison uses the same two actual mounts and alternating
old/new client order, discarding three warmup pairs. All fields except uptime
match. Ten retained samples each have medians8.32ms/8.59ms; debug CLI latency
only, not optimized data-plane throughput/tail qualification. Payload is retained
as a hash; no private endpoint/credential values are needed in the report.

The task-owned Colima profile was confirmed stopped before resumed qualification.
Restart reused the same profile and existing RustFS containers. The failed launch
occurred before compilation; tests-resumed-linux.txt is completed final proof.
No other VM/target/repository was changed. Make uses the existing bundled linker
and retains14 existing CLI feature-slice warnings. Linux ELF version text has
unknown Git SHA because the source bind cannot resolve its host worktree pointer;
source and binary hashes bind the evidence instead.

Remaining: export materialization ownership, teardown/admission/cancellation,
other product owners, Windows cache locking, stored pack-v1 retirement, and full
performance qualification. Per-target membership revalidation and registry scans
still need large-batch measurement. No all-mode production-readiness claim.
