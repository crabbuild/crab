# Languages, SDKs and the application programming model

[Design index](README.md). Names, package coordinates, commands and code examples
are proposed API sketches, not installable packages or tested samples.

## Supported forms of application execution

| Model | Where application code runs | Transaction branching | Deployment artifact |
| --- | --- | --- | --- |
| Trusted native Rust | Runtime process built by the operator | Yes, synchronous transaction callback | Operator runtime OCI image/binary |
| Embedded JS/TypeScript | Guest execution worker on the Cell owner | Yes, restricted local host calls | Compiled JS module plus manifest |
| Qualified WASM component | Component instance on the Cell owner | Yes, capability-scoped host calls | Component, WIT package/version and manifest |
| External service in any language | Ordinary application process/container | Invoke commands or atomic batches | Service OCI image plus bindings |
| External activity worker | Ordinary worker process/container | Activities complete through RPC | Worker OCI image and activity subscriptions |

Language support has an explicit compatibility matrix. HTTP/gRPC makes every
primitive accessible from any language with a suitable client. It does not
automatically make that language an embedded Cell runtime. Python or Java
services deploy as ordinary containers first. Node.js libraries requiring native
addons or broad OS access use this container model too.

The JS adapter should evaluate `deno_core` for embedding V8 and Rust host
operations. It is an engine substrate, not a ready-made Node/Deno compatibility
promise; module loading, permissions and supported APIs belong to our adapter.
See its [upstream repository](https://github.com/denoland/deno_core). Pin the
qualified version during implementation rather than copying a moving version
from documentation. TypeScript is bundled/transpiled during build.

For the later WASM adapter, Wasmtime supplies component hosting, WIT-generated
Rust bindings and host resources, as described in its
[component API](https://docs.wasmtime.dev/api/wasmtime/component/index.html).
Qualify compiler, component ABI, WASI imports and library support per language.
Do not advertise all languages as compiling to the same supported component
without those tests. JS and external services do not depend on this phase.

## Two application contexts

`ServiceContext` handles HTTP and activity execution. It exposes authorized
remote primitives, request deadlines and allowed network calls. Each call can
cross a Cell boundary; it is not implicitly one transaction.

`CommandContext` exists only during an owner-local command. It exposes a single
Cell transaction, a stable request ID, sampled time, deterministic random input
where needed, and an outbox. It has no external network or remote Cell binding.
Results remain private until Rust commits and publishes the operation.

Do not infer transaction semantics from the host language's `async` keyword.
The baseline native/JS/WASM command invocation is bounded and synchronous at the
guest API boundary. Remote calls and activities are asynchronous outside it.
SQLite page faults may wait for Rust I/O while the invocation occupies a worker
slot; the independent page driver avoids executor deadlock.

## Rust Cell module

Illustrative service definition with one command and one published-state query:

```rust,ignore
struct Counter;

impl CellDefinition for Counter {
    type Command = Increment;
    type Query = ReadValue;
    type Reply = CounterValue;

    fn command(
        &self,
        ctx: &mut CommandContext<'_>,
        command: Increment,
    ) -> ServiceResult<CounterValue> {
        ctx.sql().execute(
            "UPDATE counter SET value = value + ?1 WHERE id = 1",
            &[SqlValue::Integer(command.amount)],
        )?;
        let value = ctx.sql().integer(
            "SELECT value FROM counter WHERE id = 1", &[],
        )?;
        Ok(CounterValue { value })
    }

    fn query(
        &self,
        ctx: &mut QueryContext<'_>,
        _: ReadValue,
    ) -> ServiceResult<CounterValue> {
        Ok(CounterValue {
            value: ctx.sql().integer(
                "SELECT value FROM counter WHERE id = 1", &[],
            )?,
        })
    }
}
```

Migrations create the initial row. Framework registration binds the definition,
wire schemas and migrations to a namespace. Rust generics stop at the native
adapter: remote users invoke the schema-defined command, not a serialized Rust
closure. Trusted native code links into a deployment image; loading arbitrary
Rust shared libraries does not provide a stable or isolated plugin ABI.

## TypeScript Cell and HTTP service

The same counter in the proposed embedded JS SDK:

```ts
import { defineCell, defineService } from "@crab-platform/sdk";

export const Counter = defineCell({
  name: "counter",
  schema: "./contracts/counter.json",
  migrations: "./migrations/counter",

  command(ctx, input: { amount: bigint }) {
    ctx.sql.execute(
      "UPDATE counter SET value = value + ? WHERE id = 1",
      [input.amount],
    );
    const row = ctx.sql.first<{ value: bigint }>(
      "SELECT value FROM counter WHERE id = 1",
    );
    if (!row) throw new Error("counter migration invariant violated");
    return { value: row.value };
  },
});

export default defineService({
  async fetch(request, env) {
    const principal = await env.identity.requireUser(request);
    const requestId = request.headers.get("Idempotency-Key");
    if (!requestId) return new Response("Idempotency-Key required", { status: 400 });

    const counter = env.cells.counter(principal.accountId);
    const receipt = await counter.command({ amount: 1n }, { requestId });
    return Response.json({ value: receipt.value.value.toString() });
  },
});
```

Command input is validated at the Rust boundary using the deployed contract.
The gateway maps the signed-in identity to an authorized account partition.
It preserves a client-supplied idempotency key; generating a new key for each
HTTP retry would defeat deduplication. Integer results use `bigint` in JS and
decimal strings at the JSON boundary, avoiding silent precision loss.

A Cell invocation can insert an outbox effect in the same transaction through
`ctx.outbox.enqueue(...)`. Calling `env.queues.send(...)` from a stateless HTTP
handler is a separate durable operation. The names and documentation must make
this distinction visible to application builders.

## External Python/Node/Go services

An ordinary Python worker uses the same Rust workflow/queue engines:

```python
# Proposed SDK sketch; package not published.
from crab_platform import Client

client = Client(endpoint=endpoint, workload_identity=identity)

await client.kv("settings").put(
    key=b"theme",
    value=b"dark",
    request_id=request_id,
)

async for task in client.activities("invoice-renderer").poll():
    result = await render_invoice(
        task.input,
        idempotency_key=task.effect_id,
    )
    await task.complete(result)
```

The production worker SDK supervises lease heartbeats, consumer credits,
cancellation and completion retries. The abbreviated loop illustrates ownership
of the business activity, not a complete lease implementation. A lost lease
prevents completion from changing workflow state; an already issued external
effect may still complete and needs destination deduplication.

An external Node service invokes a deployed command rather than fetching rows
and holding a network transaction:

```ts
const receipt = await client.cells("inventory", warehouseId).command(
  "reserve",
  { sku, quantity, reservationId },
  { requestId: reservationId },
);
```

Customers deploy these services using their normal container pipeline or the
platform CLI's generated Kubernetes resources. The platform does not execute
arbitrary containers inside the Rust database process.

## Wire contract

Use versioned Protobuf definitions for RPC and an explicitly specified JSON
mapping for HTTP. Generate transport clients, then provide small ergonomic SDKs.
Transport packages do not independently implement retry or primitive semantics.
Keep WIT types separate but map them into the same internal validated operations;
conformance tests catch differences across adapters.

| Concern | Cross-language representation |
| --- | --- |
| SQL values | Tagged null, signed 64-bit integer, finite f64, UTF-8 text, bytes |
| JSON integers | Decimal strings for 64-bit fields; no lossy JS Number conversion |
| Binary payloads | Protobuf bytes; base64 in HTTP JSON; Uint8Array/bytes in SDKs |
| KV versions and receipts | Opaque bounded tokens, compared by server |
| Time | Explicit UTC milliseconds for persisted due times; separate request timeout budget |
| SQL rows | Ordered values plus column descriptors |
| User schemas | Versioned digest in deployment manifest; JSON Schema initially for command payloads |
| Request IDs | Caller-stable opaque IDs scoped by authenticated Cell identity |
| Errors | Stable code, outcome classification, request ID and retry advice |

Follow the [Protobuf JSON mapping](https://protobuf.dev/programming-guides/json/)
when exposing generated fields; custom typed payloads must declare their encoding.
Choose canonical request hashing after validation. Hash semantic typed values
using a specified encoding; arbitrary JSON key order or Protobuf serialization
order must not turn equivalent requests into different dedup identities.

Representative endpoints:

```text
POST /v1/cells:command          binding, partition, operation, request ID, input
POST /v1/cells:query            binding, partition, query, consistency
POST /v1/sql:batch              binding, partition, typed statements, request ID
POST /v1/sql:query              binding, partition, typed statement, read options
POST /v1/kv:atomic              binding, scope, checks, mutations, request ID
POST /v1/queues:send            binding, payload, dedup identity
POST /v1/queues:receive         binding, consumer credit, wait budget
POST /v1/queues:ack             binding, message ID, lease token, request ID
POST /v1/workflows:start        binding, workflow ID, input, request ID
POST /v1/workflows:signal       binding, run ID, signal ID, payload
POST /v1/activities:complete    binding, run/activity/attempt, token, result
GET  /v1/operations/{id}        resolve an indeterminate operation
```

Using body fields for arbitrary keys avoids interpreting binary keys or slashes
as path traversal. RPC equivalents carry the same data. Tenant/application
identity comes from authentication and authorized binding resolution; supplying
an application ID never grants access. Private forwarding carries verified
principal context, deployment capability and hop/deadline state.

Important errors include `VERSION_CONFLICT`, `REQUEST_ID_CONFLICT`,
`LEASE_LOST`, `RESOURCE_EXHAUSTED`, `SCHEMA_INCOMPATIBLE`, `SNAPSHOT_EXPIRED`
and `OUTCOME_UNKNOWN`. Distinguish outcomes `not_started`, `rejected`,
`committed` and `unknown`. A retryable boolean alone is insufficient. SDK retries
preserve request identity and operation bytes, have bounded budgets, and resolve
unknown outcomes rather than automatically issuing a new mutation.

## Embedded host boundary

The JS adapter resolves each imported binding to a Rust capability. A WASM
interface can similarly use a transaction resource valid for one invocation:

```wit
package crab:cell@1.0.0;

interface types {
  variant sql-value {
    null-value,
    integer(s64),
    real(float64),
    text(string),
    blob(list<u8>),
  }
  record statement {
    sql: string,
    params: list<sql-value>,
  }
  record cell-error {
    code: string,
    message: string,
  }
}

interface transaction {
  use types.{statement, cell-error};
  resource tx {
    execute: func(query: statement) -> result<u64, cell-error>;
    emit: func(binding: string, payload: list<u8>) -> result<_, cell-error>;
  }
}

world command {
  import transaction;
  use transaction.{tx};
  export invoke: func(scope: borrow<tx>, input: list<u8>)
    -> result<list<u8>, string>;
}
```

This is a proposed interface sketch requiring validation against the chosen WIT
toolchain. Full query/result operations and application errors are added when
the concrete adapter is implemented. Guest return does not call COMMIT: the
host validates output, closes capabilities, commits/captures/publishes, and then
responds. A trapped or invalid guest result rolls back if commit has not begun.

Each guest has memory/CPU limits, bounded host buffers and an import allowlist.
Do not allocate one isolate per registered Cell. Share compiled code and use a
bounded invocation pool; keep durable state in SQLite and treat guest globals as
disposable cache. Support persistent in-memory actor identity or WebSocket
hibernation only as separately specified features.

## Local development

Proposed application layout:

```text
service/
  crab.toml
  src/http.ts
  src/cells/inventory.ts
  src/workflows/fulfillment.ts
  migrations/inventory/0001.sql
  contracts/inventory.json
  workers/invoice/Dockerfile
  tests/
```

`crab-platform dev` runs the actual Rust Cell/primitive engine, chosen language
adapter and a local storage adapter. Fast local mode uses filesystem storage;
durability qualification mode uses isolated RustFS with the real conditional
publication path. The former must not be reported as cloud/provider proof.

The inspector shows active owner, published sequence, pending publication,
queues and workflow state without revealing provider credentials. Fault commands
exercise owner kill, storage failure, delayed publication and source-directory
loss. SDK contract tests run the same scenario against local and network
adapters. Deployment details are in [deployment](deployment.md).
