# Native Rust programming model and private peer protocol

[Index](README.md). These are proposed interfaces. Application developers add
ordinary Rust modules to Crab and rebuild its image; there is no language host,
runtime plugin loader or public primitive SDK.

## Compile-time application composition

`crab-http-server` is the sole composition root. The target source layout is:

```text
crates/crab-http-server/src/cells.rs
crates/crab-http-server/src/cells/repository.rs
crates/crab-http-server/src/cells/commands.rs
crates/crab-http-server/src/cells/queries.rs
crates/crab-http-server/src/cells/activities.rs
crates/crab-http-server/src/cells/migrations/*.sql
```

`cells.rs` constructs exactly one immutable registry before either listener
becomes ready. A module contributes stable descriptors and function pointers;
it cannot register after startup. The concrete target interface is:

```rust,ignore
pub trait CellModule: Send + Sync + 'static {
    const NAME: &'static str;

    fn descriptor(&self) -> &'static ModuleDescriptor;
    fn register(self, registry: &mut RegistryBuilder) -> Result<(), RegistryError>;
}

pub(crate) fn repository_registry() -> Result<Registry, RegistryError> {
    let mut registry = Registry::builder();
    registry.register(RepositoryModule)?;
    registry.register(QueueModule)?;
    registry.register(WorkflowModule)?;
    registry.finish()
}
```

`ModuleDescriptor` contains the module's stable namespace IDs, migration bytes
and digests, command/query IDs and codec versions, workflow definition digests,
and activity types. `register` binds each descriptor entry to one compiled Rust
function. `finish` sorts and validates descriptors, rejects missing or extra
bindings and duplicate IDs, and produces the canonical release bytes. It must
fail readiness if the runtime registry and release descriptor differ.

The trait is a source-level interface, not a stable ABI. Modules use normal
Cargo dependencies and are monomorphized or privately type-erased inside the
registry. No `libloading`, dynamic library, Wasmtime, V8, subprocess protocol or
network registration path is permitted. This lets command inputs and outputs
remain strongly typed inside Crab while the private peer codec carries only
bounded registered bytes between identical compatible binaries.

The crate dependency direction is fixed:

```text
crab-storage <- crab-ltx <- crab-cell-runtime <- crab-http-server
                                            ^
                                            |
                         compiled repository modules
```

Repository handlers may call narrow public contracts from existing Git crates,
but those calls occur in asynchronous activities after a durable SQL intention;
they do not add server or Git dependencies to `crab-cell-runtime`.

## Typed commands, not remotely shipped closures

The service boundary accepts serializable commands and queries. Local and remote
owners run the same validation, handler and publication path. A local call may
avoid a network hop, but cannot bypass identity, size or durability checks.

The current lower-level implementation is intentionally narrower than the typed
registry below. `CellRuntime::new` binds the node session and byte budget.
`CellRuntime::bootstrap` consumes an unpublished owned control, exclusively
creates the local SQLite file on its stable worker, runs the runtime schema and
one compiled Rust initializer atomically, captures and publishes the initial
root, and returns no handle before that root is authoritative.
`CellRuntime::activate_restored` consumes an unforgeable `CatalogProof`, verifies
that control belongs to that Cell and node session, reserves node activation,
opens control's exact immutable root as a fresh sparse writer on its stable SQL
worker, verifies persisted identity/schema/sequence, then returns a
capability-bound `CellHandle`. Its implemented command entry point is:

```rust,ignore
pub async fn execute<F>(
    &self,
    identity: MutationIdentity,
    operation_digest: Digest,
    now_ms: i64,
    operation_bytes: usize,
    max_result_bytes: usize,
    next_due_ms: Option<i64>,
    handler: F,
) -> Result<StoredOutcome>
where
    F: for<'tx> FnOnce(&rusqlite::Transaction<'tx>)
        -> Result<HandlerOutcome> + Send + 'static;

pub async fn query<F>(
    &self,
    operation_bytes: usize,
    max_result_bytes: usize,
    handler: F,
) -> Result<Vec<u8>>
where
    F: FnOnce(&rusqlite::Connection)
        -> Result<Vec<u8>> + Send + 'static;

pub async fn resolve(
    &self,
    identity: MutationIdentity,
    operation_digest: Digest,
    now_ms: i64,
    max_result_bytes: usize,
) -> Result<Resolution>;
```

This is an internal construction API, not the final application surface. The
registry must derive the digest and byte declarations from a registered codec,
hide the raw transaction behind `CommandContext`, and map `OutcomeUnknown` to
`PendingMutation`. HTTP code must not accept caller-selected digests, byte limits
or closures. The lower-level query shares mutation admission and FIFO ordering;
its SQLite connection is set to `query_only` for the callback and its output is
bounded before admission and again on the SQL worker. Resolve uses that same FIFO
but returns a typed committed, absent, unknown or expired observation and never
reruns the handler.

```rust,ignore
pub trait WireValue: Sized + Send + 'static {
    fn encode(&self, out: &mut BoundedEncoder) -> Result<(), CodecError>;
    fn decode(input: &[u8]) -> Result<Self, CodecError>;
}
pub trait Command: Send + Sync + 'static {
    const ID: u32;
    const CODEC_VERSION: u32;
    type Input: WireValue;
    type Output: WireValue;
    fn execute(
        &self, ctx: &mut CommandContext<'_>, input: Self::Input,
    ) -> Result<Self::Output, CommandError>;
}
pub trait Query: Send + Sync + 'static {
    const ID: u32;
    const CODEC_VERSION: u32;
    type Input: WireValue;
    type Output: WireValue;
    fn execute(
        &self, ctx: &mut QueryContext<'_>, input: Self::Input,
    ) -> Result<Self::Output, CommandError>;
}
impl CellClient {
    pub async fn command<C: Command>(
        &self, target: &CellTarget, identity: MutationIdentity, input: C::Input,
    ) -> Result<Committed<C::Output>, InvocationError>;
    pub async fn query<Q: Query>(
        &self, target: &CellTarget, minimum: Option<Receipt>, input: Q::Input,
    ) -> Result<Observed<Q::Output>, InvocationError>;
    pub async fn resolve(
        &self, pending: &PendingMutation,
    ) -> Result<Resolution, InvocationError>;
}
```

Committed contains output and receipt; InvocationError includes a stored
business rejection with receipt, a proven not-started failure, or PendingMutation
with identity/digest for unknown outcome. Never flatten these into a retryable
string error. SQL/LTX/storage errors preserve their sources internally; HTTP
mapping redacts SQL text, secrets and input bytes.

CellTarget is created only from an authorized namespace capability and partition,
not an arbitrary bucket/path. The registry selects handlers by namespace role,
control.code, command/query ID and codec version. Registration rejects duplicate
keys, missing migration digests and incompatible schema ranges before readiness.
Command IDs are unique per module; queries have a separate ID space. Methods are
not discovered from a Rust type name, TypeId or process address.

WireValue is a small explicit bounded codec, using the canonical scalar/length
encoding below. Each registered command supplies input/output fixtures; no
implicit serde enum discriminants or unspecified serialization defaults. Changing
field order, optionality or meaning requires a new codec version. A compiled
release retains supported old versions through outstanding request/outbox
lifetimes; it cannot retry old bytes against silently changed semantics.

## Transaction-scoped application API

CommandContext has private transaction and identity fields. It exposes sql(),
now_ms(), sequence(), cell_id() and emit(resolved_target, operation). QueryContext
exposes only read SQL and the observed receipt. Scoped SQL handles borrow the
context; statements/rows are materialized and cannot escape it. Neither context
is Send; neither exposes a raw connection, COMMIT or an async method.

```rust,ignore
// Target API sketch inside crab-http-server; input/output implement WireValue.
impl Command for AddComment {
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = AddCommentInput;
    type Output = CommentId;

    fn execute(&self, ctx: &mut CommandContext<'_>, input: Self::Input)
        -> Result<Self::Output, CommandError>
    {
        let id = input.comment_id;
        let now = ctx.now_ms();
        ctx.sql().execute(
            "INSERT INTO comments(id, issue_id, author_id, body, created_at_ms) VALUES (?, ?, ?, ?, ?)",
            params![id, input.issue_id, input.author_id, input.body, now],
        )?;
        Ok(id)
    }
}
```

The HTTP layer supplies author identity from authenticated Principal, validates
repository write access and input, and creates/preserves the operation identity.
The owner revalidates authorization before admission. The handler inserts only
application rows: runtime adds dedup, commits, captures and publishes. HTTP 201
is emitted only after Committed. A dropped HTTP waiter does not cancel accepted
publication. Browser retries must preserve the submission ID and original input.

SQL helpers implement the authorizer and bounds in [primitives](primitives.md).
A command can atomically modify several application tables and its local outbox.
Calling another Cell's KV/queue/workflow from inside that transaction is forbidden:
emit a durable intention instead. Native SQL is not a sandbox for hostile Rust;
all compiled handlers are trusted and reviewed.

## Primitive handles and asynchronous work

CellClient supplies typed SqlCell, KvNamespace, QueueNamespace and WorkflowNamespace
handles from registered capabilities. They expose exactly the operations in the
mapping below, reuse MutationIdentity/Receipt/Resolution, and call the actor
locally or forward privately. No new public /sql, /kv or /workflow API is added.

| Handle | Operations | Transaction boundary |
| --- | --- | --- |
| SqlCell | batch, query | One explicit-key Cell |
| KvNamespace | atomic, get, list | One scope-derived shard |
| QueueNamespace | send, claim, ack, retry, extend | One producer/consumer shard |
| WorkflowNamespace | start, signal, cancel, state | One workflow-ID shard |
| Native activity supervisor | claim, complete, fail, extend | Published claim and completion are separate commands |

WorkflowDefinition::transition(state, event, TransitionContext) -> Decision is
synchronous; Decision/Action are native owned Rust values. Activities are
registered asynchronous functions returning bounded bytes and typed failures.
Their context contains run/activity IDs, stable external idempotency key, attempt,
lease token and cancellation signal. No SQLite transaction spans their future.

```rust,ignore
pub trait WorkflowDefinition: Send + Sync + 'static {
    fn transition(&self, state: &[u8], event: &[u8], ctx: &TransitionContext)
        -> Result<Decision, CommandError>;
}
pub struct Decision {
    pub status: RunStatus,
    pub next_state: Vec<u8>,
    pub result: Option<Vec<u8>>,
    pub actions: Vec<Action>,
}
pub enum RunStatus { Running, Completed, Failed, Cancelled }
pub enum Action {
    Activity { activity_type: String, input: Vec<u8> },
    Timer { due_at_ms: i64 },
    Emit { target: ResolvedEffectTarget, operation: EffectOperation },
}
pub trait Activity: Send + Sync + 'static {
    fn run(&self, ctx: ActivityContext, input: Vec<u8>)
        -> impl Future<Output = Result<Vec<u8>, ActivityError>> + Send;
}
```

The registry binds a definition digest to the exact transition implementation
and its state/event codecs. Activity is registered generically; private
type-erasure is an implementation detail, not a dynamic-library ABI. Validate
Decision bounds before any writes and apply all actions in the same transaction.
Emit uses the pre-resolved destination procedure in primitives.md; action IDs
come from TransitionContext, never random callback-local state.

Use a node-wide Tokio supervisor with at most min(32, 2 * vCPU) running activities
and byte reservations for input/output. It cycles eligible shards, backs off
100 ms–1 s on empty claims, renews at lease/3, and delivers completion through
the command actor. Lease loss requests cancellation and prevents further
completion with that token; cancellation cannot undo an already issued network
effect. Keep task permits until actual task termination. Blocking activity work
uses a separate pool capped at min(vCPU, 4), never a SQL shard or the page-I/O
driver. Neither spawn_blocking nor future abortion can terminate arbitrary
native CPU loops.

Queue consumers follow the same lease supervision. Side effects use stable
message ID or (run_id, activity_id) at the destination, never attempt/token.
Pure workflow transitions are tested with fixed state/event/context fixtures;
Rust cannot prevent hidden clocks, randomness or network calls in trusted code.

## Crab integration entry points

| Existing source | Integration change to implement |
| --- | --- |
| [server.rs](../../../../crates/crab-http-server/src/server.rs) | Construct one runtime with existing resolved Store; retain handle and join/drain ownership in Server |
| [app.rs](../../../../crates/crab-http-server/src/app.rs) | Keep Principal/repository admission before native commands; map durable, rejected and unknown outcomes |
| [app_storage.rs](../../../../crates/crab-http-server/src/app_storage.rs) | Replace collaboration JSON persistence with typed SQL handlers after hard cutover |
| [config.rs](../../../../crates/crab-http-server/src/config.rs) | Extend existing config for local Cell data and private peers; do not add a second provider/auth stack |
| [main.rs](../../../../crates/crab-http-server/src/main.rs) | Keep current serve lifecycle; add release/migration administrative subcommands in the same executable |

The server startup order is normative:

1. Resolve existing Crab configuration, credentials and object-store `Store`.
2. Build and validate the static registry; derive its canonical release digest.
3. Verify root identity, catalog and active release compatibility.
4. Start bounded SQL/page-I/O/activity workers and the Cell supervisor.
5. Register the private peer route on the management router.
6. Construct product routes with a cloneable `CellClient` capability.
7. Become ready only after the management listener and public listener can use
   the same validated registry/runtime generation.

Shutdown reverses ownership: remove readiness, reject new work/acquisition,
drain accepted commands and activities, release Cell controls, close managed
SQLite handles, join workers, then stop listeners. `Server` owns the runtime
join handle; a detached global runtime or handler-created runtime is invalid.

Resolve repository UUID from the durable catalog; renaming owner/name must not
change its Cell ID. Keep issues/comments/pulls and their related application
tables in that repository Cell so their invariants can share one transaction.
Fixed primitive namespaces are provisioned only for actual callers, not empty
shards for every repository.
Preserve existing public HTTP safe-integer and JSON validation: internal SQLite
i64 values do not authorize emitting imprecise numbers to the React client.

Git receive, ref locks, pack/Xet/LFS publication and release assets stay on their
existing owners. A SQL merge/build intention is first published, then a native
activity invokes existing Git orchestration, then SQL records its verified
outcome. Never hold SQLite open while waiting for Git or claim atomicity across
these stores. Retain the existing Git-side journal/reconciliation evidence.

## Private transport and authenticated routing

[peer.proto](contracts/peer.proto) defines messages only, package
crab.cell.peer.v1; there is no generated public gRPC service. On the existing
private management listener, add POST /internal/cells/v1/forward with
application/x-protobuf. Its PeerRequest payload selects mutate, read, resolve,
deliver_effect or resolve_effect. PeerReply carries the corresponding result.
This path is never registered on the public router.

Target contains resolved tenant/application/namespace IDs (16 bytes each) and
partition (<=1024 bytes). Recompute Cell ID and shard; compare namespace role,
operation and authenticated capability. Envelope metadata carries origin session,
principal issuer/subject, action grants, release digest, issued/expiry time and
hop count. Metadata <=16 KiB, nested operation <=1 MiB. Reject malformed IDs,
unknown fields/enums, unset oneofs and unsupported protocol/codec versions
before actor admission. Reject oversized bodies during streaming decode.

Production peers use fleet-enrolled mTLS identities. Authorization is a signed
canonical envelope over payload BLAKE3, origin session, principal, grants,
release digest and a <=60 s validity interval. Sign with the node's enrollment
signing key; TLS and signature verification bind it to the enrolled boot session.
Signing bytes are ASCII crab.peer.v1 plus NUL, selected PeerRequest operation
field number as u16, then PeerAuthorization fields 1–8 in descriptor order using
the canonical codec below. Including the operation tag prevents interpreting
identical payload bytes as a different operation. Ed25519 signature is 64 bytes;
its enrollment public key is 32 bytes. Preserve the original nested operation
bytes on forwarding and verify their BLAKE3 before decoding; do not reserialize
them and silently invalidate the signature. Reject duplicate singular/oneof
fields and decreasing/expired envelope time bounds.

The signature field itself is excluded from signing; hop_count is transport
metadata outside the signature, authenticated per hop by mTLS. The receiver
rechecks current repository membership and action authorization. Peer membership
alone cannot elevate a browser user or select arbitrary trusted effect identities.
DeliverEffect additionally verifies enrolled runtime grant and source identity/
published outbox evidence. A health probe is not enrollment.

Native supervision uses issuer crab-runtime:<fleet-fingerprint> and subject
equal to the origin session's lowercase hex ID. Accept that principal only when
it matches verified enrollment and its compiled namespace/action grants; empty
browser identity fields never imply runtime authority. Browser-originated
commands carry the original issuer/subject and undergo normal repository checks.

Read cached owner hint; local requests go to CellHandle, remote requests go
directly to the enrolled owner's advertised endpoint. Maximum two forwards;
reject a third. On stale-owner response reload origin control once, then route
or acquire within remaining deadline. Never use the public Service for private
forwarding. Owner hints cap at 100K entries/32 MiB and 3 s TTL; cache hits do not
replace control publication checks. Never send credentials to an endpoint copied
from unverified control data; require the enrolled session/endpoint mapping.

Read describe=true may provision an explicit-key Cell only with create
capability; absent fixed shards return NOT_FOUND. A null bootstrap root returns
UNAVAILABLE until initialized schema publication. Describe supplies incarnation.
Restore invalidates cached incarnation; never transparently redirect old writes.

## Canonical digest, errors and retries

Validate issued_at_ms >=0, expires>issued, lifetime<=24h, issued<=now+5 min and
expires>now, again after mailbox wait. timeout_ms=0 means 30,000; otherwise
1..60,000. Request expiry bounds dedup; transport timeout bounds waiting only.
Changing identity timestamps changes the operation digest and conflicts on retry.

Compute BLAKE3 from decoded validated values, not raw Protobuf bytes:

1. ASCII crab.op.v1 followed by NUL, Cell ID, incarnation, request ID,
   issued/expires i64 big-endian and operation field number u16.
2. Message fields in ascending descriptor field number; defaults included.
   Exclude transport timeout, principal and peer envelope.
3. bool=u8; enum=u32; u32/u64/i64 fixed-width big-endian; f64 IEEE bits,
   rejecting non-finite values and normalizing negative zero.
4. Text/bytes=u32 byte length plus exact bytes; repeated=u32 count plus ordered
   values; nested=u32 encoded length plus bytes. No Unicode normalization.
5. oneof=u16 selected field plus value; optional=u8 presence plus value.
   Unset oneofs are invalid; no maps exist. Command input bytes use WireValue.

Unknown fields fail rather than hash an incomplete operation. Any field-set
change requires a protocol/digest version change. Test identical digests for
local/forwarded calls and reordered Protobuf fields. No JSON primitive transport
or cross-language value conversion is needed.

| Code | Crab HTTP mapping | Caller action |
| --- | --- | --- |
| INVALID_ARGUMENT | 400 | Correct input |
| PERMISSION_DENIED | 403 | Stop or refresh authorized identity |
| NOT_FOUND | 404 | Stop or explicitly provision |
| PRECONDITION_FAILED, REQUEST_ID_CONFLICT | 409 | Resolve application conflict |
| REQUEST_EXPIRED | 410 | Do not replay |
| LEASE_LOST | 409 | Drop task ownership, preserve historical receipt if present |
| RESOURCE_EXHAUSTED | 429 | Backoff within original identity/expiry |
| OUTCOME_UNKNOWN, UNAVAILABLE | 503 | Follow outcome classification, not HTTP code alone |
| SCHEMA_INCOMPATIBLE | 409 | Deploy compatible compiled definitions/schema |
| INTERNAL | 500 | Preserve identity; resolve UNKNOWN |

Transport authentication failures cannot imply that a previously accepted
attempt rolled back. CellClient retry ceiling is five attempts with
100/200/400/800 ms delays bounded by original timeout/expiry. Resolve committed/
rejected returns the stored reply; ABSENT permits the original mutation again;
UNKNOWN polls until deadline then returns PendingMutation. Never allocate a
fresh request ID automatically. Duplicate claims additionally revalidate their
lease before returning payloads, including through Resolve.
