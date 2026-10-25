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
reuse, selected-ref resolution, snapshot, overlay setup, reconciliation, owned
index population, hydration construction, resolver creation, and engine wiring. Only
after those steps succeed does it start workers and return their owning service.

`repository` owns Git clone/ref preparation for the pipeline, daemon and control
operations. Cached clones fetch newly selected refs; reconciliation preserves
unpublished branch commits and retains annotated-tag object identities separately
from their peeled snapshot commits. Fresh clones prepare in an owned temporary
directory and publish only after success. A failed clone never recursively removes
an existing destination. Clone/discovery commands clear ambient repository context.

`PipelineConfig.git_metadata` declares whether metadata is borrowed or owned by
the mount. Borrowed repositories preserve their index and HEAD, even when the
read context uses a remote data origin. Owned clones fetch refs and populate their
own index, including clones of local sources. Missing borrowed metadata is an
error; preparation never replaces it with a clone. Snapshot and blob reads use linked worktrees' common object
directory; local polling follows selected, packed, linked or detached refs through
`crab-git`. Client-relative source paths are normalized before crossing IPC.
Remote-helper fetches install into that common directory too, including lazy
policy-blob fetches from the linked worktree used by overlay publication.

Mounting and refresh are lifecycle operations performed outside the pipeline so a
daemon, coordinator, or foreground CLI can own cancellation.

The resolver captures one retained snapshot view for lookup, attributes and
directory paging. Generation, commit timestamp, selected ref and captured pointer
reader install together. Base read
leases and copy-on-write work retain that view, and late Git blob classification
updates the captured generation rather than whichever generation is current.
Both adapters obtain exact attributes through the engine; NFS directory listing
can still omit attributes for an unknown blob instead of hydrating it eagerly.

Snapshot pruning skips pinned generations and reclaims them on a later
publication after their final owner exits. Pins coordinate callers sharing one
`SnapshotStore`; every live publisher must use the mount owner's store. The NFS
and coordinator commit controls do this. Direct/offline publication routing,
startup Git/Capsule capture and remote GC-root retention remain separate hardening
gates; local snapshot pins do not prove those contracts. Runtime transitions
capture the next catalog before publishing the tree. A failed capture leaves the
live view unchanged. In-flight reads retain their captured reader; starting a new
read on an older protocol lease still returns the generation-stale error.
FUSE open inodes separately retain captured content for their descriptor lifetime.

Manual and automatic transitions share one per-mount control admission boundary.
An admitted task owns its reset guard and blocking jobs even if its caller stops
waiting. Shutdown closes admission and drains control tasks before read resources.
Queued automatic polls verify that their starting view is still current before
publication. Manual branch selection therefore cannot be undone by an older poll.
Hydration services for successive views share caches, admission and shutdown;
capturing a new catalog does not create an independent resource budget.

On Unix, either backend enables `ipc_client` and the unchanged `ipc_protocol`
wire types. An NFS-only main CLI can control an existing FUSE coordinator without
linking its server. Coordinator bootstrap and server ownership require `fuse`.
Missing/refused coordinator sockets permit absence; other exchange failures
propagate and must not redirect live mutations to offline publication.

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

Writable publication flushes local staging before writing pointer blobs and
publishes distinct history owners for unpushed versions. The shared
`crab-read::PointerReader` reads those retained recipes after overlay cleanup,
including after remount without a remote read context. Copy-on-write uses the
same verified local source. Published files use their captured remote catalog;
unavailable staging cannot shadow that publication. The pointer-write boundary
still omits optional shard hints because it has no resolved storage identity.
It never consults another bucket or managed view's hint cache row.
An open FUSE base pointer outside its captured remote catalog retains a local
recipe pin. Confirmed push may retire published path leases while that pin
preserves readable bytes. The pin releases after successful native promotion
or the last open/in-flight inode owner; failed promotion retains it. Pins hold
metadata ownership rather than global staging admission, allowing later writes.
Physical payload reclamation remains deferred to staging maintenance.
The publisher and remote helper derive local staging from the selected common
Git directory. Push state shares that owner even when a helper starts from an
unrelated repository with only GIT_DIR set.

Read-window remote counters measure reconstruction through the configured store
runtime, which can itself use caches. They exclude local staged reconstruction;
actual origin traffic requires the storage-layer request/byte counters.

For readers configured with a store, `hydration.rs` retains verified read-through
windows as immutable owned bytes,
with at most 64 MiB of payload allocation and 128 entries per mount. Least
recently used windows are evicted; small replies copy only their requested
bytes so they do not retain an evicted window. Hits perform no filesystem I/O.
Persistent pointer caches stay in the canonical shared reader. Old VFS window
files carry no read authority and are neither opened nor migrated. Window hit
and miss counters describe this memory cache. The cap excludes admitted
reconstructions, active replies and lower-layer caches; it is not a whole-process
memory bound. Native/provider performance and resource qualification remain open.
Local staging without a remote runtime keeps exact requested ranges and skips
remote-window prefetch, avoiding amplification of small local reads.

Ordinary Git blobs use `blob.rs`: decoded bytes are checked against their Git
object ID before serving or retaining them. A separate immutable memory cache
retains at most 128 MiB and 4,096 entries. Warm range replies copy only the
requested bytes; they do not read complete disk blobs or scan cache files.
Old VFS disk-blob files have no read authority. Native Git still retrieves
missing promisor objects, with replacement refs disabled and returned bytes
verified. Cold async reads run blocking decoding/fetching outside Tokio workers.
The retained-cache cap does not bound all upstream decoding allocations, active
responses, or total process memory; native Git fetch has no cancellation deadline.

Overlay promotion writes into a unique owned temporary file and installs it only
after reconstruction/copy completes. A cancelled writer cannot reuse a retry's
output path. Ordinary-blob copies retain the verified allocation throughout
writing; promotion never reopens an unchecked disk-blob cache entry.

## Protocol read leases

FUSE retains one shared content owner per open inode in `engine/open_file.rs`.
Base files keep their captured snapshot/catalog; overlay files keep native
descriptors rather than reopening names. Rename moves the inode's binding;
unlink/replacement detaches it. A detached write updates only retained content
and cannot recreate a former pathname. Readers and writers of the same inode
share copy-on-write promotion. Writable mounts use direct I/O for readers too,
so the kernel cannot retain base pages after another descriptor promotes it.
Linux requests `FUSE_DIRECT_IO_ALLOW_MMAP` when the kernel advertises it; shared
mmap of direct-I/O files otherwise returns ENODEV. See the
[kernel I/O contract](https://kernel.org/doc/html/latest/filesystems/fuse/fuse-io.html).
Flush/fsync use the retained descriptor and propagate failures. Operation pins
outlive final release/forget until in-flight work finishes; closing the last
owner releases its native descriptors even if lookup references remain.

Post-commit promotion retains the old native descriptors and publication binding
until reconstruction, backing installation and overlay registration succeed.
Failed promotion leaves existing reads and retry ownership intact. Access upgrades
use that same transition; opening another descriptor cannot discard usable content
before its replacement is ready. Upgrading access after cleanup can therefore
materialize a fresh backing file even before the new descriptor writes.

Namespace mutation resolves parent paths under the adapter's namespace lock.
No-replace rename checks destination absence inside the engine's mutation/path
locks. Exchange and whiteout return EINVAL rather than performing an ordinary
rename. Content edits clear disposable base lineage, preserving unpublished
writes and truncation through reconciliation. Active descriptors across commit
cleanup/adoption, refresh and reset still require lifecycle qualification.

Descriptor content changes and path truncation run through the overlay owner's
`mutate_content` boundary. Its shared publication lease covers native I/O and
metadata recording together; a frozen overlay rejects admission before bytes
change. Zero truncation of a base file records local ownership with its empty
promotion, without a separate metadata update window.

Live NFS/coordinator commits use `commit_mount_runtime` and the publisher's
`CommitView` boundary. Authentication and view installation finish before backing
cleanup. Weak open-file bindings match the exact committed device/inode to its
new base content identity; subsequent writes promote matching content through the
current captured reader. A clean push can retire local recipes without changing
the file, so retaining its pre-push catalog would break later promotion. Failed
adoption preserves the overlay for retry. Publication fingerprints
hash file content, since matching size/mtime cannot authorize discarding newer
bytes. Directory records carry namespace only; each tracked child owns its data.

Linux commit/reset/refresh/switch controls and automatic refresh drain the client
filesystem with
[`syncfs`](https://man7.org/linux/man-pages/man2/syncfs.2.html) before
taking the engine mutation gate, allowing mapped writeback callbacks to finish.
The synthetic `.git` file identifies that filesystem without enumerating the
root directory. macOS client-cache draining and other product-mode owners remain
qualification gates. Path-based reads reacquire a lease invalidated before I/O;
explicit lease callers retain their stale-lease result.

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
| Ordinary Git loads and promotion copies | `OdbReader`, retained by `VfsEngine` | `VfsEngine::shutdown_background()` closes admission and drains admitted blocking operations before cache ownership is released |
| Ref/snapshot refresh | Coordinator, daemon, or interactive NFS runtime | Cancel polling and await the task, including any admitted blocking Git/snapshot work |
| NFS server and control | NFS mount runtime | Backend teardown controls these separately from hydration |

The daemon starts hydration and refresh tasks only when installing them into a
successfully mounted runtime. Engine or backend setup failure therefore starts
no such workers. Native backend tasks have their own cleanup boundaries.

Background admission and task registration share a lock. Engine shutdown closes
Git and hydration admission and waits for both owners. Hydration shutdown also
cancels queue workers. Repeated or concurrent waits observe the same completion
boundary. Blocking Git fetches, hydration and writes can delay completion;
shutdown does not abort them. Synchronous cleanup can only request shutdown.

The coordinator warns after ten seconds of hydration shutdown and continues
waiting with mount/cache ownership intact. Refresh owners cancel polling and
join the task; aborting it could detach a blocking Git fetch. Daemon teardown
separately aborts and joins its watcher. Synchronous coordinator shutdown and
Drop request cancellation but cannot await completion; use the async path.
Remote-window requests own inflight claims through mutex admission and cache
publication. Cancellation releases the claim; queued readers retain the same
lock until the final claimant exits. Foreground request ownership and native
teardown still require backend proof.

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

## Native verification

The ignored `native_mount` tests require a dedicated Linux environment with
FUSE, the native NFS client and mount privileges. They perform OS file operations
through real mounts, including selected-ref/source-index checks, canonical
pointer reads and writable descriptor lifetime checks. Their in-memory origin
and shard hint do not establish Capsule catalog or hosted-provider qualification.
With a unique writable target on the mounted workspace volume, run:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-your-worktree-linux" \
  cargo test -p crab-vfs --locked --lib --features fuse,nfs native_mount:: \
  -- --ignored --nocapture --test-threads=1
```

## Boundaries

- [`crab-read`](../crab-read/README.md) owns verified pointer reconstruction;
  VFS supplies it with mount context.
- [`crab-git`](../crab-git/README.md) owns repository URL and Git mechanics;
  VFS owns mount lifecycle and filesystem presentation.
- [`crab-cache`](../crab-cache/README.md) owns reusable chunk/cache contracts;
  VFS owns per-mount integration and invalidation.
