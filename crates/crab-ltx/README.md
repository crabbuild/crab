# crab-ltx

Embedded SQLite WAL capture and exact LTX recovery, with optional object-store
replication and paged SQL reads. Crab-owned integration of Celld's mechanics;
no Celld Git dependency or Litestream daemon. Default features remain empty.
Enable `replica` for the existing `crab-storage` transport and Tokio integration.

Status: local and standalone remote replication are implemented. Native and
shared-bundle LTX cuts can be prepared as immutable Cell/incarnation-scoped roots,
and exact range/full compaction can produce a representation-only prepared root.
These roots are bound to checked `crab-cell-runtime` control successors and the
runtime is composed by `crab-http-server`. Initial directories are constructed
from a streaming k-way index merge and uploaded one radix leaf at a time.
Writable Cell activation now streams authenticated checksums to a local
fixed-width file and capture updates it incrementally. Cell compaction now uses
disk-spooled authenticated indexes, a k-way external merge, bounded frame reads
and multipart uploads from the injected filesystem. The complete product hard
cutover and measured capacity qualification still remain. See the
[next architecture](../crab-http-server/next-architecture/README.md).

## Contract

| API | Local result |
| --- | --- |
| `ManagedDb::open(path, limits)` | Exclusive fresh capture session; owns control, read-lock and application-writer SQLite connections, each configured with a 64 KiB page-cache target |
| `ManagedDb::{resume,resume_with_host}(plan, path, limits, …)` | New local session continuing an exact verified TXID/checksum; available without `replica` |
| `transaction(closure)` | One locally committed SQL transaction; no remote-durability claim |
| `capture()` | Ordered `CaptureBatch` containing every newly generated cut and its endpoint, including checkpoint cuts |
| `checkpoint(mode)` | Capture barrier plus PASSIVE/FULL/RESTART/TRUNCATE; returns every generated cut |
| `snapshot(path)` | Returns `(LocalSegment, CaptureBatch)`: standalone `1..=txid` snapshot plus every newly captured cut |
| `VerifiedLocalPlan::new(files, target, limits)` | Owns verified bytes of an explicitly selected snapshot-plus-deltas chain |
| `restore_exact(plan, path)` | Installs a new SQLite file at exactly the verified endpoint; never overwrites |
| `compact_exact(plan, path)` | Compacts that complete chain into a verified standalone snapshot; never deletes inputs |
| `Host::with_local_disk_budget(DiskBudget)` | Shares byte-precise WAL/LTX/sparse-page admission across cloned hosts; exhausted write admission occurs before SQL begins |
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
`checkpoint()`. When continuing an existing remote head, retain and publish the
returned batch; do not append the full snapshot to that existing delta chain:

```rust,no_run
# #[cfg(feature = "replica")]
# async fn snapshot_publication(writer: &mut crab_ltx::ManagedDb, replica: &crab_ltx::Replica,
#     head: crab_ltx::ReplicaHead, snapshot_path: &std::path::Path) -> crab_ltx::Result<()> {
let (snapshot, pending) = writer.snapshot(snapshot_path)?;
let head = replica.replicate(&pending, Some(&head)).await?;
// `snapshot` is an independent full recovery artifact, not another delta.
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

Additional runnable examples cover object-store replication, paged reads,
sparse writable continuation, compaction, and historical recovery. See the
[examples guide](examples/README.md).

## Object-store replication and paged SQLite

Enable `crab-ltx`'s `replica` feature. Construct a `crab_storage::Store` using
Crab's existing credential/provider builders; wrap it in a repository
`StoreLayout`. There is no second S3 URL parser or credential stack.

The next Cell runtime uses `CellReplica`, not the standalone epoch head:

| API | Result |
| --- | --- |
| `CellReplica::new(layout, cell, incarnation, limits)` | Binds every immutable path to one typed Cell incarnation and rejects staged stores |
| `CellReplica::open_new(path)` | Exclusively creates a fresh local database with the replica's filesystem, SQLite VFS and limits for worker-owned bootstrap |
| `prepare(base, cuts, sequence, schema).await` | Admits the complete chain, verifies native LTX/index bytes, writes content-addressed directory/descriptor/root objects and returns an unforgeable `PreparedRoot`; writes no mutable key |
| `prepare_bundle(base, bundle, sequence, schema).await` | Selects canonical rows for this Cell/incarnation from a shared bundle, verifies their chain, retains the bundle and indexes, and prepares the advancing immutable root without a mutable write |
| `prepare_compaction(base, range, level, scratch_directory).await` | Admits before remote reads, externally merges authenticated indexes through caller-owned scratch, streams the exact replacement, preserves logical position/sequence/schema, and returns a representation-only prepared root for the normal authority CAS |
| `prepare_scheduled_compaction(base, scratch_directory).await` | Selects one bounded eight-input level promotion, or a complete level-nine replacement near segment/graph-byte admission; returns `None` when no work is due and never publishes control |
| `open_root(root).await` | Reopens the exact digest, validates canonical metadata, scope, chain and the authenticated radix root without downloading LTX bodies or every directory leaf |
| `VerifiedRoot::paged().read_page(page).await` | Walks only the selected hash-pinned radix path, range-reads its LTX frame and verifies frame BLAKE3, decoded page number and page checksum |
| `VerifiedRoot::paged().prepare_writable(path).await` | Streams authenticated directory checksums to a fresh local file without LTX bodies and returns an exact-root writable activation value bound to `path` |
| `CellWritableDatabase::open_writable(path)` | Creates a fresh sparse SQLite file, seeds exact TXID/checksum continuation and faults verified pages through the shared VFS driver |
| `PreparedRoot::{root,predecessor,verified}` | Supplies the exact publication proposal and predecessor proof without exposing an unchecked constructor |

`crab-cell-runtime::Control::publish_prepared` verifies Cell/incarnation, schema
and predecessor identity before constructing the one legal control successor.
Only `CellAuthority` may then apply the ETag update. An upload or a returned
`PreparedRoot` alone is not publication and must never release an application
response.

Cell objects use `CellStorageLayout` under
`cells/v1/apps/<app>/cells/<cell>/inc/<inc>/objects/`. Root JSON is canonical
compact v1 and references at most 64 pages of 96 segment descriptors. Its binary
`CRBDIR01` radix tree has 256-entry leaves/branches, hashes every node and binds
the live-page count and rolling SQLite checksum. Cold open reads bounded root
metadata and one directory root; page bodies and descendant directory nodes fault
on demand. Writable activation walks the authenticated directory once and streams
one big-endian eight-byte checksum per database page to a fresh local sidecar in
64 KiB chunks. Capture clones only its pending overlay, updates the rolling
checksum from changed pages and a truncated suffix, then applies positional
sidecar writes only after the matching LTX cut is synced and renamed. A sidecar
write or sync failure fences the session. Incremental preparation copy-on-writes only
changed leaves and ancestors, prunes truncated subtrees by their authenticated
ranges and reuses every untouched digest; it does not fetch historical indexes or
materialize all live locators. Initial root construction does not materialize a
locator map or retain encoded directory bodies: it k-way merges final locators,
filters entries invalidated by a later truncation and uploads each completed
256-page leaf before continuing. Cell compaction range-fetches and authenticates
index chunks into scratch, streams every selected LTX range through its manifest
BLAKE3, keeps one cursor per segment, range-fetches at most 1 MiB of adjacent
frames, and spools both the codec index and authenticated sidecar. The compacted
LTX and sidecar upload in 8 MiB parts without bypassing the injected filesystem;
scratch is removed best effort on every return path.
Directory nodes do share a process-wide 8 MiB verified-byte cache whose
key isolates backing Store instances and exact Cell/incarnation paths. Sparse
page faults coalesce adjacent frames from one immutable object into bounded 1 MiB
range reads while retaining per-frame verification. `VerifiedRoot::restore`
streams those runs to a same-directory scratch file, verifies the final checksum
and length, then atomically installs a new destination without replacement. This
is therefore not yet the complete streaming 5 GB write path required by the
platform capacity gate.

The older `Replica` API below remains for standalone repository replication and
its existing callers. Its mutable epoch head is not Cell ownership authority.

| API | Result |
| --- | --- |
| `Replica::new(layout, epoch, limits)` | Explicit caller-owned epoch; rejects staged stores and invalid epoch components |
| `head().await` | Reads the one named epoch head, never lists objects to infer latest |
| `open_exact(manifest_digest).await` | Reopens a pinned immutable recovery root after restart; no mutable-head read or mutation token |
| `replicate(&batch, expected_head).await` | Fully verifies new cuts against authenticated predecessor page state, uploads LTX/indexes, then CASes the epoch head |
| `restore(&head, destination).await` | Downloads only the pinned plan and installs a verified new SQLite file |
| `compact(&head).await` | Publishes one verified full snapshot with head CAS; retains all source objects |
| `compact_range(&head, range, level).await` | Downloads only selected bodies; proves exact reduced page bytes and the replacement indexed state before CAS |
| `inherit(&source, &parent).await` | Admits limits before I/O; verifies destination indexes and object sizes, then pins a new epoch without downloading LTX bodies |
| `resume(&head, path).await` | Full exact restore into a fresh session, continuing the inherited TXID/checksum |
| `bundle(&head).await` | Verbatim LTX envelope, authenticated sidecars and exact bundle extents; CAS replacement, no fallback reads |
| `replicate_bundle(&bundle, expected).await` | Direct capture publication from this repository/epoch's bundle rows, without standalone LTX uploads |
| `paged(&head).await` | Builds an immutable page map from authenticated indexes without full LTX downloads |
| `PagedDatabase::read_page(pgno).await` | Exact range GET, compressed-frame BLAKE3 and decoded-page checksum verification |
| `PagedDatabase::open_sqlite()` | Read-only SQLite VFS over that pinned cut; SQL faults use a shared, independently progressing I/O worker |
| `PagedDatabase::open_writable(path)` | Writable sparse SQLite activation; checksum-seeded continuation without full download |
| `read_run(first, max_pages).await` | Coalesced authenticated range reads, at most 1 MiB decoded |
| `ManagedDb::hydrate_step(pages)` | Bounded background work through the same sparse VFS as foreground SQL |
| `ManagedDb::prune_published(&head)` | Removes only this session's exact published local artifacts |
| `CompactionSchedule::run_due(...)` | Caller-driven monotonic level scheduling, bounded file selection and head CAS |

```rust,no_run
# #[cfg(feature = "replica")]
# async fn remote_example(layout: crab_storage::StoreLayout<crab_storage::Store>, captured: crab_ltx::CaptureBatch) -> crab_ltx::Result<()> {
use crab_ltx::{Limits, Replica};
use std::path::Path;

// Allocate an epoch externally; never reuse it for a fresh ManagedDb session.
let replica = Replica::new(layout, "activation-19", Limits::default())?;
let head = replica.replicate(&captured, None).await?;
replica.restore(&head, Path::new("recovery/repository.sqlite")).await?;
let view = replica.paged(&head).await?;
tokio::task::spawn_blocking(move || -> crab_ltx::Result<()> {
    let sql = view.open_sqlite()?;
    let title: String = sql.connection().query_row(
        "SELECT title FROM issues WHERE number = 1", [], |row| row.get(0),
    )?;
    println!("{title}");
    Ok(())
}).await??;
# Ok(())
# }
```

The library's epoch head is **not** the next HTTP server's combined owner/head
control record. It cannot fence a former owner or acknowledge an HTTP request.
Server integration must bind a frozen exact plan to its authoritative control
CAS; it must never restore an inherited activation by reading an unfenced
mutable epoch head. Leases, repository identity, epoch allocation, scheduling,
retention pins and durable response release remain server responsibilities.

A sparse continuation uses a pinned predecessor and an externally allocated epoch:

```rust,no_run
# #[cfg(feature = "replica")]
# async fn sparse_example(previous: crab_ltx::Replica, next: crab_ltx::Replica, digest: [u8; 32], path: std::path::PathBuf) -> crab_ltx::Result<()> {
let parent = previous.open_exact(digest).await?;
let inherited = next.inherit(&previous, &parent).await?;
let pages = next.paged(&inherited).await?;
let (mut db, batch) = tokio::task::spawn_blocking(move || -> crab_ltx::Result<_> {
    let mut db = pages.open_writable(&path)?;
    db.transaction(|tx| {
        tx.execute("INSERT INTO issues VALUES (2, 'Sparse activation')", [])?;
        Ok(())
    })?;
    let batch = db.capture()?;
    Ok((db, batch))
}).await??;
let published = next.replicate(&batch, Some(&inherited)).await?;
// HTTP must separately bind published.manifest_digest() to its owner/head CAS.
tokio::task::spawn_blocking(move || -> crab_ltx::Result<()> {
    db.prune_published(&published)?;
    db.hydrate_step(128)?; // owner-paced maintenance, not required before writes
    db.close()
}).await??;
# Ok(())
# }
```

The version-2 replica manifest layout, relative to `StoreLayout::repo_path`, is:

```text
ltx/<epoch>/head.json                 conditional-create/update, at most 1 MiB
ltx/<epoch>/objects/<blake3>.ltx       immutable LTX bytes
ltx/<epoch>/objects/<blake3>.idx       immutable authenticated frame index
ltx/<epoch>/objects/<blake3>.manifest.json  immutable recovery root
ltx/<epoch>/objects/<blake3>.bundle    verbatim LTX payloads plus CRB1 footer
```

The head contains the immutable manifest's bytes; `manifest_digest()` is its
BLAKE3 identity, suitable for pinning in external control or backup records.
`open_exact()` restores that historical root even if the mutable head advances
or becomes corrupt. Such historical receipts cannot update or compact the head.
The manifest names exact `SegmentInfo` values and index digests/sizes. Each binary
index entry is 60 bytes: page number (u32 BE), frame offset and length (u64 BE),
frame BLAKE3 (32 bytes), decoded-page CRC64 (u64 BE). Entries follow LTX page order.
Publishing generates indexes only from verified sized-block LTX. No LTX wire
format change is required. Old frame files remain supported by exact local
recovery, but are not accepted for paged publication. The page map validates
coverage, ranges, each truncation/regrowth and intermediate database checksums.
Every demanded compressed frame is authenticated before decoding. This relies
on the authorized publisher/head, not a signature or protection against an
attacker authorized to rewrite the head and all its objects.

Each segment also names its origin epoch, level and optional bundle extent.
An inherited manifest contains a flattened exact plan and pins the predecessor
epoch/digest/position; it never rediscovers ancestors by listing or follows a
mutable parent head. V2 hard-replaces the unreleased V1 shape; there is no
compatibility reader. LTX wire encoding and checksums remain unchanged.

Both native and bundled appends use one verifier. Every new LTX file is checked
in full, including its digest, file CRC, header and decoded pages. Applying its
authenticated index to the predecessor map proves coverage, truncation/regrowth,
TXID continuity and each intermediate database checksum. Live publication receipts
retain an immutable map; reopened receipts fetch hash-pinned indexes, never
historical LTX bodies. A map is reusable only within the same `Replica` instance
or its clones; another instance reconstructs it through its own store. Failed
verification or CAS never mutates the predecessor map.

Inheritance validates source identity and destination resource limits before
any reads. It builds the map from destination indexes and HEAD-checks referenced
native/bundle object sizes. It does not copy or eagerly verify their bodies.
Sparse reads verify demanded frames; full restore verifies every body, while
range compaction verifies selected bodies against their pinned indexes and proves
the replacement state from the complete indexed plan. Thus same-size body corruption is detected
when read, not necessarily at inheritance. Authorized manifests/indexes and
continued object retention are required; this is not a background integrity scrub.
Cold index loading still costs work proportional to history/pages. Live appends
copy a directory of shared 256-page metadata blocks and only modified blocks;
rolling checksums avoid a full locator scan. This reduces update cost, not total
metadata residency.

`bundle::Bundle` validates standalone envelopes, including multiple repository
identities. `replicate_bundle` selects matching repository/epoch rows for direct
publication; each repository retains the envelope in its own namespace. Head
publication is independent, not an atomic multi-repository transaction. Host-level
group-commit coordination and shared-object retention remain host policy.

Publication is upload-then-CAS. Failures/cancellation can leave orphan objects
or an already-committed head whose response was lost. Retain the batch, reload
`head()` and compare its exact `segments()` and `position()` before proceeding.
Never retry a domain SQL mutation automatically. Compaction uses the expected
head token; a concurrent write makes the compaction CAS fail rather than rewind
the head. It never deletes inputs, so pinned older heads remain readable.
Due compaction levels also promote a singleton, allowing an idle repository's
last segment to progress through L1/L2/L3 without requiring another write.

S3/RustFS, GCS and Azure use the existing Crab provider implementations; only
the recorded RustFS run below constitutes live cloud-protocol proof here.
The filesystem backend supports immutable upload/initial head, restore and
paging, but `object_store` 0.14.1's filesystem backend does **not** implement
conditional update. Subsequent head publication/compaction fails closed there.

Two VFS modes are available: immutable read-only views and fresh writable sparse
activations. Sparse activation seeds CRCs from the pinned authenticated page map;
new captures continue at the inherited TXID plus one. Missing main-file pages
fault through verified ranges; WAL/locking/checkpoints use SQLite's base VFS.
Capture and snapshot main-file reads also pass through the VFS, never read holes
as database bytes. Writes and fault installation share a gate; a delayed fetch
cannot overwrite a checkpoint. Successful truncation permanently retires older
cut pages so regrowth cannot resurrect them. Partial writes first resolve untouched
page bytes. Never open a sparse file independently through the default SQLite VFS.

`hydration()` reports resolved cut pages (hydrated or superseded by writes/truncate).
`hydrate_step()` is a bounded owner-driven step, not a detached task. Call it on
the database worker between foreground operations. Range read-ahead fetches at most
64 pages/1 MiB into a shared FIFO cache capped at 8 MiB decoded payload, with
additional bounded bookkeeping. It does not reproduce Celld's B-tree-child prediction.
Use a blocking executor for SQL. Overlapping views share an independent Tokio
I/O worker: process-wide by default, or per injected executor host and its clones.
The worker permits 32 concurrent faults and 256 queued requests; a full queue
fails with a capacity error. The 30-second fault deadline includes queued wait.
`take_read_error()` (immutable) or `take_io_error()` (managed sparse writer)
retains the underlying range/decode failure when SQLite reports an I/O code.
One immutable VFS and one writable wrapper per selected base VFS live for the process;
each active view is registered separately. Closing a view removes discovery,
while each already-open SQLite file holds its own page-source reference. The
last close frees that view's source; the last view sharing a worker joins it.
Closed-view cached bytes remain bounded and age out through FIFO eviction. Leaked SQL statements
also leak their SQLite/page-source state, never leave dangling VFS pointers.
SQL is trusted application code: SQLite's process-global view registry is not
an authorization boundary. Do not expose arbitrary SQL/ATTACH to API callers.

Replication fully verifies new cuts against the predecessor map without historical
body downloads. Remote plan admission includes index bytes; local `Limits` remain
admission bounds, not RSS limits. Page maps retain one locator per live page plus
fetched indexes during construction. The [scalability assessment](SCALABILITY.md)
records what remains before qualifying 1K–10K active databases per node.

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
  and start a new caller-owned epoch. Local file listing never selects truth.
- No other process may mutate the database, sidecars or session directory.
  Paths are canonicalized before claiming a session; hard-linked database aliases
  remain forbidden, as they do not share SQLite's filename-derived sidecars.
  Directory ownership is local exclusion, not distributed fencing. Paths are
  UTF-8. Destination parent directories must exist; restore rejects SQLite
  sidecars and will not replace an existing destination.
- Retained artifacts are not removed by drop/close. `prune_captured()` releases
  one exact acknowledged batch by path and manifest equality; `prune_published()`
  reconciles exact cuts present in a pinned standalone head. Both reverify bytes
  before deletion. Prune before
  compacting that head; a replacement snapshot alone cannot prove a local cut's
  publication. Remote retention and retired-directory cleanup remain caller-owned.
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
aggregate local-disk quota with `DiskBudget`; standalone snapshot capture and
local plan operations still materialize database-sized buffers.
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
until the fenced handle is discarded. The server must still reserve filesystem
headroom and remeasure actual free space for unrelated consumers.
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
scratch permits represent one MiB each. Dirty admission
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
and memory admission remain host policy.

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

Latest local proof (2026-09-15, macOS): 98 executed runtime tests and five
doctests pass with `replica` (one additional remote test is ignored); 33 runtime
tests and five doctests pass with minimal features. Shared-worker/cache bounds,
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

Remote extension evidence (2026-09-13, macOS): 31 runtime tests and 2 compiling
doctests pass with `--features replica`; the separately invoked RustFS test also
passes. That live test covers full/paged SQL readback after source loss, exact
historical-root recovery, snapshot compaction, competing conditional updates and
stale-compaction rejection. Strict Clippy passes with the replica feature;
default/minimal tests and checks still pass. No dependency versions changed.

Subsequent parity-extension proof (same date): 40 runtime tests and 3 compiling
doctests pass with `replica`; minimal tests and strict Clippy pass with and
without the feature. The separately invoked RustFS fixture also passed
bundled epoch inheritance, sparse SQL writes before full hydration, all four
checkpoint modes, shrink/regrowth, exact partial compaction and resumed capture.
A deterministic regression first reproduced an idle-runtime deadlock: pooled
HTTP drivers stopped when the paged worker blocked on a synchronous receive.
The worker now awaits requests inside its runtime; the regression passes and
the complete RustFS scenario finishes in about seven seconds. Tests also cover
an explicitly delayed fault racing a newer checkpoint page.

Host-completion tests additionally exercise partial artifact writes, file sync
and rename failures, committed-WAL read failure, session claims, injected atomic
installation, sparse allocation/sync, named SQLite VFS selection and worker joins.
Local pruning retries after unlink succeeds but directory sync fails; accounting
is released only after sync succeeds. Cross-page partial sparse writes preserve
untouched bytes at 512/4096/65536-byte page sizes. Three-epoch continuation mixes
bundled ancestors with native deltas, including scheduled L1-to-L2 compaction.
Final host-extension proof: 50 runtime tests and 4 compiling doctests with
`replica`; 29 runtime tests and 4 doctests without default features; strict Clippy
for both feature sets and formatting. The isolated RustFS fixture passed again
in 6.36 seconds; its disposable container/bucket were removed. The page-size
fixture now persists its header and asserts the actual page size before testing
partial writes, preventing a default-size run from masquerading as coverage.

Publication/lazy-takeover corrections (same date): 58 runtime tests and 5
compiling doctests pass with `replica`; 29 runtime tests pass without default
features. New regressions cover snapshot-cut ownership, pre-I/O inheritance
limits, singleton promotion, native/bundle appends with live and reopened heads,
cross-store cache isolation, and lazy corruption detection. Live appends read
zero history bytes; cold appends read only indexes. A 2 MB predecessor opens as
writable sparse SQL with less than 500 KB total inheritance/activation reads in
the instrumented fixture. False post-state checksums with valid file CRCs remain
rejected on both append paths. Strict Clippy passes on Rust 1.97 for both feature
sets. The separate isolated RustFS round trip passed in 4.68 seconds; its
disposable container and bucket were removed. These are bounded fixtures, not
production throughput, memory or power-loss qualification.

RustFS image used:
`rustfs/rustfs@sha256:b7014e0ce2bc703c1316b3ef760e29dfae61fe4a50d1a66fa89638e0f8ea211f`.
The isolated test container had 2 CPUs, 2 GiB memory, ephemeral data/log tmpfs,
loopback-only transport and generated credentials. It and its test bucket were
removed afterward. This proves protocol behavior, not persistent RustFS storage
or server/power-loss durability. GCS/Azure live qualification remains pending.

To repeat the live test, provision a **disposable isolated bucket**, set
`CRAB_LTX_TEST_BUCKET`, `CRAB_LTX_TEST_ENDPOINT`, `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY` outside tracked files, then run:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" \
  cargo test -p crab-ltx --features replica --test remote rustfs_roundtrip \
    --locked -- --ignored --nocapture
```

The fixture writes only `test-repository`, `race-repository`, and `parity-repository`.
It deliberately tests conditional conflicts; do not point it at a shared or
production bucket. Cleanup of externally supplied test storage is caller-owned.

Remaining qualification: broad affected-consumer/platform CI, upstream golden
fixture corpus/external interoperability, fuzzing, exhaustive filesystem/power-loss
faults, measured memory/latency and the complete RustFS/HTTP owner-publication
protocol. No production-ready or browser-parity claim is made by these tests.
