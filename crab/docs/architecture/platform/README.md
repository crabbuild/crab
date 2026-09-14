# Crab platform v1: low-level implementation specification

Status: design to implement. Revision: 2026-09-13. Existing-code baseline:
`ec20643073a`. Contract files are implementation inputs; no platform server or
SDK is implemented by this documentation change.

## Deliverable and contract precedence

Deliver a Rust runtime exposing SQL, scoped KV, partitioned Queue and explicit
state-machine Workflow through native calls and one versioned HTTP/gRPC API.
Deliver embedded TypeScript/JavaScript services and ordinary OCI services and
activity workers in other languages. One command changes one SQLite Cell and
is acknowledged only after LTX dependencies and its control CAS are durable.

The SQL migrations and Protobuf descriptor are normative. Prose supplies
validations, ordering and preconditions not expressible in those formats.
Rust signatures specify interfaces to implement, not existing library symbols.

| Specification | Implementation input |
| --- | --- |
| [Runtime](runtime.md) | Rust ownership types, command loop, CAS predicates, executor lifecycle and failure actions |
| [Storage](storage.md) | Identity encoding, object keys, control/root formats, LTX API changes and activation |
| [Primitives](primitives.md) | SQL statements, leases, dedup, state transitions and scheduler procedures |
| [Language adapters](languages.md) | RPC mapping, typed values, host capability ABI and SDK retries |
| [Deployment](deployment.md) | Configuration, admission equations, deployment records, migrations and operator procedures |
| [Delivery](delivery.md) | Source changes, dependency order, named tests and executable contract validation |
| [Runtime migration](contracts/runtime.sql) | Install in every Cell |
| [KV migration](contracts/kv.sql) | Install in KV shard Cells |
| [Queue migration](contracts/queue.sql) | Install in Queue shard Cells |
| [Workflow migration](contracts/workflow.sql) | Install in Workflow shard Cells |
| [Wire descriptor](contracts/platform.proto) | Compilable Protobuf v3 data API |

## Fixed v1 boundary

| Item | Implementation decision |
| --- | --- |
| Durability | One object-store origin; immutable LTX preparation then owner/root CAS |
| Rust modules | Trusted code statically registered in the operator runtime image |
| JavaScript | deno_core host with synchronous command imports and async HTTP imports |
| Other languages | Protobuf/HTTP clients in OCI containers; TS and Python SDKs delivered |
| Workflow | Deterministic transition callback plus persisted activities/timers/signals |
| KV | Values up to 64 KiB; scoped atomic mutations and scope-local listing |
| Queue | Payloads up to 256 KiB; at-least-once, no FIFO guarantee |
| Blobs | Internal immutable artifacts/references; no public object API in v1 |
| Upgrades | Transactional per-Cell schema switch; incompatible deployments use maintenance |
| GC | Offline application-scoped collection; writers stopped and write access revoked |

WASM embedding, async workflow replay, large KV spill, global KV listing,
public object/multipart APIs, peer-disk acknowledgements, online GC and
cross-Cell transactions have no v1 endpoint or stub implementation. Other
languages deploy through containers and use every v1 primitive over RPC.

## Source ownership and target files

Create these modules with their first working caller. This is the implementation
allocation, not a list of alternative crate structures.

```text
crates/crab-ltx/src/replica/prepared.rs       immutable root preparation
crates/crab-ltx/src/replica/root.rs           bounded root/page codec
crates/crab-ltx/src/paged/directory.rs        authenticated page directory
crates/crab-ltx/src/managed.rs                transaction/read hooks

crates/crab-cell-runtime/src/
  identity.rs       CellId, request hashing, partition mapping
  authority.rs      Control codec and conditional transitions
  actor.rs          per-Cell supervised command state machine
  executor.rs       bounded synchronous SQL worker shards
  publication.rs   pending cut ownership and reconciliation
  catalog.rs       fixed catalog shards and provision-before-use
  scheduler.rs     due-summary scanner and maintenance commands

crates/crab-platform/src/
  sql.rs, kv.rs, queue.rs, workflow.rs, effects.rs
  migrations/      SQL copied from contracts/
  guest.rs         transaction-scoped native handler interfaces

crates/crab-platform-protocol/               platform.proto + generated types
crates/crab-platform-server/src/
  rpc.rs, auth.rs, peers.rs, deployment.rs, main.rs
  javascript.rs    deno_core adapter and module registry

packages/platform-sdk/                      TypeScript client
packages/platform-python/                   Python client/activity supervisor
```

Keep `crab-workflow` Git/DVC APIs and `crab-sdk` Git APIs unchanged.
`crab-http-server` consumes the native Cell runtime for repository application
data; Git/Xet/LFS retain their current publication owners. Apply the accepted
hard cutover using [deployment](deployment.md#repository-application-cutover).

## Fixed initial protocol limits

These are wire admission limits and initial implementation defaults, not
benchmark claims. A larger wire limit requires updated capability/contract tests.

| Limit | v1 value |
| --- | --- |
| Request | 1 MiB encoded; up to 128 SQL statements or KV mutations |
| Command/result/state | 1 MiB each; SQL read at most 1,000 rows and 1 MiB |
| Request ID/incarnation | 16 bytes; digests and Cell IDs 32 bytes |
| Request validity | expires - issued <= 24h; issued at most 5 min in future |
| Request record retention | Through request expiry + 24h |
| Effect lifetime / inbox retention | 7 days / effect expiry + 7 days |
| Transport wait | Default 30 s, maximum 60 s |
| Guest transaction CPU / wall time | 50 ms / 5 s; host page waits count toward wall time |
| Queue/activity lease | Default 30 s, allowed 5–300 s |
| Delivery margin | At least 1 s remaining before emitting a claimed task |
| Queue attempts / retention | 20 deliveries / 30 days from enqueue |
| Activity attempts / lifetime | 20 / 7 days from scheduling |
| Renewal / self-fence / takeover observation | 3 s / 10 s / 15 s |
| Scheduler scan pass | <= 5 s for the admitted catalog |

Capacity targets are 1K–10K simultaneously open DBs and 1,000 user commands/s
aggregate per node. Hardware and qualification are in [deployment](deployment.md#resource-profiles-and-capacity-targets)
and [delivery](delivery.md#capacity-qualification). Meeting these targets requires
bounded storage; raising today's `Limits` alone is insufficient.
