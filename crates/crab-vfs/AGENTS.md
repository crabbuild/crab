# crab-vfs

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-vfs/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns mount preparation, snapshots/overlays, filesystem adapters, and mount lifecycle. Canonical reconstruction stays in crab-read; FUSE/NFS translate that behavior into filesystem operations.

## Read first

1. `crates/crab-vfs/src/lib.rs` — mount feature gates and common contracts.
2. `crates/crab-vfs/src/pipeline.rs` — `MountPipelineBuilder::execute`: source through engine preparation.
3. `crates/crab-vfs/src/engine.rs` — `VfsEngine`: filesystem operation semantics.
4. `crates/crab-vfs/src/hydration.rs` — `HydrationService::spawn_workers / read_range`: queued reads and worker lifecycle.
5. `crates/crab-vfs/src/mount_runtime.rs` — `refresh_mount_runtime / switch_mount_runtime`: refresh/switch/adopt operations.

Trace one path: `MountPipelineBuilder::execute` in `crates/crab-vfs/src/pipeline.rs`
prepares `HydrationService` from `crates/crab-vfs/src/hydration.rs`; its read
path calls `ShardHydrator` in `crates/crab-read/src/hydrator.rs`. Backend
mount/control owners consume the pipeline output separately.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Prepare or refresh mount | `crates/crab-vfs/src/pipeline.rs` | `crates/crab-vfs/src/mount_runtime.rs` |
| Read windows | `crates/crab-vfs/src/hydration.rs` | `crates/crab-read/src/hydrator.rs` |
| Backend teardown | `crates/crab-vfs/src/nfs_mount.rs` | `crates/crab-vfs/src/mount.rs` |

## Invariants

- Start queue workers only after fallible preparation succeeds and the hydration
  service can pass directly to the owner. Daemon task startup belongs inside runtime
  installation, after backend setup. Inspect both pipeline and daemon paths.
- Separate pipeline preparation from backend mount/control lifetime; a built engine does not prove a mounted, usable filesystem.
  Source: `crates/crab-vfs/src/pipeline.rs`.
- Cached read windows retain integrity checks; corruption must not be served merely because the byte length matches.
  Source: `crates/crab-vfs/src/hydration.rs`.
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
- Review cancellation, hydration worker shutdown, leases, and control resources in both FUSE and NFS owners before changing teardown.
  Source: `crates/crab-vfs/src/nfs_mount.rs`.

## Features and platform

Empty default. `fuse` enables fuser; `nfs` enables NFS dependencies; `gix-facade` forwards crab-git/facade. Shared engine/pipeline/hydration modules require fuse or nfs. Run the corresponding fuse slice on a supported host with OS prerequisites; neither backend fixture checks nor default tests prove real mounting.

## Verification

Inline pipeline and hydration tests are feature-gated. Default crate tests do not compile those mount modules. Real mounted user-action proof belongs in `.github/workflows/nfs-mount.yml` or the appropriate FUSE environment.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
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
