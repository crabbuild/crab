# Scalability target and audit evidence

Status: library improvements implemented; target capacity **not qualified**.
Reference: Celld `10cb1303dac710dcb3b557e318e08c855261f68b`, `crates/ltx`.
See [PARITY.md](PARITY.md) for feature and safety differences, the
[standalone compatibility audit](../crab-cell-runtime/docs/standalone-replication-audit.md), and
[the HTTP design](../crab-http-server/next-architecture/crab-ltx.md) for integration.
The implementation order, Cell-runtime ownership rules, qualification gates,
and tagged standalone-contract decision are specified in
[Canonical Cell LTX scaling](../crab-cell-runtime/docs/canonical-ltx-scaling.md).

## Target and hardware profiles

The requested node target is 1,000–10,000 active databases, each typically
100–5,000 MB, and 1,000 transactions/second **aggregate per node**. Target node
envelopes:

| Profile | vCPU | RAM | SSD |
| --- | --- | --- | --- |
| Small | 1–2 | 2–4 GB | 50–100 GB |
| Medium | 4–8 | 8–16 GB | 100–200 GB |
| Large | 16 | 32–64 GB | 500–1,000 GB |

SSD IOPS, network bandwidth, transaction sizes, read/write ratio, latency
objectives and durability latency remain unspecified. No benchmark here
establishes the target on any profile; the hardware ranges are not a claim that
a small node can sustain 10K open databases at 1,000 TPS. See the proposed
[embedded runtime capacity model](../crab-cell-runtime/docs/deployment.md#node-profiles-and-admission)
and [qualification plan](../crab-cell-runtime/docs/delivery.md#capacity-qualification).

Treat active as simultaneously open unless the service explicitly defines an
activation/eviction policy. Do not substitute registered database count for
active database count when reporting capacity.

## Canonical CellReplica 5 GiB receipt

On 2026-09-18, the release `rustfs_cell_replica_scale_load` example ran against
RustFS 1.0.0-rc.1 with a 5,368,709,120-byte incompressible SQLite source grown
through 160 bounded captures (320 immutable segments). It deleted the source,
restored the published root, compacted the complete range, restored the
compacted root, and matched the source BLAKE3/length exactly. `/usr/bin/time -l`
recorded 1,496.96 seconds wall time and 592,805,888 bytes maximum resident set
size (~565 MiB); the largest observed compaction scratch LTX was about 5.1 GiB
on the external qualification volume. This qualifies the canonical native
CellReplica publication path at the design target. It is a provider-specific
library receipt, not evidence for the broader 1,000–10,000-database node target
or protected Kubernetes faults. The standalone epoch-head/paged/scheduler API
decision is recorded as HARD REMOVE and has been executed; its old object
prefixes are outside this receipt and are not reinterpreted.

At 4 KiB pages, a 5,000 MB database contains approximately 1.22 million pages.
The current on-store index uses 60 bytes per page: about 73 MB for one full
index, or 732 GB across 10,000 maximum-size databases. In-memory locators,
B-tree nodes, checksum arrays, overlapping generations and SQLite caches cost
additional memory. These are decimal MB/GB estimates, not measured RSS.
At 10,000 databases the logical data volume spans 1–50 TB; sparse activation
does not imply that all of those bytes must reside on local disk.

## Corrections made in this audit

| Problem | Current mechanism | Reproducible evidence |
| --- | --- | --- |
| Every open Cell paged view started a thread | One shared independent I/O worker for overlapping exact-root views; custom executor hosts share across their clones | `cell_roots::directory_nodes_are_shared_across_exact_root_views` and the `paged_io` driver lifecycle prove shared view state and joined worker shutdown |
| Read-ahead cache multiplied with views | Shared FIFO cache capped at 8 MiB decoded payload, keyed by immutable view/page identity | `paged_io::tests::cache_bounds_payload_and_isolates_pinned_views` checks eviction, byte accounting and view isolation |
| Cell-root appends reloaded every historical index and rebuilt every locator | Authenticated radix copy-on-write reads changed leaves/ancestors, prunes truncated subtrees and reuses untouched digests | `cell_roots::changed_cut_loads_only_touched_directory_nodes` stays below 100 KiB of origin reads after changing one page in a 20 MB database; `truncate_regrow_cannot_reuse_old_locator` restores newly written bytes after shrink/regrowth |
| Initial Cell roots materialized every final locator and encoded directory node | K-way ordered index merge with suffix truncation fences; each 256-page leaf uploads before the next and only radix summaries remain resident | `cell_replica::directory::tests::streamed_tree_matches_canonical_root_without_retaining_objects` matches the canonical 70,000-page root and `cell_roots::initial_streaming_directory_merges_truncation_and_regrowth` restores the newest bytes from a multi-cut initial root |
| Writable Cell activation and each cut allocated/cloned/scanned one checksum per page | Authenticated directory leaves stream to a local 8-byte/page file in 64 KiB chunks; capture keeps a changed-page overlay, reduces truncated suffixes in 64 KiB reads, maintains the aggregate incrementally and persists positional updates only after sealing the LTX cut | `cell_roots::exact_cell_root_opens_sparse_writer_and_publishes_incrementally` checks the disk index and successor restore; `pages::file_backed_truncation_reduces_multiple_checksum_chunks_with_overlay_updates` covers multi-chunk shrink; `host_hooks::cell_checksum_write_failure_fences_after_sealing_the_cut` proves a partial index update cannot keep serving |
| Managed capture and explicit snapshots retained every decoded page, encoded the complete LTX in memory, then reread it as one buffer | Capture feeds one page at a time through the encoder and spools its codec index; snapshots stream to a same-directory scratch file. Both sync, validate metadata plus BLAKE3 through bounded reads, and install atomically without replacement | `host_hooks::capture_and_inspection_bound_each_filesystem_transfer` captures and snapshots incompressible multi-megabyte data while asserting every LTX filesystem transfer stays below 128 KiB; the full capture, fault, checkpoint, restore, and publication suites exercise the canonical paths |
| Cell-native and bundle publication used separate body ownership and inspection paths | Native captures copy into owned scratch and replay through one authenticated inspector; bundle rows use the same inspector and replayable source contract; the prepared root retains descriptors/indexes rather than complete segment bodies | `host_hooks::capture_and_inspection_bound_each_filesystem_transfer`, `cell_roots::prepare_does_not_write_mutable_keys`, and `cell_roots::recovered_overlay_requires_exact_predecessor_and_final_position` |
| Partial compaction downloaded unrelated bodies | Verify the original indexed plan; fetch only selected bodies; authenticate regenerated indexes; compare independently reduced page bytes; verify replacement indexed state | `cell_roots::compaction_streams_large_frames_and_cleans_scratch`: range reads stay below the full LTX body and restored bytes remain identical |
| Long-lived Cell writers exhausted local/remote segment admission | Reverify and prune each exact local batch only after authoritative root confirmation; before later appends, schedule a bounded eight-input level promotion or a pressure-triggered full replacement through the owner CAS | `host_hooks::captured_pruning_retries_after_removal_but_failed_parent_sync`, `cell_roots::scheduled_cell_compaction_promotes_fanout_and_preserves_root`, and `actor::dispatcher_compacts_before_segment_admission_is_exhausted` |
| Independent Cell operations multiplied remote/recovery work | Shared I/O, CPU-job and large-recovery admission; Cell directory nodes and sparse views reuse the host-scoped cache | `cell_roots::directory_nodes_are_shared_across_exact_root_views` and `environment::cancelled_waiters_do_not_release_running_job_or_recovery_admission` |
| Cancelling a waiter could release capacity before its work stopped | CPU/recovery permits travel with dispatched non-cancellable closures; network child tasks abort on cohort drop | `environment::cancelled_waiters_do_not_release_running_job_or_recovery_admission` and `host_hooks::cancelled_cell_prepare_releases_scratch_without_publishing_a_root` |
| Temporary recovery capacity could become attached to returned handles | Strip dirty and recovery reservations before returning prepared roots, sparse writable handles or resumed writers | `cell_roots::prepared_cell_handles_release_dirty_admission` |

Earlier corrections remain covered: snapshot returns ownership of pending cuts;
native and bundle appends verify only new LTX bodies against pinned predecessor
state; inheritance admits destination limits before remote reads; singleton
segments advance through scheduled levels. A lost CAS response still requires
exact-root reconciliation, not automatic SQL replay.

The compaction change does not weaken authentication: unselected bodies are not
scrubbed, but their authenticated indexes prove original intermediate states.
Every selected body passes full LTX verification and must reproduce its pinned
index digest. The compactor's output must match an independently reduced set of
selected page bytes. The replacement indexed chain must also validate before CAS.
Full restore continues to verify every body and intermediate database checksum.

## Resource ownership now

| Resource | Default scope and ceiling | What it does not bound |
| --- | --- | --- |
| Provider operations | 32 shared process-wide permits, including retries | Caller tasks waiting for admission; total retained input bytes |
| Codec/recovery blocking jobs | Available CPU count capped at 16, process-wide | Synchronous caller-owned SQL/capture/snapshot jobs |
| Capture/recovery/compaction dirty work | CPU-capped shared defaults; the HTTP server instead derives 64 MiB slots from 25% of its Cell budget and CPU credits | Measured allocator overhead |
| Full-job scratch | One-MiB shared permits; restore/resume reserve two images plus 64 MiB, and Cell compaction also reserves exact source-index bytes before bodies | Active WAL/pending-cut growth and unrelated Git/LFS scratch |
| Full restore/resume/bundle/remote compaction | Shared recovery slots acquired before body downloads and nested inside dirty admission | Per-job RSS outside the configured dirty reservation; retained result buffers |
| Paged fault driver | One shared default worker; 32 concurrent faults and 256 queued requests | SQL worker count; custom independently configured executor hosts |
| Decoded read-ahead cache | 8 MiB payload per shared driver | SQLite caches, active fetch buffers, metadata and cache bookkeeping |
| Fault latency | 30-second deadline including queued wait | Arbitrary blocking custom transports/executors that do not yield |
| Database/input admission | Per-database `Limits`; three retained 64 KiB SQLite page caches; the HTTP server derives active count from page-cache, fixed 64 KiB native-state and file-descriptor pools | Actual native RSS variance and dynamic local SSD quota |

`Host::{with_io_slots,with_job_slots,with_recovery_slots,with_dirty_slots}` accept shared
`Arc<tokio::sync::Semaphore>` values. Configure a host once and clone it across
replicas to share a service budget. No new environment variables or provider
dependencies are introduced. Closing a semaphore rejects admission; it does not
cancel already running jobs. Zero permits intentionally pause admission.
Use a dedicated independently progressing worker for paged I/O, never the same
bounded blocking pool whose SQL calls synchronously wait for page faults.

These are count-based ceilings. They are not a memory reservation system, a
fair multi-tenant scheduler or a deadline policy for application requests.
The library default 256 MiB database ceiling intentionally does not admit 5 GB
databases. The HTTP server supplies a repository profile with a 5 GiB database
ceiling, 64 MiB capture ceiling, 6 GiB object ceiling and 8 GiB plan ceiling;
its separately derived dirty/recovery slots bound concurrent large operations.

## Remaining implementation gates

1. **Finish bounded authenticated metadata residency.** Cell roots now use an
   authenticated block-addressable radix directory, and incremental publication
   reads/copy-on-writes only changed paths while preserving truncation/regrowth
   coverage. Initial construction now streams its final locator merge and radix
   uploads while retaining the already authenticated index bytes. Writable Cell
   activation streams its eight-byte-per-page checksum index to local disk and
   capture retains only the changed-page overlay. The shared verified
   directory-node cache is bounded, restart-persistent, and charged to the
   shared disk budget; exported cache
   hit/corruption/fill metrics and zero-origin warm-restart qualification remain.
   Sparse hydration now walks each requested directory window once and then
   coalesces only a verified contiguous frame prefix; predictive B-tree
   prefetch remains unqualified and is not enabled.
   Finish those release gates before claiming the 5 GB/10K target; do not add an
   implicit fallback reader.
2. **Streaming large-database operations.** Managed WAL capture now encodes
   directly to an atomic local file with a spooled codec index, and validates
   segment metadata and BLAKE3 without rereading the whole cut into memory;
   explicit snapshots use the same bounded page pipeline and an atomically
   installed scratch file.
   Cell capture checksum updates are incremental and disk-backed; truncation
   reduces its removed checksum suffix through fixed 64 KiB reads. Cell-native
   and bundle append preparation now uses replayable scratch/range sources and
   shared authenticated inspection; remote bundle recovery spools to a bounded
   session/cell scratch file and retains only verified row metadata. Stale
   session inventory accounts abandoned recovery or compaction bytes before
   new admission, while native recovery and compaction still require
   qualification for database-sized decoded buffers, low-disk behavior and
   provider failures. Keep cryptographic body/index binding and exact output
   verification.
3. **Resident lifecycle and SQL scheduling.** The server has bounded activation
   queues, a shared SQL executor, per-database serialization and FD/page-cache/
   native-state-derived active admission. Managed sessions own three 64 KiB-cache
   SQLite connections each. Capture/recovery/compaction share CPU- and memory-
   derived dirty slots, and full restore/compaction use byte-weighted scratch
   admission. Qualify the fixed estimates, unify active WAL/Git/LFS disk
   accounting and implement explicit warm transitions. Resident/sparse/hydrating
   lifecycle and bounded eviction are wired; reusable crash-safe local warm
   reopening and zero-origin warm-restart proof remain. Every acknowledged root
   must survive eviction.
4. **Retention and collection.** The HTTP runtime now owns owner/head CAS,
   exact-response gating, ambiguous-publication reconciliation and scheduled
   representation compaction. Application backup pins now bind release metadata,
   revision-pinned catalog pages, exact controls and every digest-verified object
   reachable from the captured LTX roots. Same-bucket restore conditionally
   copies and re-verifies those objects before publishing unowned destination
   controls and commit pointers. Offline maintenance now drains signed node and
   backup-worker advertisements, verifies live controls and retained pins into
   a disk-backed mark set, streams the application inventory, and reclaims only
   recognized old unreachable immutable objects in bounded batches. Implement
   cross-provider export. Library epoch CAS is not a lease. Per-repository
   bundle publication is not node-wide atomic group commit or shared-bundle
   retention.
5. **Measured node qualification.** Define hardware and service SLOs, then prove
   capacity under realistic database sizes, skew, write amplification, object
   storage latency, simultaneous takeover, compaction and failures. The server
   exports bounded runtime metrics and ships a read-and-mutation JSON load harness;
   sustained production mutation, residency, takeover and compaction matrix receipts remain.

These gaps prevent claiming full Celld operational/performance parity or a
production-ready 10K-database backend. Celld's bounded restore concurrency and
selected-input compaction informed this audit; its surrounding actor/node-log
service does not become part of Crab merely by reusing the LTX crate.

The deliverable design for that missing node-log service, dual durability gate,
and takeover interlock is
[Follower durability and warm failover](../crab-cell-runtime/docs/failover-and-followers.md).
It uses one multiplexed log per owner session rather than one full follower
database per repository.

## Qualification plan

Run small deterministic correctness tests locally. Large workloads belong in a
dedicated node/CI environment with declared resource limits, not a developer's
checkout. Record dataset generation and provider versions with each result.

| Stage | Workload | Required evidence |
| --- | --- | --- |
| Single-database correctness | 100 MB and 5 GB; varied page sizes, incompressible and compressible rows | Exact recovery after source loss; corruption fails closed; peak RSS/scratch bytes recorded |
| Residency | 1K then 10K simultaneously active databases, explicit size distribution | Bounded RSS, threads, FDs and SSD use; no hidden substitution of cold registrations for active handles |
| Sustained writes | 1,000 aggregate TPS, uniform then hot-key skew; declared pages changed/transaction | Acknowledged/recovered transaction identity; latency percentiles, WAL/upload amplification and backlog recorded |
| Maintenance overlap | Hydration, level compaction and bursts of takeover during writes | Foreground latency within agreed SLO; bounded memory and queues; no starvation or head rewind |
| Failure/restart | Cancel, kill, disk-full/fsync failure, provider timeout and ambiguous CAS | No lost acknowledged mutation; no stale-owner acknowledgement; retained roots recover exactly |
| Platform/provider | Linux production filesystem and RustFS; other supported providers separately | Real protocol/durability proof rather than in-memory-only tests |

Current live proof is the isolated RustFS library fixture: source-loss restore,
paged SQL, CAS conflict handling, inherited bundle, sparse writes, checkpoint,
partial compaction and exact resume. Its ephemeral server storage does not prove
RustFS power-loss durability or HTTP behavior. See [verification](README.md#verification).
