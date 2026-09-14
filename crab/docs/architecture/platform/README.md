# Embedded Rust Cell runtime: low-level implementation specification

Status: design to implement. Revision: 2026-09-14. Existing-code baseline:
`ec20643073a`. SQL and peer contracts are implementation inputs. The initial
identity/control/schema foundation and native-cut immutable root preparation now
exist in `crab-cell-runtime` and `crab-ltx`; exact roots support lazy,
authenticated page reads, and the local executor now persists request outcomes
while retaining pending cuts/results until an exact prepared root is confirmed.
The publication coordinator also resolves a lost CAS response when origin names
that exact root and refreshes through pure lease renewals. The node-wide runtime
dispatcher now combines the fixed SQL workers with per-Cell request/byte
mailboxes, node byte admission, FIFO single-flight publication, cancellation-safe
accepted work, bounded retry backoff, structured unknown outcomes and drain.
Catalog activation, deadlines/watchdogs, later-root request resolution, fenced
recovery, reads, directory-backed writable SQLite, streaming directory updates,
prepared compaction/bundles, primitives, HTTP cutover and capacity qualification
remain incomplete.

## Deliverable and contract precedence

Embed SQL, scoped KV, partitioned Queue and explicit state-machine Workflow
in the existing `crab-http-server` process. Crab application handlers and
activities are trusted Rust code compiled into that binary. One command changes
one SQLite Cell and is acknowledged only after its immutable LTX dependencies
and owner/root CAS are durable. A repository's collaboration data uses one Cell;
shared queue/KV/workflow namespaces use separate fixed shards where needed.

The SQL migrations and private Protobuf descriptor are normative. Prose supplies
validation, ordering and preconditions not expressible in those formats. Rust
signatures below are interfaces to implement, not existing library symbols.
This specification refines the shared runtime contracts in the earlier
[HTTP next architecture](../../../../crates/crab-http-server/next-architecture/README.md);
that design retains repository-specific data, Git and cutover requirements.
The directory name `platform/` is retained for documentation links, not a
standalone product or server.

| Specification | Implementation input |
| --- | --- |
| [Runtime](runtime.md) | Ownership types, command loop, CAS predicates, executor lifecycle and failure actions |
| [Storage](storage.md) | Identity encoding, object keys, control/root formats, LTX API changes and activation |
| [Primitives](primitives.md) | SQL statements, leases, dedup, state transitions and scheduler procedures |
| [Rust API and peer protocol](rust-api.md) | Typed handlers, transaction lifetimes, internal forwarding and Crab integration |
| [Deployment](deployment.md) | Existing server configuration extensions, admission, compiled releases, migrations and operations |
| [Delivery](delivery.md) | Source changes, dependency order, named tests and executable contract validation |
| [Runtime migration](contracts/runtime.sql) | Install in every Cell |
| [KV migration](contracts/kv.sql) | Install in KV shard Cells |
| [Queue migration](contracts/queue.sql) | Install in Queue shard Cells |
| [Workflow migration](contracts/workflow.sql) | Install in Workflow shard Cells |
| [Private peer descriptor](contracts/peer.proto) | Protobuf messages for enrolled Crab nodes, not a public primitive service |

## Fixed v1 boundary

| Item | Implementation decision |
| --- | --- |
| Executable | Existing `crab-http-server`, one binary/container per node |
| Application model | Build-time Rust registry; typed synchronous commands/queries and asynchronous activities |
| Calls | In-process Rust calls for local owners; versioned private messages for remote owners |
| Durability | One object-store origin; immutable LTX preparation then owner/root CAS |
| Workflow | Explicit transition callback plus persisted activities/timers/signals; no stack replay |
| KV | Values up to 64 KiB; scoped atomic mutations and scope-local listing |
| Queue | Payloads up to 256 KiB; at-least-once, no FIFO guarantee |
| Git and blobs | Existing Git/Xet/LFS/release-asset owners; no second public object API |
| Upgrades | Compiled schema/definition versions; incompatible changes use maintenance |
| GC | Offline application-scoped collection; writers stopped and write access revoked |

No JS/V8/WASM execution, multi-language backend SDKs, generic public SQL/KV/RPC
listener, dynamic code loading, or service-bundle deployment system. The existing
React browser application remains a client of Crab's product HTTP API.
Cross-Cell transactions, online GC and peer-disk durability acknowledgements
are also outside v1. Native code is trusted, not a tenant sandbox.

## Source ownership and target files

Create one new crate, `crab-cell-runtime`, with its first working Crab caller.
Primitive modules share its transaction and publication owner; separate facade,
protocol, SDK and platform-server crates are unnecessary.

```text
crates/crab-http-server/src/
  server.rs, app.rs            lifecycle, auth/admission and existing HTTP routing
  cells.rs                    compiled repository registry and runtime composition
  peer.rs                     private authenticated forwarding on management listener
  cells/commands.rs           repository command/query types and handlers
  cells/activities.rs         native Git/outbox activity adapters
  cells/migrations/           repository SQL migrations

crates/crab-cell-runtime/src/
  identity.rs, authority.rs    Cell identity and owner/control CAS
  actor.rs, executor.rs        supervised commands and bounded SQL workers
  publication.rs              pending cut ownership and reconciliation
  catalog.rs, scheduler.rs     provision-before-use and due-summary scanning
  registry.rs, api.rs          typed definitions, codecs and capability handles
  peer.rs                     private message codec, no listener/auth policy
  sql.rs, kv.rs, queue.rs,
  workflow.rs, effects.rs      primitive mechanics
  migrations/                 SQL copied from contracts/

crates/crab-ltx/src/
  replica/prepared.rs          immutable root preparation
  replica/root.rs              bounded root/page codec
  paged/directory.rs           authenticated page directory
  managed.rs                  transaction/read hooks

crates/crab-storage/src/       scoped layout and existing provider-neutral storage
```

Keep `crab-workflow` Git/DVC APIs and `crab-sdk` Git APIs unchanged.
The runtime owns no repository policy, HTTP listener or provider credentials.
Git/Xet/LFS keep their current publication owners; SQL coordinates with them
through durable intentions, not a cross-system atomic commit. Apply the accepted
[hard cutover](deployment.md#repository-application-cutover).

## Fixed initial operation limits

These are admission limits and implementation defaults, not benchmark claims.

| Limit | v1 value |
| --- | --- |
| Operation/result/state | 1 MiB each; SQL read at most 1,000 rows and 1 MiB |
| Peer envelope | Operation limit plus 16 KiB authenticated metadata |
| Batch | Up to 128 SQL statements or KV mutations |
| Request ID/incarnation | 16 bytes; digests and Cell IDs 32 bytes |
| Request validity | expires - issued <= 24h; issued at most 5 min in future |
| Request record retention | Through request expiry + 24h |
| Effect lifetime / inbox retention | 7 days / effect expiry + 7 days |
| Transport wait | Default 30 s, maximum 60 s |
| Native transaction wall budget | 5 s cooperative deadline; cannot forcibly preempt arbitrary Rust |
| Queue/activity lease | Default 30 s, allowed 5–300 s |
| Delivery margin | At least 1 s remaining before emitting a claimed task |
| Queue attempts / retention | 20 deliveries / 30 days from enqueue |
| Activity attempts / lifetime | 20 / 7 days from scheduling |
| Renewal / self-fence / takeover observation | 3 s / 10 s / 15 s |
| Scheduler scan pass | <= 5 s for the admitted catalog |

Capacity targets remain 1K–10K simultaneously open DBs and 1,000 user commands/s
aggregate per node. Hardware and qualification are in [deployment](deployment.md#resource-profiles-and-capacity-targets)
and [delivery](delivery.md#capacity-qualification). No profile is promised to
meet these targets without measurement.
