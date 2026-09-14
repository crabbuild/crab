# RPC, Rust and JavaScript adapter implementation

[Index](README.md). Compile [platform.proto](contracts/platform.proto) to Rust,
TypeScript and Python transport types. The descriptor defines the v1 public
service; adapters must not invent separate SQL/KV/lease semantics.

## Transport and authenticated routing

Expose gRPC service `crab.platform.v1.Platform` on the public TLS listener.
HTTP JSON equivalents are POST `/v1/mutate`, `/v1/read`, `/v1/resolve`, using
ProtoJSON field names and representations. Both invoke the same Rust handlers.
Read the body through a 1 MiB limit before decoding; reject duplicate JSON keys,
unknown fields, unknown enum values, unset oneofs and invalid byte lengths.

`Target.binding` is 1..64 ASCII characters `[a-z][a-z0-9_-]*`. Resolve it through
the authenticated application's active deployment. Namespace role determines
which operation cases are permitted. Tenant/app identity never comes from an
unchecked field in the request. `partition` is <=1024 bytes; KV/workflow/queue
producers recompute shard routing and reject mismatches.

Private forwarding uses `/internal/v1/forward` over mTLS. An envelope contains
original encoded request, tenant/app/principal IDs, authorized action set,
deployment digest, elapsed timeout, origin session and hop_count. Sign the
envelope with the originating node session key; validate mTLS/session enrollment,
signature and target binding before forwarding to local handlers. Maximum hops
is 2. Never forward through the public Service address.

After authentication, read cached owner hint. Local requests go to CellHandle;
peer requests go directly to the advertised session endpoint. On stale-owner
response reload control once, then route/acquire within the remaining timeout.
Admit local activation before CAS. Owner cache is limited to 100K entries/32 MiB
and 3 s TTL; a cache hit never replaces command publication checks.

## RPC operation mapping

| Proto operation | Required role | Handler/result |
| --- | --- | --- |
| cell_command | native/js Cell | Registered method → command_output bytes |
| sql_batch | SQL Cell | Atomic statements → SqlResults |
| kv_atomic | KV shard | Checks/mutations → ordered KvResult |
| queue_send | Queue shard | Dedup enqueue → one message identity |
| queue_claim | Queue shard | Published leases → QueueMessages |
| queue_lease | Queue shard | Ack/retry/extend → applied |
| workflow_start | Workflow shard | Create/version-pin → workflow_run_id |
| workflow_signal, workflow_cancel | Workflow shard | Event transition → applied |
| activity_claim | Workflow shard | Published activity leases → ActivityTasks |
| activity_lease | Workflow shard | Complete/fail/extend → applied |
| describe | Any | Exists plus current receipt/incarnation |
| sql_query | SQL Cell | Read-only query → ResultSet |
| kv_get, kv_list | KV shard | Logical-expiry query → KvPage |
| workflow_get | Workflow shard | Current run → WorkflowState |
| cell_query | native/js Cell | Registered read callback → command_output |

Describe with `true` may lazily provision/activate an explicit-key namespace
only if its binding grants `create`; otherwise absent returns NOT_FOUND. Fixed
primitive shards are provisioned during deployment. A null bootstrap root returns
UNAVAILABLE until its initialized schema is published. Describe supplies the
incarnation required by MutationIdentity; clients cache it until restore conflict.

`issued_at_ms >= 0`, `expires > issued`, lifetime <=24h, issued <=now+5 min,
expires>now. Revalidate after mailbox wait. timeout_ms zero means 30,000;
otherwise 1..60,000. Request expiry controls dedup validity; transport timeout
only controls how long the caller waits. Changing either identity timestamp on
retry is REQUEST_ID_CONFLICT because they enter the operation digest.

## Canonical operation digest

SDKs encode requests once and preserve them during retries. Server computes
the digest from decoded, validated values using this canonical codec, not raw
Protobuf/JSON bytes:

1. Start with ASCII `crab.op.v1\0`, Cell ID, incarnation, request_id,
   issued/expires i64 big-endian and operation field number u16.
2. Encode operation message fields in ascending Protobuf field number. Every
   known non-oneof field is included, with defaults materialized. timeout_ms,
   authenticated principal and transport headers are excluded.
3. Scalars: bool=u8 0/1; enum=u32; u32/u64/i64 fixed-width big-endian;
   f64 IEEE bits big-endian, rejecting NaN/infinity and normalizing -0 to +0.
4. Strings/bytes: u32 byte length then exact bytes. Text is UTF-8 without implicit
   Unicode normalization. Repeated fields: u32 count then encoded items in
   caller order. Nested messages: u32 encoded length then bytes.
5. Oneof: selected field number u16 followed by its value; unset is invalid.
   Optional fields: u8 presence then value if present. No maps exist in v1.
6. BLAKE3 over that byte stream is operation_digest for dedup and Resolve.

Adding operation fields changes the digest codec version; v1 rejects unknown
fields rather than silently hashing an incomplete operation. Generate fixtures
for reordered wire fields, JSON field order, bigint limits, empty bytes vs
missing oneof, and negative zero. All three SDKs must match Rust digests.

## Error and HTTP status mapping

The Proto Error carries a code plus NOT_STARTED, REJECTED or UNKNOWN. A
MutationReply may contain a receipt alongside a recorded business rejection.
Keep structured error fields in HTTP bodies and gRPC error details; primitive
outcomes are normal typed replies, while authentication/transport faults may
use gRPC status without executing a command.

| Code | HTTP | Client action |
| --- | --- | --- |
| INVALID_ARGUMENT | 400 | Correct input; do not retry unchanged |
| PERMISSION_DENIED | 403 | Refresh authorized identity or stop |
| NOT_FOUND | 404 | Stop or explicitly provision |
| PRECONDITION_FAILED, REQUEST_ID_CONFLICT | 409 | Resolve application conflict |
| REQUEST_EXPIRED | 410 | Do not replay; request explicit new operation |
| LEASE_LOST | 409 | Drop task ownership; never complete with replacement token |
| RESOURCE_EXHAUSTED | 429 | Backoff within original identity/expiry |
| OUTCOME_UNKNOWN | 503 | Resolve by identity/digest before application retry |
| UNAVAILABLE | 503 | Retry reads; mutations follow outcome classification |
| SCHEMA_INCOMPATIBLE | 409 | Deploy compatible code/schema |
| INTERNAL | 500 | Preserve operation identity; resolve if outcome UNKNOWN |

SDK retry ceiling is five transport attempts with 100/200/400/800 ms delays,
bounded by original timeout/expiry. Resolve COMMITTED/REJECTED returns stored
reply. ABSENT permits resubmitting the original mutation. UNKNOWN polls with
backoff until deadline and returns a typed UnknownOutcomeError containing the
identity/digest. A client must never manufacture a new request ID automatically.

## Native Rust ABI

Register trusted Cell definitions at build time:

```rust,ignore
pub trait CellModule: Send + Sync {
    fn command(&self, ctx: &mut CommandContext<'_>, method: &str, input: &[u8])
        -> Result<Vec<u8>, CommandError>;
    fn query(&self, ctx: &mut QueryContext<'_>, method: &str, input: &[u8])
        -> Result<Vec<u8>, CommandError>;
}
pub trait WorkflowModule: Send + Sync {
    fn transition(&self, ctx: &TransitionContext, state: &[u8], event: &[u8])
        -> Result<Decision, CommandError>;
}
```

Registry key is immutable module digest plus method name, validated against
the deployment manifest. No Rust shared-library loading or async transaction
callback is part of v1. Errors preserve typed sources internally; public errors
do not expose SQL text, credentials or input bytes.

## JavaScript host ABI

Use deno_core's JsRuntime/extension host operations. Pin its exact version and
Rust/V8 toolchain in the adapter's initial implementation commit; no second JS
engine is supported. The [upstream engine](https://github.com/denoland/deno_core)
provides embedding, while this adapter owns module loading and API restrictions.

Load only modules listed by digest in the verified deployment artifact. TS is
compiled to JS during build. Network imports, native addons and ambient Node/Deno
filesystem APIs are absent. Every command worker uses an invocation table keyed
by `(worker_generation, invocation_number)`; a host call must match both and
the current Cell. Remove the entry on return, error or trap. A saved JS reference
cannot access a later invocation's transaction.

| Context | Host operation | Sync/async and bound |
| --- | --- | --- |
| Command | sql_execute(statement, parameters) | Sync; <=128 statements, typed row/result budget |
| Command/query | sql_query(statement, parameters) | Sync; authorizer depends on context |
| Command | emit(binding, bytes) | Sync; inserts sys_effects, no network |
| Command/transition | context() | Sync; request/run ID, sequence and sampled time |
| HTTP/activity | invoke(target, operation, identity) | Async; same Rust RPC path |
| HTTP/activity | fetch(request) | Async; only manifest-approved destinations |

Command callback must return a plain bounded result, not Promise/thenable.
Reject asynchronous returns before COMMIT. Workflow transition has only context
and action constructors; it cannot call SQL. Query callback lacks writes/effects.
Catch declared `CommandRejected(code, bytes)` as a business rejection; all other
exceptions/traps are infrastructure failure and rollback.

Expose SQL integers as bigint, blobs as Uint8Array, null as null. Results crossing
HTTP ProtoJSON encode integers as decimal strings and bytes as base64, following
[ProtoJSON](https://protobuf.dev/programming-guides/json/). Application command
input/output is opaque bytes; the generated user contract chooses its codec and
validates it before JS invocation. Do not serialize arbitrary closures or objects.

```ts
// Target SDK API. Registration owns SQL transaction and acknowledgement.
export const inventory = defineCell({
  command(ctx, input) {
    const updated = ctx.sql.execute(
      "UPDATE inventory SET stock=stock-? WHERE sku=? AND stock>=?",
      [input.quantity, input.sku, input.quantity],
    );
    if (updated.rowsChanged !== 1n) throw new CommandRejected("OUT_OF_STOCK");
    ctx.emit("fulfillment", encodeOrder(input));
    return encodeReservation(input.reservationId);
  },
});
```

Guest heap ceiling is 32 MiB/worker plus bounded host buffers. Reuse compiled
module data, but clear invocation globals by disposing/recreating the context
between tenants/deployments. Persistent JS heap state is not durable Cell state.
Resource-limit traps never cause an early durability response.

## External language deployment and activities

The TS/Python SDK wraps generated clients, preserves identities, derives shards,
converts typed values and supervises activity leases. A container uses a workload
token bound to app/deployment/actions; it receives no bucket credentials.
The SDK's activity loop polls allowed shards with at most 32 concurrent tasks,
heartbeats at lease/3 and stops ownership on LEASE_LOST.

```python
# Target SDK API; all orchestration/durability remains in Rust.
async for task in client.activities("invoice", definitions=[definition]).poll():
    result = await render_invoice(task.input, idempotency_key=task.effect_id)
    await task.complete(result)
```

The supervisor runs heartbeats while the application awaits. Cancellation stops
new external work when possible, but an already issued side effect can complete.
The completion call carries run/activity/attempt/token, original request identity
and result bytes. Repeated completion preserves them. SDKs suppress payloads from
claim replies with less than the required lease margin.
