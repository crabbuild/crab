# Implementation work packages and acceptance tests

[Index](README.md). Packages below are ordered dependencies with concrete
changes and tests. Contract files define the target; passing their validator
does not establish a working runtime.

## Existing implementation to reuse

| Source | Current behavior | Required change |
| --- | --- | --- |
| [managed.rs](../../../../crates/crab-ltx/src/managed.rs) | Trusted synchronous SQL callback; separate capture; local-only commit | Typed operation errors, restricted application/read callbacks and actor-owned cut transfer |
| [replica.rs](../../../../crates/crab-ltx/src/replica.rs) | Immutable manifest plus per-epoch mutable head | Immutable-only prepare/open_root; Cell runtime never uses epoch head as authority |
| [append.rs](../../../../crates/crab-ltx/src/replica/append.rs) | Shared native/bundle append verification | Reuse verification under the prepared-root API |
| [paged.rs](../../../../crates/crab-ltx/src/paged.rs) | Authenticated but resident page map; sparse writable SQL | Bounded directory nodes/cache and capture checksum tracker |
| [environment.rs](../../../../crates/crab-ltx/src/environment.rs) | Filesystem/executor hooks and count admission | Byte reservations held through actual job completion |
| [store.rs](../../../../crates/crab-storage/src/store.rs) | Conditional updates; ambiguous update not retried | Preserve behavior; runtime owns CAS reconciliation |
| [HTTP app_storage.rs](../../../../crates/crab-http-server/src/app_storage.rs) | Existing object application storage | Native repository Cell integration after runtime acceptance |

Reuse existing [publication tests](../../../../crates/crab-ltx/tests/publication.rs),
[host tests](../../../../crates/crab-ltx/tests/host_hooks.rs) and
[RustFS fixture](../../../../crates/crab-ltx/tests/remote.rs). They are library
mechanics evidence; none already proves multi-owner HTTP output gating.

## Work package 1: immutable LTX preparation

Modify replica.rs/append.rs/bundles.rs to route through prepared.rs/root.rs.
Implement RootRef scope validation, root/descriptor codecs and prepare/prepare_
compaction methods from storage.md. Remove any Cell-runtime call to mutable epoch
heads. Check existing standalone callers before changing their public API.

Add `crates/crab-ltx/tests/prepared.rs` cases:

- `prepare_does_not_write_mutable_keys`: instrument transport; assert no head/
  control update during native, bundled and compaction preparation.
- `prepared_root_restores_after_source_loss`: real RustFS upload, delete local
  source, reopen exact root and compare SQL and database checksums.
- `root_rejects_other_cell_or_incarnation`: reuse a valid digest in another scope.
- `snapshot_transfers_all_pending_cuts`: interleave transaction/checkpoint/snapshot;
  verify no local committed cut disappears from the caller-owned batch.

Exit: default/replica builds and existing tests pass, plus live source-loss test.

## Work package 2: runtime command and authority

Implement identity.rs, authority.rs, actor.rs, executor.rs and publication.rs.
Install runtime.sql and enforce the runtime.md state table. Add a command fixture
that increments a counter and stores request outcome. All transitions use the
existing Store conditional primitives, preserving their source errors.

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

Exit: two local runtime processes against isolated RustFS pass counter increment,
owner kill, full source-directory removal and query recovery.

## Work package 3: bounded storage

Implement directory.rs, streaming root construction, external-merge compaction,
and directory-backed capture checksum updates. Remove whole-DB buffers from
active paths; do not preserve a second unbounded implementation as fallback.

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
- `peer_auth_rechecks_repository_membership`, `forward_hop_limit_preserves_auth`,
  `public_router_has_no_primitive_or_peer_endpoint` and
  `unregistered_owner_endpoint_receives_no_credentials`.
- Compile-fail cases for moving CommandContext across threads, returning a
  borrowed statement and using an async command callback.
- `application_sql_cannot_attach_or_modify_system_table` checks direct and
  trigger/view-mediated access. This proves SQL API constraints, not a Rust sandbox.
- `native_watchdog_fences_without_recycling_worker_permits`: block native code,
  expire its budget, verify no further admission or premature permit release;
  unblock it for test cleanup and prove its tentative writes never publish.

Fuzz peer decoding, signed envelope validation and path/identity encoding.
Pin independent command/input/output fixtures for every registered codec version.

Exit: the browser creates a comment through node A while node B owns its Cell;
kill B and remove its local state, then fetch the same comment through A from
RustFS. Repeat a lost response using the original submission ID and prove one row.

## Work package 5: primitives and durable scheduler

Implement migrations and SQL algorithms in primitives.md exactly once in Rust.
Add sys_inbox/sys_effects handler and private authenticated DeliverEffect
protocol; browser callers cannot forge source effect identities. Implement
catalog scanner and Tick through the normal actor command loop.

| Test file | Required cases |
| --- | --- |
| tests/kv.rs | scoped routing, delete/recreate token, expired checked put/list, all-or-none multi-key mutation |
| tests/queue.rs | producer dedup after ack, expired-token ack, delayed claim emission, retry/extend and DLQ delivery response loss |
| tests/workflow.rs | signal identity conflict, stale attempt completion, cancellation, pinned definition and duplicate timer firing |
| tests/scheduler.rs | drop every wake notification, evict all due Cells, restart scanner and observe eventual published progress |
| tests/effects.rs | target commits/source response lost, source retries same effect; inbox retention exceeds sender horizon |

Exit: a compiled Rust workflow schedules a native asynchronous activity; kill
the executing process after its external idempotent effect. A replacement retries
safely and records one completion event. Kill every runtime and recover pending
queue/timer/outbox state from RustFS. Test transitions with deterministic fixtures
and explicit old/new definition dispatch, not language-engine behavior.

## Work package 6: releases, cutover and operations

Implement embedded release descriptors and administrative subcommands in the
existing crab-http-server binary. Reuse its image build, storage/auth construction,
health routes and metrics exporter. Implement compile-time registry validation,
namespace provisioning, migration and drain procedures from deployment.md.

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
