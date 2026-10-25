# VFS / Capsule v2 audit — 2026-10-24

**Verdict: fails qualification. Neither complete filesystem correctness nor
performance across all mount modes is established.** Real Capsule/RustFS
refresh fails, native FUSE loses an open descriptor after rename, and focused
probes expose cache integrity and local repository state failures.

Audited `origin/crab-v2` at `33f8e6c360d922b3ab1e2c0e5115a2d200eccd97`.
Fetched `origin/main` separately: `cc2e4e25f46eeaee3cfd30025d257397c17a6049`.
The working checkout was clean before investigation. Production sources were
restored after diagnostic probes; the accompanying patch preserves the probes.
No fixes, dependency changes, commits, or branch pushes are included.

## Executed evidence

[Exact-commit NFS workflow](https://github.com/crabbuild/crab/actions/runs/40108750432).
This was dispatched on `crab-v2`; its recorded SHA matches the audited commit.
[Status snapshot](vfs-capsule-v2-audit-20261024/ci-results.json) records the
observation time and outstanding jobs.

| Check | Result | Meaning |
| --- | --- | --- |
| Linux CI NFS feature gate | Pass: 489 VFS tests, 2 coordinator response tests, CLI feature check and script checks | Compile and fixture coverage; does not establish native correctness |
| Linux CI native NFS smoke | Pass | Native client, local Git files, overlay/control operations |
| Linux CI Capsule/RustFS/Xet E2E | **Fail** | Newly pushed file returns EIO after successful manual refresh |
| macOS CI native NFS smoke | Pass | Native local Git/overlay/control smoke; does not cover Capsule refresh |
| Windows CI native NFS smoke | Still running at audit close | Not qualified; consult linked workflow for its eventual result |
| macOS pipeline tests, NFS feature | 15 passed | Pipeline and fail-closed hydration construction |
| macOS hydration tests, NFS feature | 35 passed | Existing window, shutdown, range and worker tests |
| macOS engine tests, combined features | 51 passed | Existing engine tests, diagnostic probes excluded |
| macOS NFS adapter tests, combined features | 64 passed | Paging, stable writes, rename, leases and validation |
| macOS FUSE adapter tests, combined features | 8 passed | Existing handle, flag and invalidation tests |
| macOS minimal crate, no backend features | Compile check passed | Feature-free public surface |
| macOS FUSE-only feature slice | 8 passed | FUSE-only adapter compilation/tests |
| macOS shared hydrator tests | 42 passed | Canonical reconstruction, range, integrity, failure and cancellation slices |
| macOS Capsule reader tests | 54 passed | Captured catalog, visibility, dependency and range-reader slice |
| Added focused diagnostic probes | **8 failed**, 1 measurement passed | See individual failures below |
| Isolated Linux native backend probes | **3 passed, 1 failed** | FUSE read-only and NFS read-only/writable pass; writable FUSE fails after rename |

The native diagnostic probes use real Git objects, encoded shards/xorbs, the
shared hydrator, actual FUSE/NFS mounts and ordinary OS file operations.
Their object-store origin is in memory, and their pointer includes a shard
hint. They **do not** establish Capsule catalog lookup or hosted-provider
performance. The separate CI RustFS test exercises the real Capsule product
path and fails.

Evidence: [focused probe output](vfs-capsule-v2-audit-20261024/probes-macos.txt),
[native backend output](vfs-capsule-v2-audit-20261024/native-linux.txt),
[Capsule refresh output](vfs-capsule-v2-audit-20261024/capsule-refresh.txt),
[reproduction patch](vfs-capsule-v2-audit-20261024/probes.patch).

## Findings

### P1 — refreshed Git state keeps the old Capsule catalog

**Real E2E failure.** The RustFS smoke successfully pushes a new 8 MiB file.
`crab mount refresh` reports the new HEAD, but `sha256sum` of the newly visible
file fails with `Input/output error`. The job stops there; subsequent writable
refresh, publication and independent-clone checks are not reached.

`crab/src/cmd/mount.rs:2073` captures a root and creates a pinned lookup through
`SharedFileIndexLookup::for_pointer_catalog`. That dependency explicitly never
widens to newer state (`crates/crab-metadata/src/file_index_lookup.rs:856`).
`crates/crab-vfs/src/mount_runtime.rs:82` publishes a new snapshot, reconciles
the overlay and updates the resolver without replacing hydration. Automatic
refresh has the same ownership gap in `crates/crab-vfs/src/refresh.rs:818`.
Hintless pointers require lookup through that old catalog
(`crates/crab-read/src/store_client.rs:278`).

The runtime failure and the source path agree; retained CI artifacts do not
include a helper-side reconstruction backtrace. The stale-catalog explanation
is source-supported, rather than an instrumented helper trace.

Both NFS and FUSE share this hydrator. The required repair is a jointly owned
Git revision/catalog generation transition, preserving old readers' captured
view until completion. Reopening mutable latest metadata on every read would
violate the dependency contract. Startup capture races, switch, daemon refresh,
commit adoption and GC retention need the same treatment.

### P1 — read-only local mounting rewrites the source Git index

**Reproduced.** Stage a changed `base.txt`; record `git rev-parse :base.txt`;
execute a read-only local mount pipeline. The staged OID changes back to HEAD.
The working file remains, but the user's selected staged version is discarded.

`crates/crab-vfs/src/pipeline.rs:216` unconditionally calls
`run_read_tree_head` with the source `.git` directory. Its callee executes
`git read-tree HEAD` (`crates/crab-vfs/src/refresh.rs:448`). Overlay read-only
mode does not protect the source index. Both standalone backends consume this
pipeline; legacy foreground FUSE also does so.

Probe: `audit_readonly_local_mount_preserves_staged_index`.
Use mount-owned index/worktree state where required, rather than modifying a
local source index to prepare a view. This defect also exists on fetched main.

### P1 — initial mounting ignores the selected local ref

**Reproduced.** With source HEAD on `main`, request `refs/heads/other`.
Pipeline output still reports main's commit. The documented local branch-view
command therefore displays the wrong revision.

`crates/crab-vfs/src/pipeline.rs:296` calls `resolve_head` without consulting
`PipelineConfig.ref_name`. Reused remote clones also ignore the newly requested
branch in `crates/crab-vfs/src/clone_cache.rs:187`. Both standalone adapters
consume this output. Daemon branch cloning is a separate path and must not be
assumed equivalent.

Probe: `audit_initial_pipeline_honors_requested_ref`. Bind snapshot selection
and any owned Git index to the requested revision without switching the source
worktree. The initial local-ref defect also exists on fetched main.

### P1 — a local pointer commit becomes unreadable after overlay cleanup

**Reproduced through real staging, Git commit and snapshot adoption.** Create
and read a `filter=crab` overlay file, commit without push, adopt the committed
snapshot, then read again. Commit succeeds; the read fails with
`not found: file_index:...`.

`crates/crab-vfs/src/publish.rs:454` stages content and writes a pointer.
`crates/crab-vfs/src/publish.rs:1578` intentionally emits no shard hint.
Commit without push then finalizes/cleans the overlay
(`crates/crab-vfs/src/publish.rs:567`). The shared mount hydrator is built only
from remote storage (`crab/src/read/mod.rs:73`); it has no staging read path.
`adopt_published_snapshot` changes the visible tree but not that read ownership
(`crates/crab-vfs/src/mount_runtime.rs:130`).

Probe: `audit_local_pointer_commit_remains_readable`. This affects the shared
publication path for both backends. Preserve readable ownership of unpublished
content across commit/cleanup; adding hints cannot make unuploaded content
available at origin. The relevant VFS publication code is also on fetched main.

### P1 — native FUSE breaks an open descriptor after rename

**Native Linux reproduction; NFS sibling passes.** Open a created file for
read/write, write and fsync, rename it, and successfully read the new pathname.
Seek and read through the original still-open descriptor: FUSE returns ENOENT.
The same sequence succeeds over native NFS.

FUSE stores the path on each open handle
(`crates/crab-vfs/src/fuse.rs:239`). Rename updates only the inode table
(`crates/crab-vfs/src/fuse.rs:1318`). A stale read lease is reopened using the
handle's old path (`crates/crab-vfs/src/fuse.rs:989`); writes also use that old
path (`crates/crab-vfs/src/fuse.rs:1093`). NFS instead updates its file-ID path
mapping, covered by existing rename tests and the native sibling probe.

Probe: `audit_native_fuse_writable`. Open-file lifetime must be independent of
pathname lifetime, including unlink and replacement. Updating strings for
rename alone is not a complete POSIX fix. This code is also on fetched main.

### P1 — warm caches can return corrupted data successfully

**Two independent reproductions.** Warm a pointer window, overwrite its bytes
with different bytes of the same length, then read: successful corrupted data.
Warm an ordinary Git blob, overwrite its cache entry, then range-read: the same
failure.

The warm-window shortcut checks only length after a key was once verified
(`crates/crab-vfs/src/hydration.rs:687`). The blob cache trusts the named file
without verifying Git identity (`crates/crab-vfs/src/engine.rs:217`). Both paths
sit above canonical verified reconstruction, so lower-layer integrity checks
cannot protect these hits. Both adapters are affected.

Probes: `audit_warm_read_rejects_same_length_corruption` and
`audit_blob_cache_corruption_is_not_served`. The existing
`cached_window_rejects_same_length_corruption` passes because it calls the
verifier directly, bypassing the production warm-hit shortcut. Both failures
exist on fetched main.

### P2 — disposable window-cache failure breaks healthy origin reads

**Reproduced.** Replace the window-cache root with a regular file. A canonical
read with healthy origin data fails with `NotADirectory`.

`crates/crab-vfs/src/hydration.rs:625` propagates cache metadata errors;
`crates/crab-vfs/src/hydration.rs:671` makes cache publication required for
success. This differs from the optional chunk-cache contract and the shared
xorb cache's verified-origin behavior. Both adapters are affected.

Probe: `audit_window_cache_failure_does_not_break_origin_read`. Consolidate
cache failure/integrity ownership; do not add another fallback stack. This
failure exists on fetched main.

### P2 — cancellation retains window inflight entries after shutdown

**Reproduced.** Hold a real reconstruction at availability admission, cancel
the foreground read, await its task and service shutdown. The window inflight
map still contains its entry.

Insertion precedes await at `crates/crab-vfs/src/hydration.rs:635`; removal at
`crates/crab-vfs/src/hydration.rs:677` runs only when the future completes normally. There is no drop cleanup
or entry budget. Repeated abandoned distinct windows can accumulate entries.

Probe: `audit_cancelled_window_read_releases_inflight_entry`. Both adapters use
this path; background-task shutdown does not own canceled foreground futures.
This code is also on fetched main.

### P2 — ordinary Git range reads repeatedly load the whole blob

**Measured, with a source-level amplification proof.** Read a warm 4 MiB Git
blob in 1,024 requests of 4 KiB. Three runs took 167, 152 and 151 ms; direct
file-range reads took 16, 12 and 11 ms. This is approximately 10–14 times slower
on this unoptimized macOS test build.

`crates/crab-vfs/src/engine.rs:320` calls `read_blob` then slices; warm hits use
`std::fs::read` of the entire blob. Serving 4 MiB logically therefore performs
4 GiB of userspace read/copy work, not necessarily physical disk I/O because
the OS cache is warm. Each cold local-ODB cache installation additionally scans
all cached files for size accounting (`crates/crab-vfs/src/engine.rs:275`),
making repeated cold fills grow quadratically in cache entry count.

Probe: `audit_blob_range_read_amplification`. Both adapters are affected.
Bounded verified range access and owned accounting are needed. This code is
also on fetched main. These measurements are not remote throughput benchmarks.

### P2 — FUSE serializes callbacks and eagerly materializes directories

**Source and locked dependency proof.** `crates/crab-vfs/src/mount.rs:345`
leaves `fuser::Config.n_threads` unset. Locked fuser 0.17.0
(`crates/crab-vfs/Cargo.toml:32`) uses `config.n_threads.unwrap_or(1)` in
`Session::run`; it rejects multiple dispatch threads outside Linux. FUSE reads
then block that dispatcher on hydration (`crates/crab-vfs/src/fuse.rs:981`).
An uncached remote read therefore blocks unrelated callbacks on that mount.
The comments describing a configured thread pool do not match this setup.

`crates/crab-vfs/src/fuse.rs:745` also materializes all directory children and
allocates inode references on every opendir. `releasedir` releases the entry
vector, not those references; ordinary readdir entries do not create kernel
lookup references under fuser's forget contract. NFS's bounded cursor paging
is a distinct, better-covered sibling surface. No large-directory native FUSE
RSS/latency qualification was performed here.

Address callback task ownership and bounded admission across supported
platforms; setting a thread count alone cannot solve macOS. The relevant code
is also on fetched main.

### P2 — directory prefetch silently does no canonical data work

**Reproduced.** Enqueue a pointer child through `prefetch_dir`, process the
queued task, and observe zero xorb body reads.

`crates/crab-vfs/src/hydration.rs:974` returns success immediately whenever
the canonical read hydrator exists. That is precisely the real object-store
mount configuration. Both adapters still enumerate/enqueue directory children;
the queue workers discard that work without warming data.

Probe: `audit_directory_prefetch_warms_canonical_data`. Adaptive sequential
read-window prefetch is a separate working path; this finding concerns
list-then-open directory prefetch. Either implement it through the canonical
bounded path or remove the work and its documented claim. Also on fetched main.

### P2 — foreground FUSE does not install a refresh task

**Source-confirmed mode gap; no command-level native refresh probe.**
`crab/src/cmd/mount.rs:693` mounts the foreground FUSE session and enters its
cancellation loop. It never calls `spawn_refresh_loop` or installs the
coordinator/control owner. The builder's `with_no_refresh` value does not start
a task in `execute`. Coordinator and daemon callers have separate refresh
owners. A foreground mount therefore stays on its initial snapshot despite
refresh not being disabled. The same omission is present on fetched main.

## Performance evidence and limits

The CI native sequential benchmark reads a 4 MiB ordinary Git blob twice with
256 KiB application reads. Retained measurements:

| Client | Reported throughput | Server reads / application bytes | Lease reuse |
| --- | --- | --- | --- |
| Linux | 135.3 MiB/s | 4 MiB / 8 MiB; the second pass benefits from client caching | 15 hits, 1 miss |
| macOS, client cache disabled | 68.0 MiB/s | 32 MiB / 8 MiB; 4× server-returned byte amplification | 31 hits, 1 miss |

These are different client-cache conditions, not a matched OS comparison.
Neither benchmark reads a Crab pointer (`base_pointer_reads = 0`). They do not
qualify Capsule/Xet performance, large models, contention or tail latency.
[Retained measurement extract](vfs-capsule-v2-audit-20261024/measurements.json).

Before its refresh failure, the real RustFS case completed the seed push,
mounted in a 751 ms readiness phase, and verified the 32 MiB seed pointer's
whole-file SHA-256, four requested ranges and read-only rejection behavior in
a 482 ms verification phase. Those are mixed phase durations, not isolated
throughput measurements. This establishes useful initial-read integration;
it does not erase the subsequent refresh failure.

## Capsule format assessment

The VFS correctly delegates stored-format decoding instead of implementing its
own Capsule parser. The inspected format authority includes `CRBROOT2` with
root envelope version 4 (`crates/crab-metadata/src/capsule_protocol/root.rs:11`),
`CRBCAPS2` version 2, capsule runs, layered checkpoints and authenticated pointer
catalogs. The pointer marker remains `https://crab.build/spec/v1`
(`crates/crab-types/src/pointer.rs:5`); that is the pointer format and is not a
Capsule-v2 incompatibility.

Remote mount setup captures a control view through `open_git_view_from_root`,
keeps catalog pages cold, pins the resulting lookup and configures the shared
hydrator. A supplied object-store layout without that hydrator fails closed
(`crates/crab-vfs/src/pipeline.rs:692`). Range verification proves selected
chunks and exact output length; whole-file reconstruction separately proves
the complete file hash (`crates/crab-read/src/hydrator.rs:199`).

The 54 Capsule reader and 42 hydrator tests passed. Those dependency contracts
are useful evidence, but the mount's catalog lifecycle does not remain aligned
with its changing Git snapshot. The real E2E failure prevents format-integration
signoff.

## Coverage and remaining gates

| Surface | Entry and owner | Downstream contract | Current proof / gap |
| --- | --- | --- | --- |
| Standalone NFS foreground/helper | `crab/src/cmd/mount.rs:538`, `crates/crab-vfs/src/nfs_mount.rs:183` | Pipeline → engine → shared hydrator | Native Linux local-file smoke passes; Capsule refresh fails |
| FUSE coordinator | `crates/crab-vfs/src/mount_control.rs`, `crates/crab-vfs/src/coordinator.rs` | Same pipeline/engine; FUSE session owns dispatch | Fixture coverage; native adapter failure; complete coordinator Capsule run missing |
| Foreground FUSE | `crab/src/cmd/mount.rs:693` | Same engine and session | Native adapter read proof; foreground refresh owner missing |
| Named daemon NFS/FUSE | `crates/crab-vfs/src/daemon.rs:748` | Separate startup composition and shared hydration/refresh | Source traced; daemon native scripts exist but are not jobs in this dispatched workflow |
| Read-only / writable | Pipeline plus backend adapter | Overlay admission, read leases, Git publication | Native local NFS both pass; FUSE writable fails; local read-only index mutation and pointer commit fail |
| Refresh / switch / commit adoption | `crates/crab-vfs/src/mount_runtime.rs` and `refresh.rs` | Git snapshot must match immutable catalog generation | Real refresh failure; other transitions need regression coverage |
| Teardown / cancellation | Coordinator, daemon, NFS runtime, HydrationService | Backend requests finish before cache ownership release | Existing worker tests pass; foreground inflight cleanup fails; NFS child-task completion unproven |

Additional release gaps:

- Locked `nfs3_server` 0.11.0 spawns connection handlers and its cleaner without
  exposing join handles. Crab aborts/joins the listener; that alone does not
  prove all child work ended. Verify active requests, stalled clients, unstable
  writes and cache-lock reacquisition after teardown. The crate README already
  documents this limitation.
- Window and ODB byte caches have no enforced disk budget/eviction. Bounded
  key sets, the NFS lease pool and decoded-range cache do not bound these files.
  Qualify disk-full behavior, private path/symlink handling, RSS and long-lived
  mounts under churn.
- Complete the provider/mode matrix: cold/warm/missing/corrupt caches, parallel
  random/sequential reads, large directories, local and remote sources,
  read-only/writable foreground/helper/coordinator/daemon, switch, reset,
  commit with/without push, archive restore, replica failover, GC and unmount
  during reads. Hosted S3/GCS/Azure cases were not executed in this audit.
- Establish performance targets and retained baselines. The small native
  diagnostic fixture is not a throughput, tail-latency or memory acceptance
  test. No all-mode performance verdict can be drawn from its timings.

## Reproduction and repair order

The diagnostic patch adds tests only and intentionally fails against the
audited sources. Apply it in a clean disposable checkout. Set a unique external
`CARGO_TARGET_DIR` under the mounted workspace for every compiling invocation.
Replace `your-worktree` in these examples and verify that target is writable
before running the commands.

```sh
git apply crab/docs/benchmarks/vfs-capsule-v2-audit-20261024/probes.patch
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-audit-your-worktree" \
  cargo test -p crab-vfs --locked --lib --features fuse,nfs audit_ -- --skip audit_native:: --nocapture
```

On a dedicated Linux environment with FUSE, native NFS client, mount privileges
and no unrelated mounts, enable the native probes explicitly:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-audit-your-worktree" \
  CRAB_VFS_AUDIT_NATIVE=1 cargo test -p crab-vfs --locked --lib --features fuse,nfs audit_native:: -- --nocapture --test-threads=1
```

The native harness uses Crab's FUSE teardown wrapper. An earlier harness
revision called fuser's `umount_and_join` directly and stalled; that harness
mistake is excluded from product findings. The final reproduction includes
successful cleanup and pinpoints the writable descriptor failure.

Repair shared ownership before backend-specific patches:

1. Preserve local source Git state; select the requested revision correctly.
2. Couple snapshot, captured catalog and unpublished-content ownership across
   startup, refresh, switch and commit adoption. Preserve captured-view rules.
3. Give open FUSE files identity/lifetime independent of names.
4. Consolidate verified optional byte-cache ownership, bounded storage and
   cancellation cleanup. Preserve range reuse without trusting mutable files.
5. Bound FUSE dispatch/directory work, remove ineffective prefetch work, and
   install consistent foreground refresh ownership.
6. Rerun these reproductions, then the complete native/provider/performance
   matrix. A green fixture suite alone cannot close this audit.

Relevant product documentation:
[VFS mounts](https://crab.build/docs/cli/virtual-filesystem/fuse-mount),
[mount modes](https://crab.build/docs/cli/virtual-filesystem/mount-modes).
