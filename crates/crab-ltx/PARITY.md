# Celld functional parity and Crab safety boundaries

Reference: `denoland/celld`, `10cb1303dac710dcb3b557e318e08c855261f68b`,
`crates/ltx`. This is a capability comparison, not an API-name, wire-envelope,
performance or production-readiness equivalence claim. Source notices and hashes
are in [UPSTREAM.md](UPSTREAM.md); executable usage is in [README.md](README.md).

## Capability map

| Celld capability | Crab implementation | Evidence / adaptation |
| --- | --- | --- |
| Managed WAL capture and snapshots | `ManagedDb::{transaction,capture,snapshot}` | Real SQLite, WAL-hook committed boundary, CRC oracle, process kill and source loss |
| Checkpoint modes | `ManagedDb::checkpoint`, four `CheckpointMode` variants | Every generated cut returned; errors fence the writer |
| File/object-store transport | `Replica` with existing `crab-storage::Store` | RustFS publication/CAS proof; filesystem CAS updates fail closed when unsupported |
| Exact restore | `VerifiedLocalPlan`, `Replica::{open_exact,restore}` | Every intermediate checksum and exact object digest verified |
| Epoch-chain continuation | `Replica::inherit`, `resume`, sparse activation | Flattened pinned plan plus parent identity, explicit object epochs; late old-epoch writes ignored |
| Bundle envelope | `bundle::Bundle::{encode,decode,segment}` | CRB1 retains repository/string epoch, exact TXID range and digest; bounded rows, no overlaps/trailing payload |
| Bundle-backed transport | `Replica::{bundle,replicate_bundle}` and explicit manifest extents | Direct matching-row publication; restore, paging and compaction share one resolver; corrupt bundle never falls back to native objects |
| Range/level compaction | `Replica::compact_range`, `compact` | Both span endpoint and final image compared; publication CAS retains source objects |
| Level timing/selection | `CompactionSchedule` | Owner-driven monotonic deadlines; at most 128 files per selected run; no listing discovery |
| Paged reads | `PagedDatabase::{read_page,read_run,open_sqlite}` | Hash-pinned indexes and per-frame BLAKE3/CRC; range limits and short-read tests |
| Writable sparse VFS | `PagedDatabase::open_writable` | Fresh sparse file, inherited page CRC seed, normal SQLite WAL/SHM/locks; capture reads also traverse VFS |
| Incremental hydration | `ManagedDb::{hydration,hydrate_step}` | Bounded owner-paced work; writes/truncations supersede old cut pages; failed fetches remain retryable |
| Prefetch/read-ahead | Coalesced verified runs plus one-run I/O cache | Up to 64 pages/1 MiB; conservative adjacent-page policy instead of Celld's B-tree prediction |
| Local restart / release | Exact-root local resume (also without `replica`) and `prune_published` | Fresh directory; no promotion of unacknowledged leftovers; pruning retries ambiguous directory sync |
| Host facilities | `Host`, `FileSystem`, `FileIo`, `Clock`, `Executor`, `Worker`, named SQLite base VFS | Injectable claims/install/sparse allocation, capture and WAL observation; worker startup/join and blocking dispatch; scope below |

## Safety substitutions

1. **Checksums stay mandatory.** No checksum-disabled L0 writer or zero-checksum
   continuation marker. Sparse continuation uses the authenticated page-map CRC
   index; new cuts link to the inherited checksum and TXID.
2. **Exact manifests replace discovery.** No bucket listing, latest-TXID heuristic,
   stale epoch search or local-database fallback determines a restore plan.
   Inheritance pins the predecessor digest and copies its exact descriptors.
   Compaction does not follow mutable parents or choose inputs from a listing.
3. **Bundle location is explicit.** The envelope's rows are verified on creation;
   published extents and index digests authorize ranged reads. A range reader
   checks the selected LTX/frame, without downloading the entire envelope footer.
   Changing an unrelated bundled segment cannot substitute the requested one.
4. **Deletion is narrow.** Remote objects remain retained for old manifests.
   Local pruning requires an exact `SegmentInfo` in a `ReplicaHead`; it checks
   the local bytes before deletion. A historical receipt proves publication,
   not current retention: the caller must keep remote recovery roots pinned.
5. **SQLite state has one owner.** Each sparse main file shares hydration state
   across all VFS connections. Fetches run outside the installation gate, then
   recheck under the same gate as writes/truncation. Partial writes resolve the
   untouched bytes. A successful truncate retires old pages permanently.
6. **Cancellation is not rollback.** Dispatched jobs can finish after their future
   is dropped. CAS responses can be lost after commit. Supervise operations and
   reconcile exact roots before retrying; never replay application SQL blindly.

## Host and deployment boundaries

The injected filesystem covers all library-owned local operations, including
session claims, committed-WAL reads, bounded artifact verification, atomic
snapshot/restore/compaction installation, sparse creation and published pruning.
`Host::{verify,restore,compact}` and `ManagedDb::resume_with_host` keep this
boundary explicit. Default convenience functions use the same implementation.
Pruning preserves accounting through a failed directory sync and safely retries
an already removed, exactly published artifact.

`Host::with_sqlite_vfs` selects a registered process-lifetime base VFS for ordinary
and writable sparse SQLite. It must share the injected filesystem's namespace;
unknown names do not fall back. Pager/WAL writes, locks and SHM remain SQLite's
responsibility, not a second Rust pager implementation. These are simulator
integration hooks, not a bundled deterministic machine simulator.

The clock covers timestamps/checkpoint ages. The executor covers verification,
encoding, restore and compaction dispatch, plus independently progressing paged
worker startup/join. The paged worker retains its dedicated Tokio runtime and
30-second fault deadline, avoiding caller-runtime deadlock and keeping pooled
provider connections alive while SQL is idle. Defaults retain the existing
filesystem/SQLite/Tokio dependencies.

`Bundle` can encode rows from multiple repository identities. `replicate_bundle`
publishes the matching repository/epoch rows without standalone LTX uploads;
each replica retains its own envelope copy and CASes its head independently.
Node-wide group-commit aggregation scheduling and shared-bundle GC are not supplied. Background
hydration/compaction are callable bounded operations, not an autonomous daemon.
The embedding server owns work admission, worker counts, timers and cancellation.

Celld's node log, actors, follower durability, placement, owner election and HTTP
response gate are outside its LTX crate and outside this change. Crab's combined
HTTP owner/head CAS still requires server implementation. A library epoch-head
receipt is not a lease and cannot authorize a successful HTTP response.

## Format and qualification

The unreleased manifest schema hard-cuts from V1 to V2. New fields identify each
segment's storage epoch, compaction level and optional bundle extent; an optional
parent records the inherited root. No compatibility reader is added. Existing
LTX encoding and all storage dependency versions remain unchanged.

Tests cover real SQLite activation before hydration, writes, all checkpoint
modes, auto-vacuum shrink/regrowth, bundle corruption, exact inherited recovery,
range compaction byte identity, bounded scheduling and local pruning. The
`rustfs_roundtrip` fixture runs the inherited bundle/sparse-write path against
an isolated RustFS bucket, in addition to source-loss recovery and CAS races.

An explicitly delayed remote fault/checkpoint race verifies that the newer local
page wins. An idle-provider-runtime regression failed before the queue fix and
passes afterward; the expanded RustFS scenario then completed in seven seconds.
Host-extension tests exercise partial artifact writes, file sync/rename failure,
atomic installation, sparse allocation/sync, named VFS selection, worker lifecycle
and pruning retries after failed parent sync. Minimal-feature tests include exact
local resume and every checkpoint mode. Cross-page partial sparse writes cover
512/4096/65536-byte pages; multi-level compaction and three-epoch inheritance
preserve mixed bundle/native plans. Verification commands and live evidence are
recorded in [README.md](README.md#verification).

Remaining qualification: exhaustive pager partial-write/fsync and power-loss
simulation, multi-process sparse crash/recovery, broad Linux and
Windows proof, external Celld/Litestream golden fixtures, fuzzing and measured
memory/latency. Full-chain publication verification still downloads/replays the
complete bounded plan. The feature set is not yet a production-ready HTTP backend,
nor a claim of complete Celld performance, simulator or operational parity.
