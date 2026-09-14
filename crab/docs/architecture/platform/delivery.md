# Delivery plan and qualification

[Design index](README.md). This is an implementation plan, not a completion
report. No new platform capability is qualified by writing these documents.

## Current source evidence

The design was checked against the following source boundaries at
`996cde6911e9ab519c4200df0c977366377f16bd`:

| Surface | Current behavior | Design consequence |
| --- | --- | --- |
| [ManagedDb](../../../../crates/crab-ltx/src/managed.rs) | Synchronous trusted transaction callback; capture/checkpoint/snapshot return local cuts | Runtime must own transaction result, all cuts and output gate; guest SQL needs restrictions |
| [Replica](../../../../crates/crab-ltx/src/replica.rs) | Immutable manifests plus per-epoch mutable head; exact open/restore | Add an immutable preparation boundary for combined owner/root publication |
| [Append verification](../../../../crates/crab-ltx/src/replica/append.rs) | Private verification path shared by append operations | Refactor existing verified mechanics; do not create a weaker platform uploader |
| [Paged database](../../../../crates/crab-ltx/src/paged.rs) | Authenticated page map, sparse writable activation and checked range reads | Reuse correctness path; replace unbounded metadata residency before scale claims |
| [Host hooks](../../../../crates/crab-ltx/src/environment.rs) | Injectable filesystem/executor and shared count-based admission | Add byte budgets and runtime scheduler ownership above this seam |
| [Store update](../../../../crates/crab-storage/src/store.rs) | Conditional update; ambiguous errors intentionally not retried | Caller must reconcile publication and retain stable operation identity |
| [HTTP manifest](../../../../crates/crab-http-server/Cargo.toml) and [app storage](../../../../crates/crab-http-server/src/app_storage.rs) | No runtime LTX dependency; existing application storage | HTTP Cell integration remains a delivery slice |
| [Existing workflow](../../../../crates/crab-workflow/src/lib.rs) | Git/DVC planning, stage execution, artifacts and experiments | Keep platform durable workflows distinct from this existing product API |
| [Library tests](../../../../crates/crab-ltx/tests/publication.rs) and [RustFS fixture](../../../../crates/crab-ltx/tests/remote.rs) | Replication/publication mechanics have test coverage | Extend with owner/runtime/protocol tests; library tests are not platform E2E proof |

The [LTX README](../../../../crates/crab-ltx/README.md),
[parity matrix](../../../../crates/crab-ltx/PARITY.md) and
[scalability audit](../../../../crates/crab-ltx/SCALABILITY.md) record existing
qualification and gaps. This documentation change does not rerun or upgrade
that evidence. An internal `prepare_append` helper does not already provide a
public immutable-root API: it still participates in the per-epoch-head path.

## Delivery slices

Each slice is a vertical behavior with a visible result. Introduce packages only
when its implementation needs the ownership boundary. Do not ship stub bindings
that silently use memory or bypass publication.

| Slice | Work and dependency | Acceptance gate |
| --- | --- | --- |
| 1. Root preparation | Refactor LTX verification/upload, immutable root identity, first-create/inherit/compaction paths | Prepare without mutable writes; recover exactly; reject corrupt bodies/indexes on every path |
| 2. Single-node durable SQL | Cell command executor, dedup/result records, commit capture, control CAS, read barrier | Command → RustFS → delete local source → restart → same result/query |
| 3. Ownership and routing | Catalog, capacity admission, sessions, monotonic suspicion, peer auth, handoff | Competing owners, pause/partition and lost CAS response cannot lose acknowledged writes |
| 4. Remote protocol | Versioned SQL/Cell RPC/HTTP, auth, typed values, Rust and TS clients | Rust/TS invoke same command and resolve same unknown operation; no integer loss |
| 5. Embedded JS and deploy | Guest command sandbox, module loader, schemas/migrations, immutable deploy flow | Deploy TS Cell + HTTP handler; kill owner; query restored state through another node |
| 6. Durable effects | Outbox/inbox, scheduler summaries, catalog rescanning | Lose every notification and restart all workers; effect still delivers with correct dedup |
| 7. KV and Queue | Partitioning, TTL/versions, published leases, retries/DLQ | Cross-language conditional writes and queue delivery survive source loss and delayed ack |
| 8. Workflow + activities | Explicit state machine, timers/signals, run/attempt fencing, Python worker SDK | TS workflow calls Python activity, survives node/worker loss and duplicate completion |
| 9. Fleet operations | OCI services, deployment status, migration/drain, restore, retention | Customer Kubernetes rollout and recovery with pinned running workflow definitions |
| 10. Scale infrastructure | Bounded metadata, streaming operations, byte budgets, workload harness | Profile-specific 1K/10K active DB and aggregate TPS measurements within explicit limits |
| 11. WASM host | Qualified WIT ABI, guest runtime and toolchain matrix | One supported language runs transactional commands with the same conformance/fault tests |
| 12. Additional products | Async workflow replay, online GC, object API, optional peer durability | Separate protocol decisions and acceptance proofs before support claims |

Resource bounds and cancellation supervision start in slice 2; slice 10 proves
the larger envelope and completes storage changes needed for it. Embedded JS
depends on native SQL/ownership/protocol boundaries, while remote container
services can become useful at slice 4. WASM is not a prerequisite for Python,
Go or Java access over the protocol.

`crab-http-server` can adopt the native Cell API after slice 3 with its own
application migrations and browser acceptance work. It need not wait for
WASM or the general Workflow primitive. Share the Cell engine and publication
rules instead of implementing a second repository-only owner protocol.

## Contract and fault tests

| Invariant | Required adversarial scenario |
| --- | --- |
| No lost acknowledged writes | Kill after every SQL/capture/upload/CAS boundary; remove all owner-local files; restore on another node |
| No stale publication | Pause owner, acquire successor, resume old process and race user/maintenance/heartbeat updates |
| Unknown outcome reconciliation | Drop successful CAS response and every subsequent response in turn; retry same ID and compare durable result |
| No tentative-state exposure | Block publication and attempt query, error response, queue claim, workflow activity and guest network effect |
| Cut completeness | Snapshot/checkpoint/hydration/compaction overlap writes; every committed cut is either published or reported unresolved |
| Consistent language semantics | Null/blob/large integer/duplicate columns, request hashing, errors and deadlines agree in Rust, TS and Python |
| SQL isolation/security | Attempt system-table edits, pager PRAGMA, ATTACH, transaction escape, guest trap and oversized result |
| KV version safety | Delete/recreate cannot satisfy an old CAS token; TTL agrees across read/check/list; scoped keys colocate |
| Queue delivery safety | Claim publication delayed past lease, worker crash, expired token ack, duplicate receive and DLQ response loss |
| Workflow progress | Duplicate events, stale activity attempts, missing notifications, sleeping cold shards, cancellation and version-pinned runs |
| Bounded resource use | Saturate guest/SQL pools with sparse faults; inject disk full, cancellation and oversized incompressible DBs |
| Safe retention | Backup/read/upload pins race collection; attempted publication while maintenance barrier is active is rejected |
| Deployment recovery | Incompatible schema, partial container rollout, failed migration publication and old workflow worker removal |

Provider tests must prove conditional create/update, strong origin reads,
immutable integrity and range correctness using the actual transport and
production filesystem. RustFS/S3 success does not automatically qualify GCS,
Azure or another S3-compatible endpoint. Include provider request failures and
real persistent-volume restart/power-loss evidence where feasible.

SDK packages need serialization fixtures and protocol conformance, not a
separate reimplementation of durability tests for every convenience method.
Fuzz untrusted codecs, SQL parameters, cursors, manifests and guest boundaries.
Model the owner/publication state machine and check acknowledged-history
linearizability under controlled races.

## Capacity qualification

Use the [specified node profiles](deployment.md#resource-profiles-and-capacity-targets)
in isolated infrastructure. Do not run 5 GB × 10K workloads on a developer's
checkout or count logical registrations as open databases.

For each profile, record memory/CPU/SSD limits, filesystem, provider, RTT,
bandwidth, payload and changed-page distribution, compression, read/write mix,
skew, open DB count and maintenance load. Include 100 MB and 5,000 MB databases,
incompressible data, empty/idle and continuously active connections. State whether
the 1,000 TPS workload counts user commands only; separately report primitive
internal claims, acks and timer transactions.

Measure achieved TPS, p50/p95/p99 latency, recovery time, takeover lag, queue age,
timer lateness, peak RSS/SSD/FDs/threads, object requests per operation and write
amplification. Sweep offered load to saturation; preserve visible overload and
no-loss behavior rather than buffering requests without limit.

Run writes with background hydration/compaction, hot-shard traffic, rollout,
simultaneous takeover and provider errors. Capacity passes only for the measured
profile/distribution and agreed latency budget. Reports must include unsuccessful
target cells and the limiting resource. The small profile can remain supported
at a lower admitted workload without advertising unproven 10K-active capacity.

## Decisions still requiring measurements or implementation proof

| Decision | Baseline | Evidence needed before broadening |
| --- | --- | --- |
| Acknowledgement latency | Object-store publication | User latency SLO and provider benchmarks; peer durability needs a new protocol |
| Runtime engines | Native Rust + qualified JS; WASM later | Engine versions, cross-platform builds, isolation/resource tests |
| Storage format | Preserve verified digest/checksum semantics | Versioned block-addressable format fixtures and hard-cutover procedure |
| Dedup/queue/workflow horizons | Explicit configured retention | Maximum client retry, redrive and external activity duration |
| Clock policy | Monotonic ownership suspicion; qualified wall time for schedules | Skew/jump tests and stated timer/lease timing guarantees |
| Online GC | Scoped maintenance collection first | Reference/pin publication fencing and concurrent deletion proof |
| Global transactions | One Cell only | Separate coordination design if a real consumer requires more |
| Cloudflare compatibility | Own versioned platform API | Explicit API-by-API compatibility suite; engine reuse is insufficient |

The design fixes ownership, semantics and delivery order now. Exact performance
defaults and language/toolchain support remain qualification results, not guessed
configuration constants.

## Design validation

Documentation checks cover relative links, fenced-block structure, schema syntax
where a block is standalone SQL, and diagram syntax where renderer tooling is
available. Rust/TypeScript/WIT examples are explicitly proposed API shapes;
implementation slices must turn them into compiling, versioned conformance
examples before SDK release. No runtime feature or dependency is added by this
document set.
