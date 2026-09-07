# crab-vfs

`crab-vfs` exposes Crab repositories as a virtual filesystem. It combines a
local or cloud-backed Git source, snapshot and overlay state, the canonical
read/hydration path, and a filesystem engine. FUSE and NFS integrations are
feature-gated so applications that only need source parsing or shared mount
contracts do not inherit platform-specific runtimes.

## Why it exists

A mounted repository must feel like a normal filesystem even though many file
contents are lazy Xet pointers. The VFS needs a stable Git tree snapshot,
write overlay semantics, background hydration, cancellation, refresh, and
consistent read-after-write behavior. This crate owns that integration rather
than putting filesystem lifecycle policy into the storage or Xet layers.

## Architecture

```text
MountSource
  ├── local .git / bare repository
  └── remote crab://, s3://, gs://, az:// source
          │
          ▼
blobless Git clone or local Git directory
          │
snapshot SQLite + overlay reconciliation + read-tree
          │
crab-read hydrator + shared chunk cache
          │
resolver (snapshot + overlay) → VFS engine → FUSE or NFS
```

`MountPipelineBuilder::execute` runs the preparation pipeline: source clone or
reuse, HEAD resolution, snapshot, overlay setup, reconciliation, index
population, hydration construction, resolver creation, and engine wiring. Only
after those steps succeed does it start workers and return their owning service. Mounting
and refresh are lifecycle operations performed outside the pipeline so a
daemon, coordinator, or foreground CLI can own cancellation.

The `fuse` feature enables the FUSE session and mount lifecycle; `nfs` enables
the NFS server path; `gix-facade` enables the optional Git facade integration.
FUSE/NFS deployments also need the corresponding operating-system support.

The auxiliary chunk cache is optional: unsafe or unavailable storage yields a
non-storing handle, not a mount-startup failure or an alternative cache root.
`crab-cache` owns private directory creation and chunk-identity validation;
bad chunk records are removed before origin repair. The synchronous chunk
adapter bypasses current-thread Tokio runtimes, where `block_in_place` would
panic. Whole-pointer reconstruction continues through the configured
`crab-read` runtime and its decoded-range cache.

Writable mount publication currently has repository-local Git state but no
resolved storage identity at the pointer-write boundary. It therefore omits
the optional shard hint instead of consulting another bucket or managed view's
cache row; hydration retains the file-index fallback. Product composition can
thread a resolved storage scope here in a later ownership consolidation.

This does not yet qualify the separate file-window cache in `hydration.rs`:
its failure isolation, per-read verification, private filesystem access, and
budget/lifetime ownership remain Plan 017 work. Startup and in-memory-origin
tests are not native mounted-filesystem or whole-process resource proof.

## Protocol read leases

NFS has no file-open/file-close lifecycle, so `ReadLeasePool` retains bounded
leases between READ requests. A pin protects its cached entry from ordinary
budget eviction while a request uses it. Refresh and mutations may explicitly
invalidate entries; existing reads keep their owned lease until completion.
Pins identify an entry lifetime as well as a file ID, so a late release cannot
unpin a replacement inserted after invalidation.

The macOS native smoke requests uncached sequential reads with `F_NOCACHE`
and records `client_cache: disabled` in its benchmark artifact. This exercises
server lease reuse even when kernel read-ahead could fetch the fixture at once.
Its throughput is not directly comparable with older kernel-cached runs.

## Task ownership

| Task | Handle owner | Completion boundary |
| --- | --- | --- |
| Hydration queue workers and read-window prefetch | `HydrationService`, retained by the mount owner | Await `shutdown()` after backend teardown; queued work is discarded and admitted prefetch finishes |
| Ref/snapshot refresh | Coordinator, daemon, or interactive NFS runtime | Cancel polling and await the task, including any admitted blocking Git/snapshot work |
| NFS server and control | NFS mount runtime | Backend teardown controls these separately from hydration |

The daemon starts hydration and refresh tasks only when installing them into a
successfully mounted runtime. Engine or backend setup failure therefore starts
no such workers. Native backend tasks have their own cleanup boundaries.

Background admission and task registration share a lock. `shutdown()` closes
admission, cancels queue workers, and awaits their shared task tracker. Repeated
or concurrent shutdown calls observe the same completion boundary. Blocking
hydration and cache writes can delay completion; shutdown does not abort them.

The coordinator warns after ten seconds of hydration shutdown and continues
waiting with mount/cache ownership intact. Refresh owners cancel polling and
join the task; aborting it could detach a blocking Git fetch. Daemon teardown
separately aborts and joins its watcher. Synchronous coordinator shutdown and
Drop request cancellation but cannot await completion; use the async path.
Foreground request ownership and native teardown still require backend proof.

The locked `nfs3_server` listener also spawns connection handlers and a transaction
cleaner without exposing join handles. Listener drop notifies the cleaner, but
neither that notification nor joining the listener proves child-task completion.
Native unmount and backend task cleanup need separate verification.

## Control exchange ownership

The FUSE coordinator client reuses its connection only after a complete, valid
response, including an application error. One timeout covers request writes and
the response read. During I/O the exchange owns the connection; cancellation,
timeout, or a transport/parse failure closes it. Further sends return
`NotConnected`. Reconnect explicitly and determine a mutation's outcome before
deciding whether to retry it. Connection setup/spawning has a separate policy.

Only a missing or refused connection triggers coordinator startup. Permission,
invalid-path, and other connection errors return their original cause. Clients
never unlink the socket path; stale cleanup belongs to the coordinator holding
the daemon lock. The startup retry budget does not bound an individual connect.

### NFS control deadlines

Each control call owns a fresh socket. Its timeout covers connection setup,
request writes, and the response read: ten seconds normally, thirty minutes for
commit. TCP and Unix sockets share the same JSON exchange. Timeout or caller
cancellation drops the socket; the client does not retry a mutation whose result
is unknown. A timeout does not prove that the helper stopped an admitted commit.

These are asynchronous I/O deadlines, not CPU preemption or a deadline for the
native OS mount command. See `nfs_control::tests` for stalled-write and socket
closure regressions.

## Usage

Source detection requires `fuse` or `nfs`. This crate is not published to the
registry; enable the backend in a consuming Crab workspace member:

```toml
[dependencies]
crab-vfs = { workspace = true, features = ["fuse"] }
```

```rust
use crab_vfs::source::MountSource;

fn example() -> Result<(), Box<dyn std::error::Error>> {
    let source = MountSource::parse("crab://models/team/project")?;
    assert!(matches!(source, MountSource::Remote { .. }));

    let local = MountSource::parse("./working-copy")?;
    assert!(matches!(local, MountSource::Local { .. }));
    Ok(())
}
```

For a real mount, construct a `PipelineConfig`, run
`MountPipelineBuilder::new(config).execute()`, and hand its engine/resolver to
the selected FUSE or NFS lifecycle. Use a `CancellationToken` and shut down
hydration workers before releasing the snapshot, overlay, or Git resources.

## Boundaries

- [`crab-read`](../crab-read/README.md) owns verified pointer reconstruction;
  VFS supplies it with mount context.
- [`crab-git`](../crab-git/README.md) owns repository URL and Git mechanics;
  VFS owns mount lifecycle and filesystem presentation.
- [`crab-cache`](../crab-cache/README.md) owns reusable chunk/cache contracts;
  VFS owns per-mount integration and invalidation.
