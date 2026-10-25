# Live overlay inspection checkpoint

Base `9cc0fdab60a`; source parent `2a6aa267be6`. The adjacent checkpoint JSON
binds final source, installed binaries and retained inputs/outputs by SHA-256.

Before: a writable NFS mount creates unpublished bytes; a read-only remount hides
them, but the old CLI diff reports them and export copies them. FUSE refuses.
`before-export-nfs.txt` records this failure; `before-rw-bind-fuse.txt` records
the sibling policy. The CLI lost-owner regression also fails before its fix.

After: both owners route inspection through the shared runtime. Client writeback
precedes the exclusive non-mutating engine gate and process-spanning overlay
freeze. Live read-only policy rejects diff/export; known owner loss fails closed.
Reader epochs are preserved. CLI summary polling and read-side WAL checkpointing
are removed. Export materialization itself still needs a separate ownership audit.

Proof: 637 Linux VFS tests; 16 native FUSE/NFS probes; strict all-target VFS
Clippy on Linux/macOS; two macOS VFS tests and two CLI context tests. Installed
NFS-only main plus combined FUSE helper pass six RustFS 4 MiB mapped edit →
refresh/switch/automatic → commit/push → independent eager-clone hash flows.
`mapped-*-result.json` and corresponding output record exact hashes.

The original immediate NFS mode cycle fails: unmount returns before its helper
releases mutable cache ownership; the next helper reports cache-already-in-use.
`after-nfs.txt` and `immediate-remount-failed-helper.txt` retain that failure.
A separate inspection harness uses an explicit cache-lease-ready precondition
between modes. It observes a 111 ms first NFS wait after unmount. Do not count this
as a passing immediate-remount gate or a product retry fix.

Paired warm diff timing uses identical structured data against the same helper:
10 retained samples after warmup. Medians: NFS 201→61 ms, FUSE 154→40 ms. Debug
CLI latency only; no throughput, optimized performance or all-mode claim.

Scripts require the task-owned Colima image/network, fixture volume, installed
checkpoint-23 binary prefix and final installed prefix. External Cargo state is
worktree-specific on Workspace; product binaries were installed by Make.
Full CLI tests are not claimed: the pre-existing expected-format assertion patch
remains unapplied. Make feature slices emit 14 existing warnings. The worktree
bind cannot resolve its host Git pointer inside Linux, so ELF version output has
unknown Git SHA; hashes bind the source and installed binaries instead.
