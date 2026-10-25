# crab-vfs

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-vfs/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns mount preparation, snapshots/overlays, filesystem adapters, and mount lifecycle. Canonical reconstruction stays in crab-read; FUSE/NFS translate that behavior into filesystem operations.

## Read first

1. `crates/crab-vfs/src/lib.rs` — mount feature gates and common contracts.
2. `crates/crab-vfs/src/repository.rs` — shared Git clone/ref preparation, tag identity, index ownership and failure cleanup.
3. `crates/crab-vfs/src/pipeline.rs` — `MountPipelineBuilder::execute`: source through engine preparation.
4. `crates/crab-vfs/src/engine.rs` — `VfsEngine`: filesystem operation semantics.
5. `crates/crab-vfs/src/hydration.rs` — `HydrationService::spawn_workers / read_range`: queued reads and worker lifecycle.
6. `crates/crab-vfs/src/mount_runtime.rs` — `refresh_mount_runtime / switch_mount_runtime`: refresh/switch/adopt operations.

Trace one path: `MountPipelineBuilder::execute` in `crates/crab-vfs/src/pipeline.rs`
prepares `HydrationService` from `crates/crab-vfs/src/hydration.rs`; its read
path calls `PointerReader` in `crates/crab-read/src/pointer_reader.rs`, which
selects captured remote hydration or a retained local staging recipe. Backend
mount/control owners consume the pipeline output separately.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Clone or select Git ref | `crates/crab-vfs/src/repository.rs` | `crates/crab-vfs/src/clone_cache.rs`, pipeline and daemon callers |
| Prepare or refresh mount | `crates/crab-vfs/src/pipeline.rs` | `crates/crab-vfs/src/mount_runtime.rs` |
| Snapshot lifetime and attributes | `crates/crab-vfs/src/resolver.rs` | `crates/crab-vfs/src/snapshot.rs`, engine and adapter attribute callers |
| Read windows | `crates/crab-vfs/src/hydration.rs` | `crates/crab-read/src/hydrator.rs` |
| Ordinary Git reads and promotion | `crates/crab-vfs/src/blob.rs` | `crates/crab-vfs/src/engine.rs`, `crates/crab-vfs/src/memory_cache.rs` |
| Open FUSE files | `crates/crab-vfs/src/engine/open_file.rs` | `crates/crab-vfs/src/fuse.rs`, overlay lineage and native mount tests |
| Backend teardown | `crates/crab-vfs/src/nfs_mount.rs` | `crates/crab-vfs/src/mount.rs` |

## Invariants

- Start queue workers only after fallible preparation succeeds and the hydration
  service can pass directly to the owner. Daemon task startup belongs inside runtime
  installation, after backend setup. Inspect both pipeline and daemon paths.
- Separate pipeline preparation from backend mount/control lifetime; a built engine does not prove a mounted, usable filesystem.
  Source: `crates/crab-vfs/src/pipeline.rs`.
- `PipelineConfig.git_metadata` owns Git mutation policy. A remote data URL may
  accompany borrowed local metadata; a local path may feed an owned clone.
  Neither URL syntax nor Git's bare flag proves index/ref ownership.
  Sources: `pipeline.rs` and `mount_runtime.rs` under `crates/crab-vfs/src/`.
- Cached read windows retain integrity checks; corruption must not be served merely because the byte length matches.
  Source: `crates/crab-vfs/src/hydration.rs`.
- Publication and reads derive the same staging namespace through
  `repository::staging_root_for_git_dir`. Mounted versions keep distinct history
  owners; a later local commit must not retire earlier unpushed bytes.
  Sources: `crates/crab-vfs/src/publish.rs` and `crates/crab-read/src/pointer_reader.rs`.
- Remote-window requests own inflight claims through lock admission and cache
  publication. Cancellation drops the claim; retire its map entry only after
  the final claimant exits, so queued readers keep one lock.
  Source: `crates/crab-vfs/src/hydration.rs` (`ReadWindowClaim`).
- Chunk waiters subscribe before checking stored completion; a notification alone
  cannot represent a fetch that finished before subscription.
  Source: `crates/crab-vfs/src/hydration.rs` (`InflightEntry::wait`).
- Hydration task admission must serialize with shutdown. Await the service's
  `shutdown` after backend teardown and before releasing cache ownership; its
  tracker includes queue workers and read-window prefetch. A timed-out wait does
  not complete background work. Synchronous cleanup can only request shutdown.
  Source: `crates/crab-vfs/src/hydration.rs` and `crates/crab-vfs/src/coordinator.rs`.
- Retain refresh task handles and await them after cancellation. Refresh can
  await blocking Git/snapshot work; aborting the outer task does not finish it.
  Inspect coordinator, daemon, and interactive NFS owners together.
  Sources: `crates/crab-vfs/src/coordinator.rs`, `crates/crab-vfs/src/daemon.rs`,
  `crates/crab-vfs/src/nfs_mount.rs`.
- Read-pool pins belong to an entry lifetime, not just a reusable protocol file
  ID. Invalidation may retire active pins; their drops must not unpin a replacement.
  Source: `crates/crab-vfs/src/read_lease_pool.rs`.
- FUSE descriptors retain inode content independently of pathname lifetime.
  All opens share promotion; release/forget must preserve in-flight pins and
  never remove a replacement's path mapping. Take the inode content lock before
  the engine publication gate; controllers never take the inode lock. Namespace
  mutations resolve parent paths under the adapter namespace lock. Sources:
  `fuse.rs` and `engine/open_file.rs` under `crates/crab-vfs/src/`.
- Actual content edits clear disposable overlay source lineage. Otherwise a
  later base reconciliation may discard unpublished writes or truncate-to-zero.
  Sources: `overlay.rs` and both engine mutation paths.
- Live commits delegate to `mount_runtime::commit_mount_runtime`. The publisher
  must complete `CommitView` installation before clearing frozen backing files.
  Bind open files by committed device/inode incarnation, never pathname alone.
  Retain committed content identity, not its old reader; identical content resumes
  through the current captured catalog after push retires local staging recipes.
  Failed adoption keeps recoverable bytes; retry fingerprints include content.
  Post-commit COW and access upgrades replace native descriptors and their
  publication binding only after promotion succeeds; failure retains read/retry
  ownership. Prepare fallible descriptor clones before overlay registration.
  Unpublished base-pointer opens retain a canonical local recipe pin through
  push retirement. Release it only after successful native promotion or the last
  open/in-flight inode owner. It must not retain global staging admission.
  Sources: `publish.rs`, `mount_runtime.rs`, `engine/open_file.rs`.
- Linux commit/reset/refresh/switch controls and automatic refresh use
  `begin_mount_update` to drain client mapped writeback before
  the mutation gate; reversing that order blocks the WRITE callbacks needed by
  syncfs. Existing inactive/unmounted helpers have no client filesystem to drain.
  macOS client draining needs separate proof.
- Review cancellation, hydration worker shutdown, leases, and control resources in both FUSE and NFS owners before changing teardown.
  Source: `crates/crab-vfs/src/nfs_mount.rs`.
- Pointer windows retain immutable verified bytes with bounded payload and entry
  ownership. Do not reopen mutable VFS window files on warm hits; persistent
  cache integrity and repair stay in the shared read/cache crates.
- Ordinary blobs retain immutable bytes only after Git object-ID verification.
  Engine shutdown drains admitted Git loads/copies and hydration before releasing
  cache ownership. Cancellation cannot release a blocking operation's admission
  or reuse its unique promotion output path while that operation still runs.
- Resolver operations capture a pinned snapshot view; generation, timestamp,
  selected ref and captured pointer reader install together. Runtime catalog
  capture must succeed before publishing the next tree. Engine classification
  must update that captured generation.
  Exact backend attributes come from the engine without a second path resolution.
  Pins protect only publishers sharing the same `SnapshotStore`; remote Capsule
  retention and direct/offline publication routing require separate proof.
- Admitted control tasks retain guards, blocking jobs and invalidation through
  completion even when callers stop waiting. Shutdown closes admission and drains
  controls before read resources. Owned jobs must not reacquire a coordinator
  lock held by shutdown. Sources: `engine.rs`, `mount_runtime.rs`, `ipc_server.rs`.
- Live inspection uses the helper/coordinator's actual overlay and mode.
  Flush client writeback before quiescing mutations; inspection does not change
  epochs or invalidate open readers. Keep the cross-process freeze through the
  metadata read. Never redirect a known owner's failed exchange to saved state.
  Sources: `mount_control.rs`, `mount_runtime.rs`, both backend control owners.
- NFS helper unmount completion requires kernel detachment and the live owner's
  cache lease. Retain the acquired lease through owner-conditional registry
  removal. Single/batch callers propagate live-control errors; only a helper
  confirmed dead before sending may select stale cleanup. Sources: `nfs_control.rs`,
  `clone_cache.rs`, `mounts_registry.rs`, and `crab/src/cmd/mount.rs`.
- NFS control deadlines cover connect/write/read on a request-owned socket.
  Timeout closes that socket; it does not roll back a helper mutation. Do not
  retry an uncertain commit. Source: `crates/crab-vfs/src/nfs_control.rs`.
- FUSE IPC exchanges temporarily own their connection. Restore it only after a
  valid response; cancellation or errors must close it so a late response cannot
  satisfy a later request. Source: `crates/crab-vfs/src/ipc_client.rs`.
- IPC clients must not unlink coordinator socket paths. The daemon lock holder
  owns stale cleanup; only missing/refused connections permit spawn or retry.
  Sources: `crates/crab-vfs/src/ipc_client.rs` and
  `crates/crab-vfs/src/coordinator.rs`.

## Features and platform

Empty default. `fuse` enables fuser; `nfs` enables NFS dependencies; `gix-facade` forwards crab-git/facade. Shared engine/pipeline/hydration modules require fuse or nfs. Run the corresponding fuse slice on a supported host with OS prerequisites; neither backend fixture checks nor default tests prove real mounting.

## Verification

Inline pipeline and hydration tests are feature-gated. Default crate tests do not compile those mount modules. Real mounted user-action proof belongs in `.github/workflows/nfs-mount.yml` or the appropriate FUSE environment.

Ignored Linux `native_mount` tests perform real FUSE/NFS OS operations. Run only
in a dedicated environment with FUSE, the native NFS client and mount privileges:
`cargo test -p crab-vfs --locked --lib --features fuse,nfs native_mount:: -- --ignored --nocapture --test-threads=1`,
with the external `CARGO_TARGET_DIR` rule below. These tests use an in-memory
origin with shard hints; Capsule/RustFS qualification remains a separate gate.
Mapped-transition probes also require Python 3 for a separate native client;
the server's Git subprocesses must not inherit client descriptors or mappings.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-vfs --locked --lib --features nfs repository::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-vfs --locked --lib --features nfs pipeline::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-vfs --locked --lib --features nfs hydration::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-vfs/README.md` — usage and detailed contracts.
- `crates/crab-vfs/Cargo.toml` — dependency and feature authority.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
