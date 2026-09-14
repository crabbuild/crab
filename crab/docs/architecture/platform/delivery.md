# Implementation work packages and acceptance tests

[Index](README.md). Packages below are ordered dependencies with concrete
changes and tests. Contract files define the target; passing their validator
does not establish a working runtime.

## Existing implementation to reuse

| Source | Current behavior | Required change |
| --- | --- | --- |
| [managed.rs](../../../../crates/crab-ltx/src/managed.rs) | Typed mutation callbacks, capture ownership and a temporary SQLite `query_only` read boundary | Keep raw connection access inside the runtime; typed application SQL authorization is implemented above this layer |
| [cell_replica.rs](../../../../crates/crab-ltx/src/cell_replica.rs) | Native cuts prepare immutable Cell roots; cold reads and sparse writable activation use exact digest-pinned radix paths; incremental publication copy-on-writes only changed directory paths and safely prunes truncation | Add prepared compaction/bundles, shared node cache, streaming initial construction and directory-backed capture checksums |
| [replica.rs](../../../../crates/crab-ltx/src/replica.rs) | Standalone immutable manifest plus per-epoch mutable head | Keep existing callers working; Cell runtime uses only `CellReplica` and never treats this head as authority |
| [append.rs](../../../../crates/crab-ltx/src/replica/append.rs) | Shared native/bundle append verification | Reuse verification under the prepared-root API |
| [paged.rs](../../../../crates/crab-ltx/src/paged.rs) | Authenticated but resident page map; sparse writable SQL | Bounded directory nodes/cache and capture checksum tracker |
| [environment.rs](../../../../crates/crab-ltx/src/environment.rs) | Filesystem/executor hooks and count admission | Byte reservations held through actual job completion |
| [store.rs](../../../../crates/crab-storage/src/store.rs) | Conditional updates; ambiguous update not retried | Preserve behavior; runtime owns CAS reconciliation |
| [cell_layout.rs](../../../../crates/crab-storage/src/cell_layout.rs) | Typed application/Cell/incarnation object paths | Reuse from authority, immutable-root and backup code; never rebuild path strings in callers |
| [crab-cell-runtime](../../../../crates/crab-cell-runtime/src/lib.rs) | Stable IDs/control authority/schema, verified CAS catalog, worker-owned bootstrap, exact-root sparse activation, fixed SQL workers, ordered bounded reads, FIFO publication, retry, unknown outcomes, five-second SQL/native watchdog, bounded owner renewal, idle acquisition, observed takeover, per-Cell drain, node-wide terminal drain with explicit worker join, transactional scheduler summaries, revision-pinned due scans and bounded typed Tick, typed registry/CellClient and bounded authorized SQL/KV/Queue/Workflow capabilities plus exact native activity execution | Add sparse page-I/O deadlines, automatic fenced recovery, private peer routing, Workflow effects, catalog-driven activity scheduling and scheduler liveness/routing |
| [sql.rs](../../../../crates/crab-cell-runtime/src/sql.rs) | Typed 128-statement/1-MiB batches, read/write classification, 1,000-row/1-MiB materialization, scoped SQLite authorizers, registered codecs and a role-checked SqlCell proven through publish and exact-root restore | Use the completed handle from the repository HTTP adapter |
| [kv.rs](../../../../crates/crab-cell-runtime/src/kv.rs) | Normative schema install, bounded atomic check/write, stable versions, TTL get/list/cleanup, binary pagination and typed scope-sharded KvNamespace/registry codecs | Add scheduler cleanup invocation |
| [queue.rs](../../../../crates/crab-cell-runtime/src/queue.rs) | Normative schema, producer dedup, bounded claim, token validation, lease mutations/reclaim and retention cleanup plus typed producer-sharded QueueNamespace and registry codecs | Add DLQ effects and native polling scheduler |
| [workflow.rs](../../../../crates/crab-cell-runtime/src/workflow.rs) | Normative schema, pinned and registry-verified definitions, deterministic start/signal/timer transitions, cancellation, bounded action persistence, typed workflow-ID-sharded dispatch, published activity claims and validation, exact compiled native activity dispatch, durable heartbeat/completion/retry and terminal cleanup | Add effects, catalog-driven shard polling and bounded multi-activity orchestration |
| [effects.rs](../../../../crates/crab-cell-runtime/src/effects.rs) | Stable source effect IDs/digests, bounded claim/lease/retry/delivery, target inbox dedup/savepoint isolation and sender/inbox cleanup horizons | Add Workflow/Queue adapters, compiled codecs, authenticated peer delivery and native supervision |
| [scheduler.rs](../../../../crates/crab-cell-runtime/src/scheduler.rs) | Derives the earliest durable work/lease/expiry/retention deadline inside bootstrap and every command transaction; publication binds it to the exact pending root | Add catalog shard assignment/scanning and bounded idempotent Tick dispatch |
| [registry.rs](../../../../crates/crab-cell-runtime/src/registry.rs) | Startup-only static module registration, migration/schema/namespace validation, exact descriptor/function/Workflow-definition/native-activity inventory matching, canonical module/release digests, typed Command/Query trampolines, transaction-scoped compiled dispatch and async activity dispatch with registry-owned primitive shard counts | Add server runtime composition and compatibility activation |
| [application.rs](../../../../crates/crab-cell-runtime/src/application.rs), [release.rs](../../../../crates/crab-cell-runtime/src/release.rs) | Canonical immutable root identity, exact-winner initialization, immutable descriptor upload, expected-revision prepared release CAS and checked status | Add compatibility inventory, node eligibility, Cell migration and activation CAS |
| [codec.rs](../../../../crates/crab-cell-runtime/src/codec.rs) | Canonical bounded scalar/bytes/text/option encoding, strict full-input decoding and finite normalized floats | Add independent fixtures for each product codec and peer integration |
| [client.rs](../../../../crates/crab-cell-runtime/src/client.rs) | Typed local command/query/Resolve capability, namespace/code/schema/incarnation checks, canonical operation digest, outcome classification and minimum receipts over the FIFO publication actor; typed KV, SQL, Queue and Workflow handles use it | Add authenticated peer transport and stale-owner retry |
| [HTTP app_storage.rs](../../../../crates/crab-http-server/src/app_storage.rs) | Existing object application storage | Native repository Cell integration after runtime acceptance |

Reuse existing [publication tests](../../../../crates/crab-ltx/tests/publication.rs),
[host tests](../../../../crates/crab-ltx/tests/host_hooks.rs) and
[RustFS fixture](../../../../crates/crab-ltx/tests/remote.rs). They are library
mechanics evidence; none already proves multi-owner HTTP output gating.

## Work package 1: immutable LTX preparation

`CellReplica::prepare` and `open_root` now implement native-cut scope validation,
canonical root/descriptor codecs, immutable dependency upload and the persistent
radix directory format. `Control::publish_prepared` binds that checked proposal
to exactly one Cell/incarnation/predecessor before the authority CAS. Incremental
native publication now authenticates and copy-on-writes only changed leaves and
ancestors, reuses untouched subtree digests and prunes truncation without loading
discarded leaves. Complete this package by sharing preparation with bundles and
compaction, streaming initial directory construction, replacing the dense
capture/activation checksum array and adding the shared node cache. Exact
Cell roots now load authenticated directory checksums without LTX bodies and
open a sparse writable continuation through the existing VFS.
Existing standalone `Replica` callers retain their current API; Cell runtime
code must not call its mutable epoch head.

Current local coverage is in `crates/crab-ltx/tests/cell_roots.rs` and
`crates/crab-cell-runtime/tests/publication.rs`. Add the remaining cases:

- `prepare_does_not_write_mutable_keys`: instrument transport; assert no head/
  control update during native, bundled and compaction preparation.
- `prepared_root_restores_after_source_loss`: real RustFS upload, delete local
  source, reopen exact root and compare SQL and database checksums.
- `root_rejects_other_cell_or_incarnation`: reuse a valid digest in another scope.
- `snapshot_transfers_all_pending_cuts`: interleave transaction/checkpoint/snapshot;
  verify no local committed cut disappears from the caller-owned batch.

`exact_cell_root_opens_sparse_writer_and_publishes_incrementally` currently
proves local source deletion, exact sparse activation and successor publication.
`changed_cut_loads_only_touched_directory_nodes` proves a one-page update does
not reload a 20 MB snapshot index; `truncate_regrow_cannot_reuse_old_locator`
proves a truncated locator cannot reappear after database growth.

Exit: default/replica builds and existing tests pass, plus live source-loss test.

## Work package 2: runtime command and authority

Identity derivation, strict control encoding, transition predicates, ETag updates,
runtime.sql installation and the synchronous command-ledger executor are
implemented. The executor runs application changes inside a savepoint, records
success or rejection once, captures post-commit cuts, blocks later commands and
releases the result only after the exact bound root is confirmed. The fixed SQL
worker layer is also implemented: 1-16 OS threads, 256-command bounded shard
queues, stable Cell-ID routing, a node-wide 10,000-Cell admission ceiling, and
continued execution after an accepted caller is cancelled. Exact-root lost-response
reconciliation and renewal-token refresh are implemented without SQL replay.
The node dispatcher now adds 64-request/8-MiB Cell admission, node byte admission,
single-flight FIFO publication, 100/200/400/1,000-ms storage retry, structured
unknown outcomes and accepted-work drain without a permanent task per Cell.
Its terminal shutdown closes node and per-Cell admission, drains ingress accepted
before the shutdown marker, publishes accepted work, then closes every active
SQLite handle and releases every owned control. It closes the shared worker pool
and joins all fixed SQL threads before returning; direct pool shutdown rejects
active Cells so it cannot bypass the authority-release order.
Immutable catalog pages, CAS heads, concurrent merge, collision rejection,
proof-before-control creation and local-session activation checks are implemented.
Exact-root restore now reserves active-Cell admission before I/O, prepares the
authenticated checksum index, opens SQLite on the assigned worker, verifies
`sys_meta` against control/root, and reloads authority before serving. Complete
bootstrap now exclusively creates the local database with the replica host,
installs runtime plus application schema in one worker transaction, captures and
publishes its initial root before returning a handle, and releases activation
capacity after initialization failure. Caller-opened runtime activation has been
removed. Exact-root lost-response reconciliation also rejects a subsequent
takeover instead of letting the old executor serve. Ordered queries now share
Cell/node admission and the mutation FIFO, run only after prior publication, use
SQLite `query_only`, and enforce declared result bytes. Complete deadlines/SQLite
interruption, fenced takeover recovery,
and panic supervision. All transitions use the existing Store conditional
primitives, preserving sources.
Drained and orphaned activations close their SQL worker first, then release the
same observed authority record to `Idle`; transient CAS failures retry across
pure renewals and exact lost responses reconcile without reviving stale owners.

Add `crates/crab-cell-runtime/tests/publication.rs`:

- `lost_cas_response_resolves_without_sql_replay`: drop response after origin
  accepts CAS; same request increments once and returns stored result.
- `takeover_preserves_winning_publication`: enumerate CAS orderings; any returned
  success survives successor activation, including delayed old-owner response.
- `resolve_absent_waits_for_inflight_publisher`: hide pending upload from a
  historical root; Resolve must return UNKNOWN until drain/fence proves absence.
- `client_drop_retains_pending_cut_and_permits`: cancel at commit/capture/upload;
  supervisor completes or fences, and memory accounting remains reserved.
- `business_rejection_rolls_back_effects_but_records_outcome`: savepoint rollback
  preserves only the rejection/dedup record after publication.

Current worker coverage is in `crates/crab-cell-runtime/tests/workers.rs`:

- `fixed_workers_own_execute_prepare_confirm_and_dedup` proves worker ownership,
  pending-cut retention, exact-root confirmation, replay dedup and drained removal.
- `cancelled_waiter_does_not_cancel_an_accepted_sql_command` aborts the awaiting
  Tokio task after handler entry and proves the worker still commits and retains
  its pending publication for later preparation and confirmation.
- `active_cell_admission_is_global_and_released_after_drain` places Cells on
  different worker shards and proves the node-wide ceiling is returned only by
  a completed drain.

Current dispatcher coverage is in `crates/crab-cell-runtime/tests/actor.rs`:

- `dispatcher_serializes_and_publishes_commands_before_drain` proves two queued
  commits receive sequences one and two, are visible after SQLite close, and the
  authoritative control is released to `Idle` with no owner.
- `cancelled_command_waiter_is_resolved_by_original_identity` proves cancellation
  does not stop publication and retry returns the first stored result.
- `per_cell_request_admission_caps_inflight_and_queued_commands` proves the 64th
  retained command exhausts admission while one handler is in flight.
- `node_byte_admission_rejects_before_sql_execution` proves byte rejection is a
  pre-SQL outcome; `post_commit_publication_failure_returns_resolvable_unknown_outcome`
  proves the opposite boundary carries the original identity and digest.
- `runtime_shutdown_drains_accepted_work_and_releases_all_owners` blocks one
  accepted mutation across shutdown, rejects later work, proves that mutation is
  published, and verifies two independently owned Cells both become `Idle`.
- `proven_handler_rollback_keeps_the_cell_servable` proves application errors do
  not inherit infrastructure fencing.
- `source_loss_takeover_restores_exact_root_and_continues_publication` deletes
  bootstrap and first-owner local state, changes owner session, restores from the
  published root, resolves a predecessor request from that root and advances the
  command sequence again.
- `failed_bootstrap_keeps_control_unpublished_and_releases_cell_capacity` proves
  application migration rollback leaves no root and returns the one active-Cell
  slot for a successful retry at a fresh destination.
- `query_waits_for_preceding_publication_and_cannot_write` proves a concurrent
  read observes the preceding published mutation, a write through the query
  callback is rejected, and the Cell remains readable.
- `resolve_distinguishes_committed_absent_conflict_and_expired` proves the local
  typed resolution states and digest binding;
  `resolve_waits_for_inflight_publication_and_returns_unknown_after_fence` proves
  a queued resolver cannot claim absence while its publisher is unresolved.
`publication_rebases_over_a_pure_lease_renewal_without_sql_replay` covers the
coordinator's latest-token retry path;
`published_root_observed_after_takeover_fences_the_old_executor` proves that
durability confirmation does not retain stale serving authority.

Current catalog coverage is in `crates/crab-cell-runtime/tests/catalog.rs`:

- `catalog_provision_is_idempotent_and_precedes_control` proves immutable catalog
  reachability exists before strict control creation and exact lost-create adoption.
- `concurrent_catalog_writers_merge_entries_on_one_shard` races two writers and
  proves both entries survive the ETag loop.
- `catalog_rejects_conflicting_bootstrap_contract_for_one_cell` and
  `catalog_page_digest_is_checked_before_entry_use` prove collision and content
  integrity boundaries. Actor coverage also rejects a valid control owned by a
  different node session.

Exit: two local runtime processes against isolated RustFS pass counter increment,
owner kill, full source-directory removal and query recovery.

## Work package 3: bounded storage

The authenticated radix directory and incremental copy-on-write update are now
implemented. Continue with streaming initial root construction, external-merge
compaction, shared bounded node caching and directory-backed capture checksum
updates. Remove whole-DB buffers from active paths; do not preserve a second
unbounded implementation as fallback.

Add tests `directory_hash_and_coverage_reject_missing_page`,
`truncate_regrow_cannot_reuse_old_locator`, `changed_cut_loads_only_touched_nodes`,
`five_gb_restore_stays_within_job_reservation`, and
`sparse_fault_pool_progresses_under_saturated_sql_workers`. Run large tests in
dedicated infrastructure. Assert actual peak RSS/scratch and requested bytes,
not just configured semaphore counts.

Exit: 5,000 MB incompressible source-loss restore, low-disk failure and concurrent
capture/compaction pass without memory proportional to database size.

## Work package 4: native API and embedded Crab routing

Implement registry.rs/api.rs and the typed Command/Query/WireValue boundary.
Generate private message types from peer.proto inside crab-cell-runtime; no
protocol facade crate or public gRPC service. Implement canonical digest codec,
bounded decoding and outcome-aware retry/Resolve once in CellClient.

Treat the module author as a Crab contributor. A capability is not delivered
unless one pull request contains its migration and digest, stable codecs and
fixtures, native bindings, authorized product-route adapter and end-to-end test.
Do not add a module package format, scaffold command, external application SDK or
per-repository module selector. The complete server image is the sole artifact.
Do not keep an abstraction for a hypothetical guest language unless the same
change gives it a concrete compiled Crab caller and it reduces the canonical
Rust path's ownership or duplication.

Make `crab-http-server/src/cells.rs` the only composition root. Its static module
descriptors and bindings must produce one canonical registry or fail startup.
Add the read-only `cells release inspect --json` command and run it in the image
pipeline so release evidence is derived from the built binary, not a parallel
manifest. `crab-cell-runtime` must not depend on server/auth/Git crates, and no
application registration, module upload or handler replacement is accepted after
`RegistryBuilder::finish`.

In crab-http-server, construct the runtime from the existing resolved Store and
register repository commands in cells.rs. Add private peer forwarding to the
management router and preserve product HTTP authorization in app.rs. Wire one
real comment create/read path to the runtime in the integration fixture before
expanding the migration. Do not ship a selectable second persistence backend.

Add tests:

- `local_and_peer_command_have_identical_digest_and_outcome`: same typed input
  and identity execute once regardless of the first ingress node.
- `i64_max_roundtrips_peer`, `unknown_codec_rejected_before_admission` and
  `expired_identity_cannot_reexecute_after_dedup_gc`.
- `unknown_outcome_keeps_request_identity`: cancellation/transport loss returns
  a resolvable identity, never an automatic new submission.
- `compiled_registry_rejects_descriptor_binding_drift`: missing, extra or
  duplicate function bindings fail before listener readiness.
- `release_inspect_matches_running_registry`: the built binary's inspection
  bytes and startup release digest are byte-identical.
- `repository_route_uses_the_single_static_registry`: a real HTTP command reaches
  the registered native handler; no route can supply code, a handler name or a
  caller-selected command ID.
- `peer_auth_rechecks_repository_membership`, `forward_hop_limit_preserves_auth`,
  `public_router_has_no_primitive_or_peer_endpoint` and
  `unregistered_owner_endpoint_receives_no_credentials`.
- Compile-fail cases for moving CommandContext across threads, returning a
  borrowed statement and using an async command callback.
- `tests/sql.rs` now checks that application SQL cannot attach, configure SQLite,
  control transactions, load extensions or access protected state directly or
  through a trigger/view. It also proves bounded typed inputs/results, read-only
  classification and authorizer cleanup. This proves SQL API constraints, not a
  Rust sandbox. Add the registry/actor acceptance case when `CommandContext::sql()`
  lands.
- `native_watchdog_fences_without_recycling_worker_permits`: block native code,
  expire its budget, verify no further admission or premature permit release;
  unblock it for test cleanup and prove its tentative writes never publish.

Current registry and client coverage in `crates/crab-cell-runtime/tests/registry.rs`
and `crates/crab-cell-runtime/tests/client.rs` proves
registration-order-independent release bytes, bounded compiled command/query
execution, schema rejection, startup failure for missing or extra function
bindings, canonical operation-digest fixtures, published typed execution,
idempotent replay, durable rejection rollback and minimum receipts. Authenticated
CellClient forwarding remains required. `crab-http-server` now supplies the
first compiled repository descriptor/migration, and its binary command and tests
prove release inspection returns the same deterministic registry bytes.

Fuzz peer decoding, signed envelope validation and path/identity encoding.
Pin independent command/input/output fixtures for every registered codec version.
Use `cargo tree -p crab-http-server` plus source-policy checks to reject dynamic
loader, V8, Wasm and guest-runtime dependencies unless this architecture is
replaced through a separately approved design.

Exit: the browser creates a comment through node A while node B owns its Cell;
kill B and remove its local state, then fetch the same comment through A from
RustFS. Repeat a lost response using the original submission ID and prove one row.

## Work package 5: primitives and durable scheduler

Implement migrations and SQL algorithms in primitives.md exactly once in Rust.
Add sys_inbox/sys_effects handler and private authenticated DeliverEffect
protocol; browser callers cannot forge source effect identities. Implement
catalog scanner and Tick through the normal actor command loop.

Current implementation includes the bounded Tick transaction and typed actor
command, including stale-root rejection, timer dispatch and terminal activity
events. It also includes revision-pinned page enumeration, 32-control due
batches and deterministic rendezvous assignment. Node-progress advertisements,
fallback, remote/idle route orchestration and retry remain. Active-local routing
already revalidates dispatcher admission and the scanned control identity before
submitting Tick through a reconstructed capability.

| Test file | Required cases |
| --- | --- |
| tests/kv.rs | scoped routing, delete/recreate token, expired checked put/list, all-or-none multi-key mutation |
| tests/queue.rs | producer dedup after ack, expired-token ack, delayed claim emission, retry/extend and DLQ delivery response loss |
| tests/workflow.rs | signal identity conflict, stale attempt completion, cancellation, pinned definition and duplicate timer firing |
| tests/scheduler.rs | drop every wake notification, evict all due Cells, restart scanner and observe eventual published progress |
| tests/effects.rs | target commits/source response lost, source retries same effect; inbox retention exceeds sender horizon |

Current implementation already dispatches signals and activity completions by a
persisted digest across a module's current-plus-retained definition inventory.
The remaining rollover gate must inventory timers and other retained work and
reject takeover when any required digest is absent.

Exit: a compiled Rust workflow schedules a native asynchronous activity; kill
the executing process after its external idempotent effect. A replacement retries
safely and records one completion event. Kill every runtime and recover pending
queue/timer/outbox state from RustFS. Test transitions with deterministic fixtures
and explicit old/new definition dispatch, not language-engine behavior.

## Work package 6: releases, cutover and operations

Embedded release descriptors, compile-time registry validation, read-only
inspection, immutable root identity, descriptor upload, prepared-state CAS and
status are implemented in the existing `crab-http-server` binary. Continue with
compatibility analysis, activation, namespace provisioning, migration and drain
procedures from deployment.md, reusing its image build, storage/auth construction,
health routes and metrics exporter.

Add `migration_digest_conflict_blocks_activation`,
`lost_activation_cas_reconciles_published_cells`,
`takeover_rejects_binary_missing_pinned_definition`,
`old_request_codec_survives_rollout`,
`maintenance_gate_alone_cannot_fence_old_writer`, and
`restart_reads_published_schema_not_local_wal`.
Verify all retained workflow/queue/outbox data has executable code after rollover;
an image digest or artifact object by itself is not sufficient proof.

Use three Crab Pods, RustFS and the existing React application in dedicated
Kubernetes qualification. Exercise rolling drain and restore while removing a
node's entire local volume. Combine Git clone/push, browser mutations and native
activity execution to measure shared-process interference. Test offline GC only
in a disposable Cell namespace with real writer revocation; prove unrelated
Git/LFS/release-asset objects remain untouched.

Hard-cutover acceptance inventories every app_storage caller and application
document family. Import exact IDs, memberships, sequences and relationships;
verify create/edit/list/search plus permission/conflict/unknown-outcome UI states.
Prove native Git receive and fetch retain their existing publication contracts.
Remove old runtime JSON callers; keep an importer only in maintenance tooling.

Exit: existing Crab build/deploy workflow starts one binary per node, serves all
repository application functions through the native runtime, upgrades compiled
schema/definitions, and restores an isolated copy with effects disabled. Source
files and tests above must exist and pass before reporting implementation complete.

## Capacity qualification

Run every [hardware profile](deployment.md#resource-profiles-and-capacity-targets)
separately. Sweep 1K then 10K open databases with a declared distribution: 70%
100 MB, 25% 500 MB, 5% 5,000 MB, plus a worst-case 5,000 MB cohort. Object-store
capacity belongs in dedicated infrastructure; local SSD holds admitted working
sets. Test uniform traffic and 50% of traffic concentrated on 1% of Cells.

Offer 1,000 user mutation commands/s for 60 minutes, each modifying 1, 8 and 64
4-KiB pages in separate runs. Report user commands independently of request
dedup, queue claims/acks, renewal and scheduler transactions. Repeat with concurrent Git/browser traffic,
hydration/compaction and a ten-node-equivalent takeover burst.

Record actual TPS, latency percentiles, queue age, timer lag, control requests,
write amplification, RSS/FD/thread/scratch peaks and recovery time. Initial
acceptance gate: zero lost acknowledged operations, bounded configured memory/
disk, p99 user latency <=1 s, scan lag <=5 s, and no growing publication backlog
during the steady portion under an origin with measured p99 CAS <=50 ms.
These are test gates, not claims that all profiles currently meet them. Publish
the maximum passing envelope and any failed target; never relabel cold Cell
registrations as simultaneously open databases.

## Repeatable design-contract validation

Run from repository root:

```sh
node crab/docs/architecture/platform/validate.mjs
git diff --check
cargo fmt --all -- --check
```

Prerequisites: Node.js, sqlite3 with STRICT-table support, protoc with proto3
optional support. The validator creates only an isolated temporary descriptor
directory, removes it afterward, and uses in-memory SQLite. It checks all four
schema compositions, lease null/state constraints, stale queue-token updates,
workflow event uniqueness/foreign keys, completion token pairs, Protobuf compile,
signed-64-bit and native-command peer roundtrips, absence of public service
definitions, document links and Markdown fences/whitespace.

These executable inputs make the design reviewable before implementation.
They do not claim native runtime/retry, cloud durability, HTTP integration or
capacity tests already pass. Each work package's named tests remain required delivery
evidence, alongside existing crate checks and provider-qualified CI.
