# crab-ltx

Embedded SQLite WAL capture and exact LTX recovery, with an optional canonical
Cell root transport. Crab-owned integration of Celld's mechanics; no Celld Git
dependency or Litestream daemon. Default features remain empty. Enable
`replica` for the existing `crab-storage` transport and Tokio integration.

Status: local capture and canonical Cell root preparation are implemented.
Native and Cell-scoped bundle LTX cuts can be prepared as immutable
Cell/incarnation-scoped roots, and exact range/full compaction can produce a
representation-only prepared root.
These roots are bound to checked `crab-cell-runtime` control successors and the
runtime is composed by `crab-http-server`. Initial directories are constructed
from a streaming k-way index merge and uploaded one radix leaf at a time.
Writable Cell activation now streams authenticated checksums to a local
fixed-width file and capture updates it incrementally. Cell compaction now uses
disk-spooled authenticated indexes, a k-way external merge, bounded frame reads
and multipart uploads from the injected filesystem. The complete product
cutover and measured capacity qualification still remain. The former
standalone epoch-head/paged/scheduler API was hard-removed under the recorded
compatibility decision; its stored prefixes are never read as Cell roots. See
[next architecture](../crab-http-server/next-architecture/README.md).

## Contract

| API | Local result |
| --- | --- |
| `ManagedDb::open(path, limits)` | Exclusive fresh capture session; owns control, read-lock and application-writer SQLite connections, each configured with a 64 KiB page-cache target |
| `ManagedDb::{resume,resume_with_host}(plan, path, limits, …)` | New local session continuing an exact verified TXID/checksum; available without `replica` |
| `transaction(closure)` | One locally committed SQL transaction; no remote-durability claim |
| `capture()` | Ordered `CaptureBatch` containing every newly generated cut and its endpoint, including checkpoint cuts |
| `checkpoint(mode)` | Capture barrier plus PASSIVE/FULL/RESTART/TRUNCATE; returns every generated cut |
| `snapshot(path)` | Returns `(LocalSegment, CaptureBatch)`: full `1..=txid` snapshot plus every newly captured cut |
| `VerifiedLocalPlan::new(files, target, limits)` | Owns verified bytes of an explicitly selected snapshot-plus-deltas chain |
| `restore_exact(plan, path)` | Installs a new SQLite file at exactly the verified endpoint; never overwrites |
| `compact_exact(plan, path)` | Compacts that complete local chain into a verified snapshot; never deletes inputs |
| `Host::with_local_disk_budget(DiskBudget)` | Shares byte-precise WAL/LTX/sparse-page admission across cloned hosts; exhausted write admission occurs before SQL begins |
| `Host::install_disk_admission(...)` | Reconciles every local-disk reserve, resize, release, and late host installation with the embedding runtime's node ledger |
| `Host::with_scratch_monitor(ScratchMonitor)` | Rechecks embedding-service disk pressure after process-wide full-job scratch admission and before remote body downloads |
| `close()` | Releases local connections/read lock; does not upload, publish or release a remote lease |

`SegmentInfo` includes TXID range, page size/count, pre/post rolling checksum,
encoded size and BLAKE3 digest. Persist those expectations in the server's
authenticated manifest. `LocalSegment::new` is an **unverified selection**;
`VerifiedLocalPlan::new` validates it before recovery. Changing a path after plan
construction cannot change its owned bytes. LTX CRC64 checks file structure and
database state; it is not cryptographic authentication.

The first file must be a full snapshot. Every subsequent range starts at the
previous maximum TXID plus one. Validation rejects gaps, overlaps, missing files,
wrong digests, wrong metadata/target, invalid page order/index offsets, missing
snapshot or growth pages, and checksum-disabled files. Every applied cut's
rolling database checksum is verified, not just the final trailer.

Snapshot capture transfers ownership of pending cuts just like `capture()` and
`checkpoint()`. When preparing a Cell root, retain and publish the returned
batch through `CellReplica`; do not append the full snapshot to an existing
delta chain:

```rust,no_run
# #[cfg(feature = "replica")]
# async fn snapshot_publication(writer: &mut crab_ltx::ManagedDb, replica: &crab_ltx::CellReplica,
#     base: Option<&crab_ltx::RootRef>, snapshot_path: &std::path::Path) -> crab_ltx::Result<()> {
let (snapshot, pending) = writer.snapshot(snapshot_path)?;
let prepared = replica.prepare(base, &pending, 1, 1).await?;
// `snapshot` is an independent full recovery artifact, not another delta.
let _root = prepared.root();
# Ok(())
# }
```

Writers emit checksum-bearing LTX v3 **sized-block** files (LTX v0.5.2 layout).
Readers accept both sized-block and older LZ4-frame files when checksummed.
Litestream v0.5.11 cannot read the sized-block layout; compatibility must not be
inferred from the unchanged file-version number. Independent local vectors test
both encodings; a full external Litestream/Celld interoperability matrix remains
a release gate. Source revision and notices: [UPSTREAM.md](UPSTREAM.md).

## Use

```rust,no_run
use crab_ltx::{Limits, ManagedDb, VerifiedLocalPlan, restore_exact};
use std::path::Path;

# fn example() -> crab_ltx::Result<()> {
// Parent directories already exist, are private, and are exclusively owned.
let limits = Limits::default();
let mut db = ManagedDb::open(Path::new("cell/repository.sqlite"), limits)?;
db.transaction(|tx| {
    tx.execute("CREATE TABLE issues (number INTEGER PRIMARY KEY, title TEXT)", [])?;
    tx.execute("INSERT INTO issues VALUES (1, 'First issue')", [])?;
    Ok(())
})?;
let captured = db.capture()?;

// Server integration goes here: upload immutable files, publish a manifest and
// prove the owner/head CAS before responding. Capture alone is not publication.
let plan = VerifiedLocalPlan::new(&captured.segments, captured.position, limits)?;
restore_exact(&plan, Path::new("recovery/repository.sqlite"))?;
db.close()?;
# Ok(())
# }
```

`ManagedDb::transaction_with` preserves typed application failures separately
from SQLite and WAL-boundary ambiguity. `ManagedDb::query_with` temporarily
enables SQLite `query_only` for one synchronous callback and fences the session
if that boundary cannot be installed or removed. This is a trusted-code
guardrail, not a sandbox or a replacement for the runtime's scoped authorizer.

Runnable demonstration, from the repository root with this worktree's external
Cargo target directory configured:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" \
  cargo run -p crab-ltx --example local_roundtrip --locked
```

It writes an issue to real SQLite, copies LTX artifacts to another local
directory, deletes the original database directory, restores and queries the
issue. This demonstrates local mechanics, **not RustFS publication**.

The canonical RustFS Cell example covers object-store publication, sparse
activation, compaction, source deletion, and exact recovery. See the
[examples guide](examples/README.md).


## Object-store Cell roots and sparse SQL

Enable the `replica` feature. Construct a `crab_storage::Store` with Crab’s
existing provider builders and a `CellStorageLayout`; there is no second
credential or S3 parser.

| API | Result |
| --- | --- |
| `CellReplica::new(layout, cell, incarnation, limits)` | Binds every immutable object to one typed Cell incarnation and rejects staged stores |
| `prepare(base, cuts, sequence, schema).await` | Verifies native LTX cuts, writes content-addressed objects and directory nodes, and returns a private `PreparedRoot` for authority CAS |
| `prepare_bundle(base, bundle, sequence, schema).await` | Selects `BundleEntry::for_cell` rows for this Cell, verifies the chain, and prepares the advancing root |
| `prepare_compaction(base, range, level, scratch).await` | Performs bounded authenticated range compaction and returns a representation-only prepared root |
| `open_root(root).await` | Reopens one exact root and validates Cell scope, chain, metadata, and the authenticated directory |
| `VerifiedRoot::paged().read_page(page).await` | Walks the selected hash-pinned directory path, range-reads one frame, and verifies BLAKE3, page number, and checksum |
| `VerifiedRoot::paged().prepare_writable(path).await` | Streams authenticated checksums to a fresh sidecar and returns a root-bound writable activation |
| `CellWritableDatabase::open_writable(path)` | Creates a fresh sparse SQLite file and seeds exact TXID/checksum continuation |
| `ManagedDb::{hydration,hydrate_step,take_io_error}` | Reports and advances bounded hydration through the same VFS as foreground SQL |
| `ManagedDb::prune_captured(batch)` | Re-verifies and removes only the exact local capture batch after its root is durably acknowledged |
| `bundle::Bundle` | Validates CRB1 ranges used by Cell recovery overlays; it does not publish mutable authority |

The runtime calls `Control::publish_prepared` and `CellAuthority` to bind a
prepared root to owner, incarnation, sequence, and response durability. A
`PreparedRoot` or uploaded object alone is not publication.

Cell objects live under
`cells/v1/apps/<app>/cells/<cell>/inc/<inc>/objects/`. Root JSON is canonical
compact v1 and references bounded segment descriptors. The `CRBDIR01` radix
tree hashes every node and binds live-page count and rolling SQLite checksum.
Initial directories use a streaming k-way index merge; incremental preparation
copy-on-writes changed leaves only. Compaction range-fetches authenticated index
chunks into caller-owned scratch and streams selected frames through one
verifier. Directory-cache bytes, sparse pages, and scratch are charged to the
embedding runtime’s limits.

Cell sparse activation uses a fresh local file and the writable VFS. Missing
main-file pages fault through verified object ranges while SQLite WAL, locking,
and checkpoints use the base VFS. `hydrate_step` is an owner-paced bounded
operation on the database worker; it is not a detached task. Range read-ahead is
capped at 64 pages/1 MiB per request and the shared decoded cache is capped at
8 MiB. `with_paged_io_deadline` applies one absolute deadline to the SQL
thread’s page faults, while `take_io_error` preserves the provider/checksum
failure behind SQLite’s I/O code.

The object-store provider remains caller-owned. The filesystem backend supports
immutable reads but does not provide conditional Cell authority updates; the
runtime must fail closed when the authority CAS is unavailable. Server
retention owns remote pins and deletion scope. No method lists object prefixes
to infer state, and no method reads the retired standalone `ltx/<epoch>/`
layout as a Cell root.

A canonical Cell preparation looks like:

```rust,no_run
# #[cfg(feature = "replica")]
# async fn cell_example(
#     replica: &crab_ltx::CellReplica,
#     writer: &mut crab_ltx::ManagedDb,
# ) -> crab_ltx::Result<()> {
let capture = writer.capture()?;
let prepared = replica.prepare(None, &capture, 1, 1).await?;
let root = prepared.root();
// Bind `root` through crab-cell-runtime authority CAS before acknowledging.
let _verified = replica.open_root(&root).await?;
# Ok(())
# }
```

The old standalone epoch-head, public page-map, read-only VFS, and scheduler
surface is intentionally absent. Existing tagged standalone objects remain
outside the Cell graph and require an explicit offline export/import tool if an
operator must migrate them; this crate adds no compatibility reader or alias.

## Session, filesystem and execution rules

- Use a dedicated database thread or bounded blocking executor for local APIs.
  Optional replica methods perform async network I/O and offload replay/compaction.
  `&mut ManagedDb` serializes SQL and capture;
  the server must additionally prevent reads/writes while publication is pending.
- Treat SQL callbacks as trusted application code. Only mutate the main
  database. Do not ATTACH databases, change pager pragmas/hooks, manually
  commit/rollback, run direct checkpoints, or alter `_litestream_seq` and
  `_litestream_lock`. There is no arbitrary-SQL service or borrowed writer pool.
- SQLite's WAL hook records the application's committed frame boundary. Capture
  must reach that boundary before any checkpoint/control write can reset it.
  A damaged later commit cannot be acknowledged as an earlier valid cut.
- The private metadata directory `.<filename>-crab-ltx` is atomically claimed.
  Existing sessions are refused, including after clean close. On activation or
  capture failure, restore the authoritative plan to a **fresh local directory**
  and let the runtime reopen the authoritative Cell root. Local file listing
  never selects truth.
- No other process may mutate the database, sidecars or session directory.
  Paths are canonicalized before claiming a session; hard-linked database aliases
  remain forbidden, as they do not share SQLite's filename-derived sidecars.
  Directory ownership is local exclusion, not distributed fencing. Paths are
  UTF-8. Destination parent directories must exist; restore rejects SQLite
  sidecars and will not replace an existing destination.
- Retained artifacts are not removed by drop/close. `prune_captured()` releases
  one exact acknowledged batch by path and root equality after Cell authority
  publication. It reverifies bytes before deletion; remote retention and
  retired-directory cleanup remain caller-owned.
- Artifact writes fsync files and their containing directory. A failed operation
  may have installed a file before directory fsync failed; treat it as ambiguous,
  not published. No power-loss guarantee beyond the filesystem's fsync contract.
  Qualified locally on macOS; Linux CI and other platform/filesystem qualification
  are separate evidence. Windows directory fsync is not currently supported.

## Resource bounds and current limits

Defaults: 256 MiB database, 64 MiB per capture, 512 MiB per local input/output
file and 1 GiB aggregate plan/retained captured bytes, with 1,024 segments.
Oversized headers
are rejected before page allocation. The managed writer has `max_page_count`;
capture checks database/WAL sizes and stops on limits. Capture errors fence the
handle. A session at its retention limit must be published/rotated by the caller.

These format and per-operation limits are not an RSS quota. A host may add an
aggregate local-disk quota with `DiskBudget`; local snapshot capture and plan
operations still materialize database-sized buffers.
Plans retain compressed input bytes. Cell exact-root restore and compaction use
bounded frame batches and disk scratch rather than database-sized memory. Cell
capture keeps its packed checksum index on local disk (about 2 MiB
per GiB at 4 KiB pages) and keeps only the current changed-page overlay resident;
large truncations still read the removed suffix to update the exact rolling sum.
Standalone local capture retains its dense in-memory checksum base. Cell
compaction retains O(segment count) cursors and a bounded decoded-page batch;
its scratch requirement includes all authenticated indexes plus the compacted
LTX, codec index and sidecar. Managed writes reserve twice `max_capture_bytes`
before SQLite begins and reconcile to exact main database, live WAL and retained LTX bytes
after capture, checkpoint and pruning. Sparse activation reserves every newly
materialized page. A failed post-BEGIN operation retains conservative admission
until the fenced handle is discarded. `ScratchMonitor` lets the embedding
service reserve filesystem headroom and remeasure actual free space alongside
unrelated consumers.
`resume_with_host` reserves the complete restored database before installing its
destination; writable sparse activation instead admits pages as they materialize.

`Host` injects all library-owned local filesystem operations: canonical paths,
exclusive session claims, committed WAL observation, bounded artifact reads,
capture, snapshot/restore/compaction installation, sparse creation and local pruning.
Use `Host::{verify,restore,compact}` for injected local operations; the free
functions use the same path with the default host. `resume_with_host` retains
that host through installation and subsequent capture, even without `replica`.
`with_local_disk_budget(DiskBudget)` shares byte-precise admission across cloned
hosts. A failed write reservation returns `TransactionError::Admission` before
the callback runs, so callers may retry without treating the writer as ambiguous.

`with_sqlite_vfs(name)` selects an already registered SQLite base VFS, including
for the writable sparse wrapper. The embedding host must keep that registration
alive for the process lifetime and match its file namespace to `FileSystem`.
Unknown names fail closed. The filesystem controls artifacts/installation;
SQLite's selected VFS controls its pager, WAL writes, locks and shared memory.
Both seams are needed to simulate local machine faults coherently.

`Executor::dispatch` runs replica verification/encoding/recovery/compaction jobs.
`Executor::start_worker` starts each paged I/O worker independently of the caller
and blocking pool; `Worker::join` supervises teardown after queue closure. The
worker still drives a Tokio runtime while idle so pooled provider connections
continue progressing. Dispatch cancellation never rolls back side effects.
Default hosts share 32 object-store request slots, up to 16 CPU and dirty-job
slots (each independently capped by available CPUs), and two large-recovery slots.
`with_io_slots`, `with_job_slots`, `with_dirty_slots`, `with_recovery_slots` and
`with_scratch_slots` accept shared Tokio semaphores for explicit service budgets;
scratch permits represent one MiB each. `with_scratch_monitor` receives the
process-wide admitted scratch bytes after those permits are acquired, so the
embedding service can reject current disk pressure before body downloads. Dirty admission
precedes capture preparation; recovery admission is nested inside it before body
downloads for restore, resume, bundle and compaction. Full restore/resume and
Cell compaction reserve two database images plus 64 MiB; Cell compaction also
reserves the exact authenticated source-index bytes. Oversized jobs fail before
body downloads instead of waiting forever. Cancelled dispatched jobs retain
their CPU/dirty/recovery/scratch reservation until the work finishes; returned
roots, page maps, sparse writers and database handles do not retain it.
Closed semaphores reject new work. These are concurrency ceilings, not byte-weighted
memory admission, bounded caller task queues or admission for synchronous local APIs.
The disk budget covers managed WAL/LTX and sparse-page growth; request scheduling
and memory admission remain host policy. An embedding runtime may install one
`HostResourceAdmission` so each host I/O, blocking job, recovery cohort, dirty
  cohort and scratch MiB also owns a runtime-ledger token; hosts without that
  hook retain the semaphore-only contract.

The clock controls capture timestamps and checkpoint ages; compaction receives
explicit monotonic times from its owner. No default provider or dependency
versions change. See the [parity matrix](PARITY.md) for qualification boundaries.

```no_run
use crab_ltx::{CaptureBatch, Host, Limits, ManagedDb, Result};
use std::path::Path;

fn continue_local(host: Host, acknowledged: &CaptureBatch, fresh: &Path) -> Result<ManagedDb> {
    // The caller supplies the complete acknowledged snapshot-plus-deltas plan,
    // not a latest-file listing or an unacknowledged local WAL.
    let limits = Limits::default();
    let plan = host.verify(&acknowledged.segments, acknowledged.position, limits)?;
    ManagedDb::resume_with_host(&plan, fresh, limits, host)
}
```

Not implemented here: HTTP owner/control CAS, owner election, metrics export,
remote retention/GC, encryption/key management, application schema or HTTP integration.

## Verification

Latest local proof (2026-09-18, macOS): the canonical Cell root suites and
doctests pass with `replica`; the minimal-feature local suite also passes.
Shared-worker/cache bounds,
ordered concurrent reads, cancellation-safe admission, copy-on-write metadata,
external-merge compaction, injected-filesystem failure cleanup and bounded frame
reads have regression coverage. The isolated RustFS fixture passes separately;
its disposable container/bucket were removed. This is library correctness evidence,
not a 1K–10K active-database or 1,000 TPS capacity result. See
[SCALABILITY.md](SCALABILITY.md) for sizing assumptions and remaining gates.
Strict Clippy passes on Rust 1.97 with and without `replica`; formatting and the
workspace architecture guardrails pass.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" cargo test -p crab-ltx --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" cargo clippy -p crab-ltx --all-targets --locked -- -D warnings
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" cargo clippy -p crab-ltx --features replica --all-targets --locked -- -D warnings
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" cargo fmt -p crab-ltx -- --check
```

Tests exercise real SQLite commit/rollback, passive/truncate checkpoints,
auto-vacuum shrink/regrowth, cold restore, process kill followed by source loss,
snapshot/compaction byte identity, an independent bitwise CRC oracle, both page
encodings, corrupt WAL capture fencing, malformed files/chains and limits.
The workspace's existing `cargo test --workspace` CI includes this member.

Initial local evidence (2026-09-13, macOS): 8 unit tests, 14 integration tests
(including the subprocess fixture entry point), and 1 compiling doc-test passed.
Strict Clippy, minimal-feature/all-target compilation, formatting and the local
round-trip example passed. Both copied license texts match upstream SHA-256.

Canonical Cell-root evidence (2026-09-18, macOS): the feature-gated
`cell_roots`, `host_hooks`, `replication`, and node-frame suites cover source
loss, exact root reopen/restore, sparse activation, bounded hydration, delayed
faults, checksum fencing, compaction, bundle overlays, and provider/cache
lifetime. The default-feature local suite and strict Clippy remain separate
proofs. These are bounded correctness fixtures, not a 1K–10K active-Cell or
1,000 TPS capacity result.

The RustFS scale example is the live provider path. It prepares a unique
CellStorageLayout prefix through `CellReplica`, deletes the source database,
restores the exact root, compacts the complete range, and compares source and
restored BLAKE3/length. Run it only against a disposable bucket as documented
in [examples/README.md](examples/README.md).

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo clippy -p crab-ltx --features replica --all-targets --locked -- -D warnings
```

Remaining qualification: broad affected-consumer/platform CI, upstream golden
fixture corpus/external interoperability, fuzzing, exhaustive filesystem and
power-loss faults, measured memory/latency, and the complete RustFS/HTTP
owner-publication protocol. No production-ready or browser-parity claim is
made by these tests. Tagged standalone object prefixes remain outside the Cell
graph and are not read by the runtime.
