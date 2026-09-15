# Implementation work packages and acceptance tests

[Index](README.md). Packages below are ordered dependencies with concrete
changes and tests. Contract files define the target; passing their validator
does not establish a working runtime.

## Existing implementation to reuse

| Source | Current behavior | Required change |
| --- | --- | --- |
| [managed.rs](../../../../crates/crab-ltx/src/managed.rs) | Typed mutation callbacks, capture ownership and a temporary SQLite `query_only` read boundary | Keep raw connection access inside the runtime; typed application SQL authorization is implemented above this layer |
| [cell_replica.rs](../../../../crates/crab-ltx/src/cell_replica.rs) | Native and canonical Cell-selected bundle cuts prepare immutable roots; exact range/full compaction produces representation-only prepared roots; cold reads and sparse writable activation use exact digest-pinned radix paths backed by a process-wide bounded directory cache; incremental publication copy-on-writes only changed directory paths and safely prunes truncation | Add streaming initial construction, external-merge compaction and directory-backed capture checksums |
| [replica.rs](../../../../crates/crab-ltx/src/replica.rs) | Standalone immutable manifest plus per-epoch mutable head | Keep existing callers working; Cell runtime uses only `CellReplica` and never treats this head as authority |
| [append.rs](../../../../crates/crab-ltx/src/replica/append.rs) | Shared native/bundle append verification | Reuse verification under the prepared-root API |
| [paged.rs](../../../../crates/crab-ltx/src/paged.rs) | Authenticated but resident page map; sparse writable SQL | Bounded directory nodes/cache and capture checksum tracker |
| [environment.rs](../../../../crates/crab-ltx/src/environment.rs) | Filesystem/executor hooks and count admission | Byte reservations held through actual job completion |
| [store.rs](../../../../crates/crab-storage/src/store.rs) | Conditional updates; ambiguous update not retried | Preserve behavior; runtime owns CAS reconciliation |
| [cell_layout.rs](../../../../crates/crab-storage/src/cell_layout.rs) | Typed application/Cell/incarnation object paths | Reuse from authority, immutable-root and backup code; never rebuild path strings in callers |
| [crab-cell-runtime](../../../../crates/crab-cell-runtime/src/lib.rs) | Stable IDs/control authority/schema, verified CAS catalog, worker-owned bootstrap, exact-root sparse activation, fixed SQL workers, ordered bounded reads, FIFO publication, retry, unknown outcomes, one absolute five-second SQL/native/sparse-page deadline, automatic fenced discard and authority-safe idle release, bounded owner renewal, idle acquisition, observed takeover, per-Cell drain, node-wide terminal drain with explicit worker join, transactional scheduler summaries, revision-pinned due scans and bounded typed Tick, progress-stall fleet filtering, typed registry/CellClient and bounded authorized SQL/KV/Queue/Workflow capabilities, exact native activity execution, owner-independent typed Queue/Workflow effects with delivery-time incarnation resolution, Queue dead-letter insertion/retention, destination inbox execution/Resolve, authenticated generic effect peer client/dispatch, signed node enrollment and streamed verified live-fleet enumeration | Add generic Workflow activity polling |
| [sql.rs](../../../../crates/crab-cell-runtime/src/sql.rs) | Typed 128-statement/1-MiB batches, read/write classification, 1,000-row/1-MiB materialization, scoped SQLite authorizers, registered codecs and a role-checked SqlCell proven through publish and exact-root restore | Use the completed handle from the repository HTTP adapter |
| [kv.rs](../../../../crates/crab-cell-runtime/src/kv.rs) | Normative schema install, bounded atomic check/write, stable versions, TTL get/list/cleanup, binary pagination, typed scope-sharded KvNamespace/registry codecs and bounded scheduler expiry cleanup | Use the completed handle from a product adapter |
| [queue.rs](../../../../crates/crab-cell-runtime/src/queue.rs) | Normative schema, producer dedup, bounded claim, token validation, lease mutations/reclaim, typed QueueNamespace, automatically registered Tick, compile-time DLQ target validation, atomic typed dead-letter effect insertion and payload retention | Add the fleet-level generic Queue polling loop |
| [workflow.rs](../../../../crates/crab-cell-runtime/src/workflow.rs) | Normative schema, pinned and registry-verified definitions, deterministic start/signal/timer transitions, cancellation, bounded activity/timer/effect persistence, typed workflow-ID-sharded dispatch, published activity claims and validation, exact compiled native activity dispatch, durable heartbeat/completion/retry and terminal cleanup | Add catalog-driven shard polling and bounded multi-activity orchestration |
| [effects.rs](../../../../crates/crab-cell-runtime/src/effects.rs) | Stable source effect IDs/digests, source-target-verified owner-independent typed command intentions for Queue and Workflow, delivery-time destination Describe, typed registered claim/validation/lease operations, one-effect native supervision, target inbox dedup/savepoint isolation, actor/LTX publication, authenticated generic peer delivery/Resolve, exact-root recovery and sender/inbox cleanup horizons; the server repository scanner runs one supervisor step for a due source | Add generic non-repository polling |
| [scheduler.rs](../../../../crates/crab-cell-runtime/src/scheduler.rs) | Derives the earliest durable work/lease/expiry/retention deadline inside bootstrap and every command transaction; publication binds it to the exact pending root; exposes revision-pinned due scans, rendezvous assignment, 15-second advertised-progress filtering and bounded idempotent Tick | Add general Workflow activity polling |
| [registry.rs](../../../../crates/crab-cell-runtime/src/registry.rs) | Startup-only static module registration, migration/schema/namespace validation, exact descriptor/function/Workflow-definition/native-activity inventory matching, canonical module/release digests, typed Command/Query trampolines, transaction-scoped compiled dispatch, async activity dispatch and exact namespace/role/code/schema support checks | Add retained old-code dispatch and remote dispatch support |
| [application.rs](../../../../crates/crab-cell-runtime/src/application.rs), [release.rs](../../../../crates/crab-cell-runtime/src/release.rs) | Canonical immutable root identity, exact-winner initialization, immutable descriptor upload, expected-revision prepared CAS, verified descriptor reads, operation-bound resumable activating/ready transitions, one-live-compatible-node activation floor and release-aware catalog provisioning with post-publication operation recheck | Add configured multi-replica eligible-node quorum, Cell migration progress and maintenance activation |
| [node.rs](../../../../crates/crab-cell-runtime/src/node.rs) | Canonical signed 15-second node advertisements, strict-create/ETag refresh and explicit shutdown withdrawal with exact ambiguous-write reconciliation, fleet/certificate/release/key/inventory/capacity binding, non-regressing scheduler progress, certificate-SPKI-bound session verification, streaming bounded live-fleet enumeration and ETag-fenced stale-record collection after the clock-skew horizon | Add qualified large-directory latency evidence |
| [codec.rs](../../../../crates/crab-cell-runtime/src/codec.rs), [peer.rs](../../../../crates/crab-cell-runtime/src/peer.rs) | Canonical bounded scalar/bytes/text/option encoding; generated peer Protobuf messages; strict request/reply unknown/duplicate/oneof rejection; exact nested-payload BLAKE3; canonical Ed25519 signing; enrollment/release/time binding; two-hop forwarding; mandatory authorization; active-owner resolution; canonical local command/query/effect dispatch; bounded mTLS management ingress; and mutation/effect-safe ambiguous transport classification | Fuzz the complete boundary |
| [client.rs](../../../../crates/crab-cell-runtime/src/client.rs) | Typed local and authenticated peer command/query/Resolve capabilities, namespace/code/schema/incarnation checks, canonical operation digest, outcome classification and minimum receipts; repository issue/comment routes and typed KV, SQL, Queue and Workflow handles use it | Add remaining product-domain adapters |
| [HTTP server.rs](../../../../crates/crab-http-server/src/server.rs), [issues.rs](../../../../crates/crab-http-server/src/issues.rs), [peer.rs](../../../../crates/crab-http-server/src/peer.rs), [peer_tls.rs](../../../../crates/crab-http-server/src/peer_tls.rs), [cells.rs](../../../../crates/crab-http-server/src/cells.rs), [cells/initializer.rs](../../../../crates/crab-http-server/src/cells/initializer.rs), [cells/repository.rs](../../../../crates/crab-http-server/src/cells/repository.rs), [cells/router.rs](../../../../crates/crab-http-server/src/cells/router.rs), [cells/scheduler.rs](../../../../crates/crab-http-server/src/cells/scheduler.rs), [cells/importer.rs](../../../../crates/crab-http-server/src/cells/importer.rs) | Static repository registry with UUID lookup; typed issue/comment HTTP bindings; bounded runtime replay plus permanent product-submission replay/conflict and source-loss restore; explicit new-repository bootstrap; import/initialization/ready catalog states; startup and changed-catalog rejection of missing roots; process-session runtime lifecycle; authoritative local peer resolution; current repository issuer/member/action reauthorization; strict Ed25519 certificate/key/CA loading; mandatory management mTLS; initial node publication and heartbeat; authoritative outbound owner reload; endpoint/enrollment equality; pinned bounded peer client reuse; one definitely-not-started stale-owner retry; exact local reuse, authenticated remote selection, idle exact-root restore and absent/expired active-owner takeover after an unchanged 15-second observation; one-second repository due scanning across rendezvous-assigned shards, 128-Cell cycle bounds, typed Tick/effect supervision, fleet/session-bound scheduler grants, temporary-activation drain, signed scan progress, 15-second fallback/readiness and scheduler health/progress/lag metrics; shard-zero-elected bounded stale-node collection; descriptor/inventory startup gate; maintenance-only, two-pass verified and resumable legacy issue/comment import | Add remaining collaboration-domain import/adapters, Workflow activity polling, migration/node eligibility quorum and complete resource-derived Cell budgets |
| [HTTP app_storage.rs](../../../../crates/crab-http-server/src/app_storage.rs) | Remaining pulls, labels, releases, statuses, checks and settings still use object application storage; issue serving no longer calls its legacy storage path | Retain legacy issue codecs only in maintenance import, then remove remaining serving callers domain by domain during the hard cut |

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
discarded leaves. Canonical Cell/incarnation rows can now be selected from a
multi-Cell bundle, independently verified and retained under one immutable bundle
digest. Exact range/full compaction verifies only selected LTX bodies plus their
indexes, preserves TXID/checksum/commit sequence/schema and returns a normal
representation-only `PreparedRoot`; suffix indexes rebuild changed directory
locators without downloading unselected bodies. Directory reads now share an
8 MiB process-wide verified-byte cache keyed by Store instance, complete typed
Cell/incarnation path and node digest; eviction cannot change correctness and
distinct backing Store instances cannot alias. Complete this package with
streaming initial directory construction, external-merge compaction, replacing the dense
capture/activation checksum array. Exact
Cell roots now load authenticated directory checksums without LTX bodies and
open a sparse writable continuation through the existing VFS.
Existing standalone `Replica` callers retain their current API; Cell runtime
code must not call its mutable epoch head.

Current local coverage is in `crates/crab-ltx/tests/cell_roots.rs` and
`crates/crab-cell-runtime/tests/publication.rs`. Add the remaining cases:

- `prepare_does_not_write_mutable_keys`: covered for native, bundled and
  compaction preparation by asserting every emitted key is below `objects/`.
- `prepared_root_restores_after_source_loss`: real RustFS upload, delete local
  source, reopen exact root and compare SQL and database checksums.
- `root_rejects_other_cell_or_incarnation`: reuse a valid digest in another scope.
- `snapshot_transfers_all_pending_cuts`: interleave transaction/checkpoint/snapshot;
  verify no local committed cut disappears from the caller-owned batch.

`exact_cell_root_opens_sparse_writer_and_publishes_incrementally` currently
proves local source deletion, exact sparse activation and successor publication.
`changed_cut_loads_only_touched_directory_nodes` proves a one-page update does
not reload a 20 MB snapshot index; `truncate_regrow_cannot_reuse_old_locator`
proves a truncated locator cannot reappear after database growth; and
`directory_nodes_are_shared_across_exact_root_views` proves a second view faults
through the verified shared directory cache without another metadata GET.

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
SQLite `query_only`, and enforce declared result bytes. Command, query and
Resolve now carry one absolute five-second deadline into the SQL worker and
sparse VFS; both the blocking page wait and asynchronous provider read stop at
that instant, and the executor recovers the typed VFS source before fencing.
Fenced task completion now closes the worker even with an unpublished local cut,
reloads the newest authority state, releases only the same owner/epoch to Idle,
and leaves a takeover untouched. A fresh idle acquisition restores the exact
published root. The fixed worker now catches native initializer/command/effect/
query unwinds, fences only that Cell and stays alive for unrelated Cells;
bootstrap releases capacity without publishing control, while accepted command
panic recovery discards tentative SQL and reopens the exact authoritative root.
All transitions use the existing Store conditional
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
- `panicking_handler_fences_only_its_cell_and_worker_continues` mutates then
  unwinds one Cell callback, proves that Cell is fenced and proves another Cell
  assigned to the same OS thread still executes.
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
- `panicking_bootstrap_keeps_worker_alive_and_releases_cell_capacity` proves the
  same invariants for an unwinding initializer and then bootstraps successfully
  through that same fixed worker.
- `query_waits_for_preceding_publication_and_cannot_write` proves a concurrent
  read observes the preceding published mutation, a write through the query
  callback is rejected, and the Cell remains readable.
- `resolve_distinguishes_committed_absent_conflict_and_expired` proves the local
  typed resolution states and digest binding;
  `resolve_waits_for_inflight_publication_and_returns_unknown_after_fence` proves
  a queued resolver cannot claim absence while its publisher is unresolved.
- `native_handler_deadline_discards_late_commit_and_reopens_authoritative_root`
  proves timeout admission fencing, delayed callback ownership, recovery-only
  worker close, Idle release, same-runtime acquisition and restoration without
  the tentative mutation.
- `native_handler_panic_discards_transaction_and_reopens_authoritative_root`
  proves an accepted panic returns outcome-unknown with the native-panic source,
  releases only the same owner to Idle and restores without the tentative write.
- `observed_takeover_fences_the_old_cell_before_more_work` drains the old runtime
  after recovery cleanup and proves it does not release or rewrite the new
  owner's epoch.
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

A private workspace crate is acceptable only as source-level decomposition: it
must compile into `crab-http-server`, expose no runtime registration or network
surface, and be covered by the same registry, route and image evidence. Reject a
delivery claim that validates a library crate without also proving its actual
server composition and product entry point.

Make `crab-http-server/src/cells.rs` the only composition root. Its static module
descriptors and bindings must produce one canonical registry or fail startup.
Add the read-only `cells release inspect --json` command and run it in the image
pipeline so release evidence is derived from the built binary, not a parallel
manifest. `crab-cell-runtime` must not depend on server/auth/Git crates, and no
application registration, module upload or handler replacement is accepted after
`RegistryBuilder::finish`.

The internal repository operation set is implemented: `cells.rs` is the single
registry composition root; its schema and stable codecs bind create/update and
get/list operations for issues and comments, including label and assignee
selection state. The integration fixture provisions a repository Cell, creates
and replays an issue, records missing-resource and wrong-author rejections,
creates and updates a comment, updates issue metadata, exercises both list
queries, drains the first owner, removes its local database and restores the
updated detail and list results from the exact published root on a new owner.

The private wire/authentication and local-dispatch slice is also implemented in
`peer.rs`. Its
build script generates messages directly from `contracts/peer.proto` with a
vendored cross-platform protoc, while the runtime rejects unknown or duplicate
fields before Prost decoding. Mutation, typed query, describe and Resolve
requests retain the exact nested bytes covered by BLAKE3 and an Ed25519
boot-session signature. Forwarding can only reduce the deadline, increments the
hop once and reuses those exact bytes. Replies receive the same strict field,
enum and size validation. `CellClient::peer` maps typed calls and unknown outcome
evidence through an injected `PeerRoundTrip`; `PeerDispatcher` mandates current
product authorization, resolves only an active local handle and reuses the local
transport. The local/peer integration test proves one operation digest, one
dedup entry and one result across both paths. The server now exposes that receive
path only on its mandatory mTLS management listener, binds the client leaf and
SPKI to the live signed node advertisement, and rechecks repository authority.
The outbound round trip now reloads authoritative control, validates the exact
live enrolled endpoint, pins the mTLS server leaf and SPKI, reuses at most 1,024
identity-specific clients, and reloads ownership once only when execution is
definitely not started. Ambiguous mutation transport is converted to the same
resolvable `OutcomeUnknown` evidence as a local lost publication response.

The server-owned `RepositoryCellRouter` now resolves authorized identities to a
local-or-peer `CellClient`, including release-fenced provision, serialized
rootless bootstrap, local handle reuse and idle exact-root acquisition. Its test
publishes through the router, drains the first owner and verifies a second
session restores and reads the same row. Next, connect authorized product
handlers to that result. Preserve product HTTP authorization in app.rs.
Switch the complete issue/comment route group only after its offline importer and
route-level recovery test exist. Do not ship a selectable second persistence
backend or route some mutations to JSON while related reads use SQLite.

Add tests:

- `local_and_peer_command_share_digest_dedup_and_query_state` is implemented:
  the same typed input and identity execute once through local and complete peer
  codec/auth/dispatch paths, and a subsequent query observes one mutation.
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
`CellClient` forwarding, live session/certificate verification and mTLS receive
dispatch are now covered. A real two-endpoint mTLS test proves server-owned owner
selection, exact endpoint/certificate/SPKI validation and authoritative reload
after a stale endpoint returns definitely-not-started. A transport unit test
proves an ambiguous command is sent once and retains its original request ID and
operation digest. Repository routing and idle acquisition are now implemented
and exercised by the public issue/comment adapter. The server acceptance test
`public_issue_request_reaches_remote_owner_over_mtls_and_publishes_ltx` starts a
real public HTTP ingress and a distinct real mTLS owner endpoint, then proves
public JSON create/read, private typed forwarding, current authorization and an
advanced authoritative LTX root in one path. A mutation capability is admitted
for its required `Describe` preflight but remains forbidden from product queries.
`crab-http-server` now supplies the
first compiled repository implementation, including migration, descriptors,
typed codecs and bindings for the complete internal issue/comment route group.
Its runtime test
`repository_commands_publish_replay_reject_and_restore_from_exact_root` proves
native create/update/list/detail behavior, exact runtime replay, later permanent
submission replay, conflict rejection and source-loss recovery. The companion
`repository_submission_reservations_repair_incomplete_visibility` test proves an
imported issue or comment reservation retains its number, display name and time
when the matching product retry makes it visible. Codec tests pin exact bytes for
all eight operation input/output pairs; binary command and inspection tests prove
release bytes remain deterministic.
`route_reuses_and_restores_explicit_repository_cell` additionally proves local
reuse and second-session idle restoration through the server-owned router. The
authenticated HTTP fixture provisions a ready release, explicitly initializes
the repository, publishes issue/comment mutations, closes the Cell, removes its
local SQLite state and restores the exact root before replay. It also proves a
malformed legacy issue object cannot affect the native serving path. Remaining
collaboration-domain imports and route cuts remain.

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
`sys_inbox`/`sys_effects` mechanics and destination actor execution are now
implemented. Destination delivery uses the fixed SQL worker, commits through the
normal LTX/control publication path, survives caller cancellation and resolves
from the published inbox after exact-root restore. Typed source
claim/validation/ack/retry operations and the one-effect native delivery
supervisor are implemented; browser callers cannot forge source effect
identities. Queue death paths now atomically insert one typed command intention,
link the source row and retain its payload through pending or failed delivery.
Workflow actions use one source-target-verified command-scoped batch of typed
command intentions; definition-declared namespaces are enforced before writes
and raw pre-encoded insertion is private. The server
repository scheduler polls due Cells and runs the supervisor through internal
fleet/session-bound authorization. Add generic non-repository polling.
The generic compiled
Cell-command DeliverEffect/ResolveEffect protocol, typed peer client, strict
identity/digest/incarnation checks and fleet/session-bound server runtime
authorization are implemented.
Current implementation includes the bounded Tick transaction and typed actor
command, including stale-root rejection, timer dispatch and terminal activity
events. It also includes revision-pinned page enumeration, 32-control due
batches and deterministic rendezvous assignment. `crab-http-server` scans its
rendezvous-assigned repository shards every second with a 128-Cell cycle cap,
submits Tick to the existing local or authenticated remote owner, or temporarily
acquires an idle/stale-owner Cell and drains it after the cycle. A zero-item Tick
runs one effect supervision step. Successful cycles advance the signed node
progress counter; equal-progress heartbeat refresh is valid, but peers exclude a
session after 15 seconds without progress. The same deadline withdraws local
readiness and feeds scheduler health/progress/lag metrics. Bounded retry queues
and generic Workflow activity orchestration remain.

| Test file | Required cases |
| --- | --- |
| tests/kv.rs | scoped routing, delete/recreate token, expired checked put/list, all-or-none multi-key mutation |
| tests/queue.rs | producer dedup after ack, expired-token ack, delayed claim emission, retry/extend and DLQ delivery response loss |
| tests/workflow.rs | signal identity conflict, stale attempt completion, cancellation, pinned definition and duplicate timer firing |
| tests/scheduler.rs | drop every wake notification, evict all due Cells, restart scanner and observe eventual published progress |
| tests/effects.rs | target commits/source response lost, source retries same effect; inbox retention exceeds sender horizon |
| tests/actor.rs | accepted target delivery survives caller cancellation, executes once, publishes its root and resolves after source-local loss and exact-root restore |
| tests/client.rs | signed generic effect delivery executes once across retries, private Resolve returns the durable inbox outcome, and the supervisor publishes source claim and acknowledgement around one destination inbox commit |

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
inspection, immutable root identity, descriptor upload, prepared/activating/ready
CAS, exact-compatible inventory scans, status and release-aware namespace
provisioning are implemented. The provision path verifies descriptor bytes and
initial namespace contract before its catalog write, then rechecks the exact
release operation after publication. Continue with migration, eligible-node
quorum and maintenance/drain procedures from deployment.md, reusing the existing
image build, storage/auth construction, health routes and metrics exporter.

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
