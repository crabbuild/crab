# Celld LTX delta adoption plan

| Field | Value |
| --- | --- |
| Status | In progress; timing, lookaside removal, authenticated directory spans, bounded parallel exact-root restore, and file-backed bundle recovery implemented |
| Scope | `crab-ltx` mechanics and their `crab-cell-runtime` integration |
| Celld reference | `denoland/celld` `12d5b6333fe52717325addcfe1e99e9fd4f77bcd`, `crates/ltx` |
| Goal | Adopt useful Celld mechanics while keeping one exact-root Cell architecture |

## Decision

Keep `CellReplica` and Cell authority as the only remote publication and
recovery path. Borrow Celld's bounded I/O, memory-density, prefetch, and
measurement techniques behind that path. Do not restore the retired standalone
replication interface.

In simple terms, an exact root is a sealed packing list. It names every box
needed to rebuild one Cell and records a fingerprint for every box. Recovery
uses that exact list or fails. It never walks around the warehouse guessing
which boxes look newest.

The alternatives are useful in Celld's architecture but are the wrong seams for
Crab:

- A broad replica client can list levels and discover files. In Crab, discovery
  would duplicate the exact list and make bucket-listing behavior part of the
  correctness interface.
- An epoch head is another mutable sign saying "this is latest." Cell authority
  already says who owns the Cell and which exact root is current. Two signs can
  disagree after a timeout, crash, or partial write.
- A standalone replication interface lets callers publish without the Cell
  actor's fencing, response gate, and authority transition. That would create a
  second durability protocol instead of deepening the canonical module.

The Celld interfaces are not inherently defective. They are unnecessary in
Crab because Cell authority already owns the same decisions with stronger
identity and fencing.

## Baseline

| Capability | Current Crab state | Decision |
| --- | --- | --- |
| Authority and recovery selection | One control-pinned immutable Cell root; no object listing or mutable LTX head | Retain |
| Integrity | Checksum-bearing LTX plus BLAKE3-bound bodies, indexes, directory nodes, frames, and roots | Retain |
| Native compaction | Range reads, disk-spooled indexes, external merge, scratch admission, multipart file upload | Already meets the Celld delta |
| Bundles | Exact and verified; recovery reopens from bounded file-backed storage and replica upload uses replayable staged multipart | Qualify |
| SQLite density | Three 64 KiB page-cache targets per Cell; managed connections now disable SQLite lookaside | Qualify density and throughput |
| Sparse reads and restore | Authenticated radix lookup, adjacent 1 MiB runs, bounded ordered cold-root and restore fetches, shared 8 MiB decoded cache, deadlines | Measure production workloads and improve |
| Capture telemetry | `CaptureBatch` reports bounded LTX phase and byte observations; runtime aggregation is not wired | Qualify |
| Scratch lifecycle | Named files clean up on normal drop; stale session inventory accounts process-death bytes before new admission | Qualify kill/restart fault matrix |

The current `crab-ltx` replica-feature suite passes, including the deterministic
timing recorder and real managed-capture timing tests. The audited Celld crate
compiles, but its package disables library tests and its package test command
runs zero tests. This is correctness evidence for Crab, not a matched
performance comparison.

## Non-negotiable invariants

Every plan below must preserve all of these:

1. Cell authority remains the sole mutable owner/root record.
2. Recovery uses an exact control-pinned root; object listing never selects
   state.
3. Every accepted LTX range remains checksum- and digest-verified.
4. Bundle/native selection never falls back after an arbitrary read error.
5. Provider construction, credentials, retry classification, and storage paths
   remain owned by `crab-storage` and the embedding runtime.
6. Cancellation does not imply rollback. Owned scratch and admission remain
   attached until dispatched work actually completes.
7. New work deepens the canonical module; it does not add a parallel public
   replication interface.

## Plan 1: add a closed-book LTX timing ledger

**Context.** Celld attributes one capture across schema checking, WAL reading,
verification, encoding, file writing, fsync, and checkpointing. Crab records
runtime durability outcomes but cannot yet localize an LTX latency regression.
The later memory and prefetch changes need a trustworthy before/after baseline.

**Change.** Add bounded per-phase capture observations inside the managed LTX
module. The timing source must be injectable or observational only: timing must
never change capture decisions. The runtime adapter may aggregate finite labels;
the default library path remains allocation-conscious and non-blocking.

**Implementation surface.** `src/types.rs`, `src/environment.rs`,
`src/host.rs`, `src/managed.rs`, `src/db.rs`, `src/db/capture.rs`, and
`src/db/checkpoint.rs`.

**Implementation status.** `CaptureBatch` now carries a bounded
`CaptureTiming` value with preparation, WAL-read, verification, encoding,
durable-write, checkpoint, logical-byte, and segment-count fields. The clock
has an injectable monotonic hook with a `SystemClock` default. Timing is
in-memory only: it does not enter LTX bytes, roots, decisions, or fencing, and
there is no asynchronous sink or metric-label expansion yet. Runtime
aggregation and matched workload receipts remain qualification work.

**Acceptance criteria.**

- One capture reports preparation, WAL read, verification, encode, durable file
  write/fsync, and checkpoint time plus relevant byte counts.
- Accounted phases reconcile with total capture time within documented dispatch
  overhead.
- No Cell, path, transaction, or object identifier becomes a metric label.
- Consumers that ignore timing incur no asynchronous work, labels, or
  persistence; the capture path records only fixed-size scalar observations.
- Deterministic tests control time and prove observations never alter output,
  error selection, checkpointing, or fencing.

**Proof.** Unit-test ledger arithmetic and a real managed WAL/LTX capture with
a deterministic clock; run the existing capture, checkpoint, process-kill, and
runtime publication suites; record a repeatable baseline workload before Plans
2 through 4.

## Plan 2: remove unused SQLite lookaside reservation

**Context.** Celld disables SQLite's default 48 KiB lookaside arena before the
first operation on each managed connection. Crab retains three SQLite
connections per active Cell, so the default reservation can consume about
1.37 GiB at 10,000 active Cells before allocator overhead.

**Change.** Disable lookaside once in the shared connection-opening
implementation before any pragma or statement. Keep the three connections and
their 64 KiB page-cache targets; their separate writer, capture, and read-lock
roles remain correctness-sensitive.

**Implementation surface.** `src/managed.rs`; all managed connections already
pass through its shared connection-opening implementation.

**Implementation status.** The shared opener now disables lookaside before the
first pragma, preserves SQLite's native error, and has a status-probe regression
test. Fleet-density RSS and sustained-write receipts remain open qualification
work; this change does not claim those measurements from unit tests.

**Acceptance criteria.**

- SQLite status proves zero lookaside use for all three managed connections.
- Opening a connection fails with the original SQLite source error if the
  configuration call fails.
- Transaction, capture, checkpoint, sparse activation, and recovery behavior is
  byte-identical.
- A declared active-Cell density run demonstrates the expected RSS reduction.
- Durable-write throughput and p95/p99 latency do not regress beyond the agreed
  benchmark tolerance.

**Proof.** Add a connection-level probe test, run both minimal and `replica`
`crab-ltx` suites, then compare matched resident-density and sustained-write
receipts with Plan 1 telemetry.

## Plan 3: make bundles file- and range-backed

**Context.** Native Cell compaction already streams through bounded ranges and
multipart file sources. The bundle module still owns `Vec<u8>` rows and a
complete `Bytes` envelope. Node-log recovery copies frame bodies, downloads the
complete bundle, copies it for decode, and retains it through root preparation.
This makes memory proportional to the configured plan limit. Celld's
`fetch_range` and scratch-file upload demonstrate the useful mechanic, but its
fallback behavior is not acceptable for Crab.

**Change.** Deepen one verified bundle-source module used by encode, reopen,
row inspection, root preparation, and upload. It should spool and hash
incrementally, parse bounded footer/tail data, expose only manifest-selected
row ranges, reuse the existing authenticated LTX inspector, and upload from a
replayable file source. Prepared roots retain descriptors and indexes rather
than the complete body.

**Implementation surface.** `src/bundle.rs`, `src/cell_replica.rs`,
`src/cell_replica/compaction/scratch.rs`,
`../crab-cell-runtime/src/node_log.rs`, and
`../crab-cell-runtime/src/recovery_manifest.rs`.

**Implementation status.** `Bundle` now has a verified file-backed form. Recovery
manifests stream the remote bundle to a runtime-owned session/cell scratch file,
verify its bounded footer, every selected row and the whole-file BLAKE3 on a
blocking worker, then retain only the file path and row metadata. The exact
remote size is reserved from the shared runtime `DiskBudget` and the reservation
is attached to the returned overlay until its temporary file is dropped. The
server restart inventory walks stale session directories, so a process-death
bundle or compaction scratch file is accounted before new work is admitted.
`CellReplica` reads selected rows from that source and uploads the same
replayable source to a private staging key before the existing content-addressed
promotion CAS; staged keys are deleted before preparation returns. In-memory
encode remains available for small node-log construction, but canonical remote
recovery no longer requires resident complete-bundle bytes.

Peak-residency, low-disk, process-kill and 5 GiB bundle qualification remain
open. The implementation does not claim independent memory from the total
bundle size for `pin`, which still receives newly encoded in-memory overlays.

Do not add a default whole-object range fallback. Do not try a different source
after corruption, authorization failure, timeout, or provider error. Correct
the scalability documentation when the implementation and proof land; until
then, it must not claim bundle preparation is streaming.

**Acceptance criteria.**

- A bundle larger than the dirty-memory budget publishes, reopens, compacts,
  and restores with peak memory independent of total bundle size on the
  file-backed recovery path.
- Footer, row extent, segment digest, LTX checksum, predecessor, Cell,
  incarnation, and final position are verified before root preparation.
- Every provider/local transfer stays within its configured chunk ceiling;
  short ranges and trailing bytes fail closed.
- Cancellation, disk full, sync failure, digest mismatch, and multipart failure
  release admission and remove only scratch owned by that attempt; a returned
  overlay retains its exact temporary-file reservation until drop.
- Existing CRB1 bytes and prepared-root semantics remain unchanged unless a
  separately approved format migration is documented.

**Proof.** The `cell_roots::prepare_does_not_write_mutable_keys` regression
decodes a bundle from a file, prepares it through ranged row reads and asserts
that no staging key remains. Runtime recovery tests exercise streamed reopen,
digest-before-parse ordering, corrupt manifests/bundles, exact overlay
preparation, and temporary-file ownership. Add transfer-size and
peak-residency tests, kill during spool/upload, and run node-log recovery
through source loss before claiming the 5 GiB RustFS bundle receipt; compare
restored database length and BLAKE3.

## Plan 4: batch authenticated directory runs before predictive prefetch

**Context.** Crab safely coalesces adjacent frames, but `read_run` performs a
directory lookup for each candidate page. Cached nodes avoid most provider I/O,
yet they are repeatedly parsed and locked. Celld additionally reads SQLite
interior B-tree pages and prefetches likely child windows concurrently. That may
reduce scan round trips, but copying Celld's complete in-memory page map would
weaken Crab's bounded metadata design.

**Change.** First deepen the authenticated directory module so one lookup can
return a verified contiguous locator run. Measure that change alone. Add bounded
B-tree-guided prediction only when matched traces show a material remaining
benefit. Prediction may choose speculative reads; it must never choose the bytes
served for a page.

**Implementation surface.** `src/cell_replica/directory.rs`, the paged read path
in `src/cell_replica.rs`, `src/paged_io.rs`, and the existing sparse VFS tests.

**Implementation status.** The batching and exact-restore slices are
implemented. Sparse `read_run` requests perform one authenticated radix walk
for the requested window, then retain only the first contiguous same-object
span for the existing bounded range read. Restore asks the same authenticated
directory lookup for every contiguous same-object span in a fixed 1 MiB page
window. It fetches spans and up to eight windows concurrently through the
existing host I/O permits, preserves window and page order, verifies every
frame and the final whole-database checksum, and installs only a fresh synced
destination. Window sizing uses the worst legal encoded-frame size, so at most
eight 1 MiB encoded windows are prefetched per restore; decoding and ordered
writing retain only one additional SQLite page.

Cold exact-root opening now fetches immutable segment pages with an
order-preserving concurrency of eight, then validates the flattened descriptor
chain in root order. It does not list objects, read a mutable LTX head, or fall
back to another source. Predictive B-tree prefetch remains intentionally
deferred until request-count and latency traces justify it.

The sparse hydration regression now hydrates a 320-page window, crossing the
256-entry leaf boundary, while retaining the existing range-request bound.

**Acceptance criteria.**

- A run walks and verifies each required directory node once and preserves
  frame hash, page number, and page checksum verification.
- Point reads issue no additional provider requests after prediction is enabled.
- Representative scans reduce page-fault requests and improve p95/p99 latency
  by an agreed material threshold.
- Speculative bytes, requests, workers, queue entries, and decoded-cache use are
  bounded by existing host admission.
- Malformed SQLite pages produce no plan or bounded wasted work; they cannot
  change returned page bytes or surface an unverified page.
- Hydration, foreground faults, overlapping exact-root views, truncation, and
  regrowth retain current isolation behavior.

**Proof.** Use instrumented object storage to count directory/frame requests and
bytes for point lookup, range scan, index scan, and large-row overflow cases.
Run corruption and truncation/regrowth tests, then compare cold and warm p50,
p95, and p99 with Plan 1 telemetry.

The deterministic latency suite injects 5, 20, and 100 ms per object read and
proves lower cold-open and restore p95 than sequential service time, exact
restored bytes, ordered publication, and range overlap. A shared three-permit
host test runs simultaneous restores and observes no more than three object
reads in flight. Warm point reads retain one range request and one injected
latency interval. Short ranges, corrupted ranges, provider timeout, caller
cancellation, local write failure, and final install failure leave no
destination or owned scratch. The existing sparse hydration test continues to
prove coalesced warm reads; a matched production-hardware p99 receipt remains a
release claim gate.

## Plan 5: account for process-death scratch before admitting new work

**Context.** Celld uses anonymous scratch handles for oversized compaction.
Crab uses named scratch because multipart retry can reopen exact ranges. Normal
drop cleans those files, but a process kill can leave them behind. Anonymous
files cannot be copied directly without changing the replayable upload seam.

**Change.** Keep named files because multipart retry needs reopenable ranges, and
account them through the existing stale-session inventory instead of deleting
unknown files during startup. Recovery bundles now use the same session/cell
scratch volume. The inventory rejects symlinks and special entries and reserves
every regular byte before new staging or recovery work is admitted. Do not
weaken retryability merely to obtain anonymous-file cleanup.

**Implementation surface.** `src/cell_replica/compaction/scratch.rs`, injected
filesystem ownership in `src/environment.rs`, the runtime scratch-directory
owner, `crab-cell-runtime/src/recovery_manifest.rs`, and
`crab-http-server/src/local_disk.rs`.

**Acceptance criteria.**

- Restart accounts or removes abandoned scratch before admitting equivalent new
  work.
- Cleanup cannot follow symlinks, cross the configured scratch root, or remove
  another live job's files.
- Multipart retry, cancellation ownership, disk accounting, and exact digest
  verification remain intact.
- If the qualification proves the embedding runtime already provides these
  properties, close the plan with evidence and no implementation change.

**Proof.** The restart-inventory tests account for nested compaction and
recovery scratch, exclude the current session, hold the reservation until the
owner drops, and fail closed on symlinks or capacity overflow. A protected
kill-at-source-spool/output/multipart/final-sync qualification remains required
for release; cleanup is intentionally not attempted because deleting a live
owner's reopenable file would be less safe than accounting it.

## Explicitly rejected Celld deltas

| Celld mechanism | Reason not to adopt |
| --- | --- |
| `ReplicaClient` as the canonical public seam | Exposes listing/discovery and creates a second recovery protocol beside exact roots |
| Mutable epoch head and epoch-chain lookup | Duplicates Cell authority and can disagree with it after ambiguous failure |
| Bounded object listing for compaction | Cell roots already name the exact authenticated inputs; listing adds weaker truth |
| Checksum-disabled capture or compaction | Removes database-state continuity that Crab already verifies |
| Fallback from native read failure to a bundle row | Can hide corruption, authorization, timeout, or provider failures |
| Provider/credential construction in LTX | Duplicates `crab-storage` and composition ownership |
| Complete in-memory page map | Improves locality for some reads by making metadata residency proportional to database size |

## Delivery order and claim gate

Implement Plans 1 and 2 first. Plan 1 creates the measurement seam; Plan 2 is a
small density improvement at one connection seam. Implement Plan 3 next because
it closes a known database-sized allocation on a durability path. Split Plan 4
into directory-run batching and optional prediction so the predictor is justified
by evidence. Plan 5 now closes its accounting half through restart inventory;
protected kill/restart receipts remain a release qualification rather than a
new cleanup path.

Crab may claim that its LTX architecture surpasses Celld when the retained
integrity and authority invariants still pass and Plans 2 through 4 have matched
receipts showing equal or better resource use and latency on identical hardware,
SQLite page size, database distribution, object-store latency, mutation payload,
and durability policy. Exclude JavaScript/V8 time from both systems. Until then,
the accurate claim is narrower: Crab has the stronger exact-root durability
architecture, with fleet density and latency still under qualification.
