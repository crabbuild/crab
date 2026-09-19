# Celld functional parity and Crab safety boundaries

The former standalone epoch-head/paged/scheduler API was hard-removed under
the authorized decision recorded in
[`standalone-replication-audit.md`](../crab-cell-runtime/docs/standalone-replication-audit.md).
This matrix compares the retained canonical Cell path with the pinned Celld
mechanics; it is not an API-name, wire-envelope, performance, or readiness
equivalence claim.

Reference: `denoland/celld`,
`10cb1303dac710dcb3b557e318e08c855261f68b`, `crates/ltx`.
Source notices and hashes are in [UPSTREAM.md](UPSTREAM.md); executable usage is
in [README.md](README.md).

## Capability map

| Celld capability | Canonical Crab implementation | Evidence / adaptation |
| --- | --- | --- |
| Managed WAL capture and snapshots | `ManagedDb::{transaction,capture,snapshot}` | Snapshot returns all newly generated cuts; capture and snapshot ownership tests |
| Checkpoint modes | `ManagedDb::checkpoint`, four `CheckpointMode` variants | Every generated cut returned; errors fence the writer |
| File/object-store transport | `CellReplica` with existing `crab-storage::Store` | Cell root preparation and RustFS source-loss receipt; authority CAS remains runtime-owned |
| Exact restore | `VerifiedLocalPlan` locally; `CellReplica::open_root` remotely | Every intermediate checksum, object digest, directory node, and page frame is verified |
| Cell-root continuation | `PreparedRoot`, `RootRef`, `CellWritableDatabase` | Exact predecessor identity, pre-I/O admission, sparse activation, and successor restore |
| Bundle envelope | `bundle::{Bundle,BundleEntry,BundleRow}` | CRB1 retains exact ranges and digests; `BundleEntry::for_cell` scopes rows to one Cell |
| Bundle-backed preparation | `CellReplica::prepare_bundle` and recovery overlay | Matching Cell rows share the canonical verifier; corrupt bundle data never falls back |
| Range/level compaction | `CellReplica::{prepare_compaction,prepare_scheduled_compaction}` | Selected bodies only; exact reduced pages and replacement directory state verified |
| Paged reads | `CellPagedDatabase::{read_page,read_run}` | Hash-pinned directory paths, per-frame BLAKE3/CRC, bounded ranges and short-read tests |
| Writable sparse VFS | `CellWritableDatabase::open_writable` | Fresh sparse file, inherited page CRC seed, SQLite WAL/SHM/locks through the base VFS |
| Incremental hydration | `ManagedDb::{hydration,hydrate_step}` | Bounded owner-paced work; writes/truncations supersede old cut pages; failed fetches retry |
| Prefetch/read-ahead | Coalesced verified runs plus shared FIFO cache | Up to 64 pages/1 MiB per run and 8 MiB decoded cache; no Celld B-tree prediction |
| Shared resource admission | Shared worker, I/O/job/recovery semaphores on `Host` | Bounded concurrent reads; cancelled dispatched jobs retain permits until completion |
| Managed SQLite cache | Three retained connections with 64 KiB page-cache targets | Exported aggregate permits let the runtime derive active-Cell admission |
| Local restart / release | Exact-root `ManagedDb::resume` and `prune_captured` | Fresh directory; no unacknowledged local promotion; pruning retries ambiguous sync |
| Host facilities | `Host`, `FileSystem`, `FileIo`, `Clock`, `Executor`, `Worker`, named SQLite VFS | Injectable claims/install/sparse allocation, capture and WAL observation, worker lifecycle |

## Safety substitutions

1. **Checksums stay mandatory.** No checksum-disabled L0 writer or zero-checksum
   continuation marker. Sparse continuation uses the authenticated Cell
   directory checksum index; new cuts link to inherited checksum and TXID.
2. **Exact roots replace discovery.** No bucket listing, latest-TXID heuristic,
   stale prefix search, or local-database fallback determines a restore plan.
   Cell roots pin their predecessor and exact descriptors; directory coverage and
   intermediate checksums are verified before publication and frame hashes on read.
3. **Bundle location is explicit.** Envelope rows are verified on creation and
   Cell-scoped extents authorize ranged reads. Changing an unrelated row cannot
   substitute the requested frame.
4. **Deletion is narrow.** Remote objects remain retained for pinned roots.
   Local `prune_captured` requires an exact acknowledged `CaptureBatch` and
   re-verifies local bytes before deletion. Remote retention stays server-owned.
5. **SQLite state has one owner.** Each sparse main file shares hydration state
   across all VFS connections. Fetches run outside the installation gate, then
   recheck under the same gate as writes/truncation. Partial writes resolve
   untouched bytes; successful truncate retires old pages permanently.
6. **Cancellation is not rollback.** Dispatched jobs can finish after their
   future is dropped. Authority responses can be lost after commit. Supervise
   operations and reconcile exact roots before retrying; never replay SQL blindly.

## Host and deployment boundaries

The injected filesystem covers all library-owned local operations, including
session claims, committed-WAL reads, bounded artifact verification, atomic
snapshot/restore/compaction installation, sparse creation, and exact capture
pruning. `Host::{verify,restore,compact}` and
`ManagedDb::resume_with_host` keep this boundary explicit.

`Host::with_sqlite_vfs` selects a registered process-lifetime base VFS for
ordinary and writable sparse SQLite. It must share the injected filesystem's
namespace; unknown names do not fall back. Pager/WAL writes, locks, and SHM
remain SQLite's responsibility.

The executor covers verification, encoding, restore, and compaction dispatch,
plus independently progressing sparse-worker startup/join. Overlapping Cell
views share a worker; the 30-second fault deadline includes queued wait and
keeps pooled provider connections alive while SQL is idle. The embedding server
still owns SQL admission, byte-weighted memory budgets, activation counts,
timers, cancellation, leases, and response gating.

Celld actors, follower placement, owner election, and HTTP policy stay outside
the LTX crate. `crab-ltx` owns strict node-frame and recovery-overlay mechanics;
`crab-cell-runtime` owns follower storage, seal/gather mechanics, and dual-proof
durability. A library root or follower receipt is not a lease and cannot by
itself authorize an HTTP response.

## Format and qualification

The unreleased Cell root schema hard-cuts from V1 to V2. New fields identify each
segment's Cell/incarnation, compaction level, directory extent, and predecessor
root. No compatibility reader is added. Existing LTX encoding and dependency
versions remain unchanged.

Tests cover real SQLite activation before hydration, writes, all checkpoint modes,
auto-vacuum shrink/regrowth, bundle corruption, exact root recovery, range
compaction byte identity, bounded hydration, and local pruning. The canonical
RustFS scale example covers source loss, root restore, and full compaction.

Remaining qualification: exhaustive pager partial-write/fsync and power-loss
simulation, multi-process sparse crash/recovery, broad Linux and Windows proof,
external Celld/Litestream golden fixtures, fuzzing, and measured memory/latency.
Cell roots use an authenticated radix directory whose incremental publisher reads
only changed leaves/ancestors; initial construction k-way merges ordered index
streams without retaining a complete locator map. Persistent cache rebuild and
broader multi-process/provider qualification remain scalability gates; the
canonical 5 GiB RustFS receipt is recorded in [SCALABILITY.md](SCALABILITY.md).
See [SCALABILITY.md](SCALABILITY.md) for the 1K–10K Cell target and remaining gates.
