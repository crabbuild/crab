# Native Rust programming model and private peer protocol

[Index](README.md). This document marks implemented interfaces explicitly; all
other signatures and source layouts are target contracts. Crab contributors add
ordinary Rust modules to the server source and rebuild the complete image;
repository owners do not provide executable code. There is no language host,
runtime plugin loader or public primitive SDK.

## Compile-time application composition

`crab-http-server` is the sole composition root. The current source layout is:

```text
crates/crab-http-server/src/cells.rs
crates/crab-http-server/src/cells/repository.rs
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

pub(crate) fn compiled_registry() -> Result<Registry, RegistryError> {
    let mut builder = RegistryBuilder::new(BuildDescriptor {
        source_revision: source_revision().to_owned(),
        cargo_lock_digest: digest(include_bytes!("../../../Cargo.lock")),
    });
    builder.register(RepositoryModule)?;
    builder.finish()
}
```

Primitive registries are reusable runtime mechanics; they are not automatically
part of the product release. Add a KV, Queue or Workflow module to this function
only with a concrete Crab route/activity caller, its namespace and migration,
and the corresponding hard-cutover data plan.

`ModuleDescriptor` contains the module's stable namespace IDs, migration bytes
and digests, command/query IDs and codec versions, workflow definition digests,
and activity types. `register` binds each descriptor entry to one compiled Rust
function. `finish` sorts and validates descriptors, rejects missing or extra
bindings and duplicate IDs, and produces the canonical release bytes. It must
fail readiness if the runtime registry and release descriptor differ.

Implementation status: `crab-cell-runtime::RegistryBuilder` now registers static
`CellModule` descriptors and compiled command/query function pointers, then
freezes them into an immutable `Registry`. `finish` rejects descriptor/binding
drift, duplicate identifiers, invalid migration digests/schema coverage,
unbounded codec declarations, invalid namespace/effect/DLQ topology, and
duplicate workflow/activity inventory. Registration order produces identical
canonical release bytes, module code digests and release digest. Dispatch uses
`CommandContext`/`QueryContext`, which expose bounded authorized SQL and metadata
without a raw connection accessor. `BoundedEncoder`/`BoundedDecoder` implement
fixed-width big-endian scalars, length-delimited bytes/text and strict tags;
they reject incomplete/trailing input, non-finite floats and declared-limit
overflow; encoding normalizes negative zero while decoding rejects its
non-canonical bit pattern. Generic `Command`/`Query` registration uses
monomorphized decode/execute/encode trampolines, with no raw byte-handler
registration escape hatch. The implemented `CellClient` covers canonical
operation-digest integration across both the local actor path and authenticated
peer transport. The peer transport signs requests, strictly decodes replies and
preserves mutation evidence for Resolve. The server implementation selects the
authoritative owner, authenticates its live advertisement, uses pinned mTLS and
retries one definitely-not-started stale-owner attempt without retrying ambiguous
mutations. Typed local SQL, KV, Queue
and Workflow capabilities are implemented. The server composition root now
compiles and binds create/update and get/list operations for issues and comments
with its repository identity/sequence/issue/comment migration. A server
integration test drives all eight bindings through the runtime, publishes each
decision through LTX, removes the first local SQLite database and restores the
updated detail and list results under a second owner. The built-binary release
inspection command exposes the resulting exact canonical registry bytes.

The currently assigned repository operation inventory is fixed below. IDs are
scoped independently to commands and queries; a later operation must use a new
ID or a new codec version and retain any version referenced by authoritative
roots during rollout.

| Kind | ID | Rust type | Input bound | Output bound | Transaction effect |
| --- | ---: | --- | ---: | ---: | --- |
| command | 1 | `CreateIssue` | 80 KiB | 80 KiB | Allocate issue number, insert row, advance app revision |
| command | 2 | `CreateComment` | 80 KiB | 80 KiB | Reject if issue is absent; otherwise allocate number, insert row, advance app revision |
| command | 3 | `UpdateIssue` | 96 KiB | 80 KiB | Check actor/metadata permission and version, replace supplied fields, advance app revision |
| command | 4 | `UpdateComment` | 80 KiB | 80 KiB | Check author and version, replace body, advance app revision |
| command | 5 | `MaintenanceTick` | 8 B | 5 B | Reject stale root position or advance at most 128 due items and recompute summary |
| command | 6 | `EffectClaim` | 8 B | 1 MiB | Claim at most one due source effect under a published lease |
| command | 7 | `EffectLease` | 1 MiB | 9 B | Acknowledge delivery or schedule the same effect bytes for retry |
| query | 1 | `GetIssue` | 8 B | 80 KiB | Primary-key read |
| query | 2 | `GetComment` | 16 B | 80 KiB | `(issue, number)` primary-key read |
| query | 3 | `ListIssues` | 1 KiB | 1 MiB | Descending cursor/state/search page, at most 50 results and 200 number probes |
| query | 4 | `ListComments` | 32 B | 1 MiB | Descending cursor page, at most 50 results and 200 number probes |
| query | 5 | `EffectValidate` | 1 MiB | 1 B | Validate the exact published source lease at a minimum receipt |

All twelve use codec version 1 and schema version 1. Issue/comment numbers and
versions are positive integers no larger than 9,007,199,254,740,991. The
initializer writes the catalog repository UUID to the singleton identity row;
handlers fail the complete application savepoint if that row is missing or its
application revision is exhausted. The durable missing-issue rejection does not
consume a comment number or advance the application revision, but does advance
the runtime command sequence and therefore carries a receipt. Author, metadata
permission, missing-row and version failures on updates are also durable typed
rejections. Label IDs and assignee subjects are canonical sorted bounded vectors
stored with each issue; the HTTP adapter remains responsible for checking them
against the current label and repository-member catalogs before dispatch.
One exact byte fixture for every command input/output and query input/output
pins codec v1 independently of descriptor construction and runtime dispatch.

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

An application domain may be split into a private workspace `rlib` when that
reduces product-code complexity. Its exported surface is ordinary typed Rust;
the descriptor, handler bindings, HTTP authorization adapter and lifecycle
wiring still terminate in `crab-http-server`. Do not use `dylib`, `cdylib`,
`libloading`, subprocesses or a module discovery registry. A separate crate does
not gain storage credentials, a listener or a deployable artifact, and cannot
register itself after `RegistryBuilder::finish`.

## Contributor change and release procedure

One product capability is delivered as one reviewed vertical slice. Its pull
request must make these changes together; none is a separately deployable unit:

1. Add or extend an application migration under
   `crab-http-server/src/cells/migrations/`. Use a monotonic schema version and
   pin the exact migration digest in the module descriptor.
2. Define stable command/query IDs, codec versions and independent byte fixtures
   under `crab-http-server/src/cells/`. IDs and published codec meanings are never
   reused. The codec owns bounded decode before runtime admission.
3. Implement a synchronous transaction-scoped Rust handler. External Git,
   object-store or network work is represented by an outbox intention and runs
   in a registered asynchronous activity after SQL publication.
4. Register every descriptor and function binding in `cells.rs`. Startup fails
   if migration, descriptor, fixture and binding inventories disagree.
5. Adapt an existing authenticated Crab HTTP route to a typed `CellClient` call.
   The route resolves repository capability and Cell target; it cannot accept a
   primitive name, command ID, handler name, digest or byte budget from the user.
6. Add transaction, publication, retry/Resolve, private-forwarding and product
   HTTP tests. The browser-visible test must enter through the real route and
   verify a result after restoring the owner from the object-store root.
7. Build one `crab-http-server` image, run `cells release inspect --json` against
   that binary, prepare the descriptor, and use the normal compatible or
   maintenance fleet rollout. There is no module-only deployment or rollback.

For a separately operated product, the same procedure runs in that product's
Crab source fork and CI. The operator deploys the resulting complete Crab image
to its own fleet and object-store namespace. The architecture does not create a
hosted extension marketplace or accept third-party code into an already running
fleet.

This procedure deliberately couples application and runtime compatibility to
the server release. A Cargo feature, repository setting or environment variable
must not select between old and new persistence implementations. During the hard
cutover, old application storage exists only as importer input; all serving uses
the registered Cell path.

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

pub async fn deliver_effect<F>(
    &self,
    delivery: InboxDelivery,
    now_ms: i64,
    operation_bytes: usize,
    max_result_bytes: usize,
    handler: F,
) -> Result<StoredOutcome>
where
    F: for<'tx> FnOnce(&rusqlite::Transaction<'tx>)
        -> Result<HandlerOutcome> + Send + 'static;

pub async fn resolve_effect(
    &self,
    delivery: InboxDelivery,
    now_ms: i64,
    max_result_bytes: usize,
) -> Result<Resolution>;

pub async fn migrate(
    &self,
    plan: MigrationPlan,
    now_ms: i64,
) -> Result<MigratedCell>;
```

This is an internal construction API, not the final application surface. The
implemented `CellClient` derives the digest and byte declarations from a
registered codec, hides the raw transaction behind `CommandContext`, and maps
`OutcomeUnknown` to `PendingMutation`. HTTP code cannot select module names,
digests, byte limits or closures. The lower-level query shares mutation admission and FIFO ordering;
its SQLite connection is set to `query_only` for the callback and its output is
bounded before admission and again on the SQL worker. Resolve uses that same FIFO
but returns a typed committed, absent, unknown or expired observation and never
reruns the handler. The effect entry points are private runtime construction
APIs: they share Cell admission, FIFO actor ordering, SQL-worker affinity,
cancellation-safe execution, exact-root publication and fence/unknown semantics.
They are not exposed to application or browser callers; the implemented peer
translator derives `InboxDelivery` only from authenticated source evidence.

Schema migration is likewise a private Rust control capability. The caller can
obtain SQL only through the frozen registry, and migration consumes the old
handle rather than letting old schema-bound codecs continue through its clones:

```rust,ignore
let Some(plan) = registry.next_migration(
    repository_namespace,
    handle.code(),
    handle.schema(),
)? else {
    return Ok(handle);
};

let MigratedCell { handle, outcome } = handle.migrate(plan, now_ms).await?;
assert_eq!(handle.schema(), outcome.schema);
```

`next_migration` currently accepts only the exact current module code and returns
one adjacent schema step. `migrate` places that step behind already accepted Cell
work, executes the static SQL on the assigned worker, records its digest, captures
LTX and publishes root/schema/code together. It returns the replacement handle
only after the authority CAS is proven. Catalog iteration, predecessor-code
retention and code-only rollover belong to release activation and are not yet
implemented by `crab-http-server`.

```rust,ignore
pub trait WireValue: Sized + Send + 'static {
    fn encode(&self, out: &mut BoundedEncoder) -> Result<(), CodecError>;
    fn decode(input: &mut BoundedDecoder<'_>) -> Result<Self, CodecError>;
}
pub trait Command: Send + Sync + 'static {
    const MODULE: &'static str;
    const ID: u32;
    const CODEC_VERSION: u32;
    type Input: WireValue;
    type Output: WireValue;
    fn execute(ctx: &mut CommandContext<'_, '_>, input: Self::Input)
        -> Result<CommandResult<Self::Output>>;
}
pub trait Query: Send + Sync + 'static {
    const MODULE: &'static str;
    const ID: u32;
    const CODEC_VERSION: u32;
    type Input: WireValue;
    type Output: WireValue;
    fn execute(ctx: &mut QueryContext<'_>, input: Self::Input)
        -> Result<Self::Output>;
}
impl CellClient {
    pub fn local(registry: Arc<Registry>, handle: CellHandle) -> Self;
    pub fn peer(
        registry: Arc<Registry>,
        signer: Arc<PeerSigner>,
        principal: PeerPrincipal,
        round_trip: Arc<dyn PeerRoundTrip>,
    ) -> Self;
    pub async fn command<C: Command>(
        &self, target: &CellTarget, identity: MutationIdentity, input: C::Input,
    ) -> Result<Committed<C::Output>, InvocationError<C::Output>>;
    pub async fn query<Q: Query>(
        &self, target: &CellTarget, minimum: Option<Receipt>, input: Q::Input,
    ) -> Result<Observed<Q::Output>, InvocationError<Q::Output>>;
    pub async fn resolve(
        &self, pending: &PendingMutation,
    ) -> Result<Resolution, InvocationError<Vec<u8>>>;
}
```

Committed contains output and receipt; InvocationError includes a stored
business rejection with receipt, a proven not-started failure, PendingMutation
with identity/digest for unknown outcome, or an invalid typed payload carrying
the already-published receipt. The last case is not safe to replay as though the
mutation never ran. Never flatten these into a retryable string error.
SQL/LTX/storage errors preserve their sources internally; HTTP mapping redacts
SQL text, secrets and input bytes.

Both transports describe the active handle before dispatch and check
target Cell, incarnation, registry-owned namespace, canonical module code and
schema range. It hashes `crab.op.v1`, Cell/incarnation, immutable mutation
identity, the CellCommand tag, typed command ID/codec version and exact encoded
input. The private describe reply therefore carries Cell ID, incarnation, code
and schema; a boolean existence response is insufficient to enforce the same
fence remotely. The receiving dispatcher rechecks the description immediately
before actor admission. A query reads its receipt from `sys_meta` on the same
FIFO SQL worker and fails if it cannot satisfy the requested minimum.

CellTarget is created only from an authorized namespace capability and partition,
not an arbitrary bucket/path. The registry selects handlers by namespace role,
control.code, command/query ID and codec version. Registration rejects duplicate
keys, missing migration digests and incompatible schema ranges before readiness.
Command IDs are unique per module; queries have a separate ID space. The module
name is an associated constant on the compiled Rust type, not a route parameter.
Methods are
not discovered from a Rust type name, TypeId or process address.

WireValue is a small explicit bounded codec, using the canonical scalar/length
encoding below. Each registered command supplies input/output fixtures; no
implicit serde enum discriminants or unspecified serialization defaults. Changing
field order, optionality or meaning requires a new codec version. A compiled
release retains supported old versions through outstanding request/outbox
lifetimes; it cannot retry old bytes against silently changed semantics.

## Transaction-scoped application API

`CommandContext` has private transaction and identity fields. It exposes
`sql(&SqlBatch)`, `now_ms()`, `sequence()`, `cell_id()`, the validated
`target()` and one
command-scoped `effect_batch()`. `QueryContext` exposes `sql(&SqlBatch)`,
`now_ms()`, `commit_sequence()` and `cell_id()`. Results are materialized and
cannot retain a SQLite statement or row. Neither context exposes a raw
connection, COMMIT or an async method.

```rust,ignore
// Compiled inside crab-http-server; input/output implement WireValue.
impl Command for RecordNote {
    const MODULE: &'static str = RepositoryModule::NAME;
    const ID: u32 = 1;
    const CODEC_VERSION: u32 = 1;
    type Input = RecordNoteInput;
    type Output = RecordNoteOutcome;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        input: Self::Input,
    ) -> crab_cell_runtime::Result<CommandResult<Self::Output>> {
        let results = context.sql(&SqlBatch {
            statements: vec![SqlStatement {
                sql: "INSERT INTO repository_notes(note_id, body) VALUES (?, ?)".into(),
                parameters: vec![
                    SqlValue::Integer(input.note_id),
                    SqlValue::Text(input.body),
                ],
            }],
        })?;
        if results[0].rows_affected != 1 {
            return Err(crab_cell_runtime::Error::Command(
                "note insert changed an unexpected row count",
            ));
        }
        Ok(CommandResult::Success(RecordNoteOutcome {
            note_id: input.note_id,
        }))
    }
}
```

The HTTP layer supplies author identity from authenticated Principal, validates
repository write access and input, preserves the product submission ID, and
creates a bounded runtime `MutationIdentity` for that HTTP attempt. The owner
revalidates authorization before admission. The compiled handler owns permanent
domain submission deduplication; runtime owns exact-attempt deduplication,
commit, capture and publication. HTTP 201 is emitted only after Committed. A
dropped HTTP waiter does not cancel accepted publication. A later browser retry
uses the same submission ID and original identity/content with a fresh runtime
identity; it returns the current visible object even after `sys_requests`
retention expires.

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
| Maintenance Tick | bounded expiry, lease recovery, timers, retention | One scanned root position |

Implementation status: `KvNamespace<M>`, `SqlCell<M>` and `QueueNamespace<M>`
are complete for local routing. A
compile-time `KvModule` supplies atomic/get/list IDs and codec version;
`register_kv` binds those typed handlers to the static registry. The capability
hashes scope into its fixed shard, maps failed checks to durable typed rejection,
and exposes receipted point/list reads whose TTL time is sampled by the owner.
Its integration test covers publication, rejection rollback, list encoding and
exact-root restore. A compile-time `SqlModule` supplies batch/query IDs and
`register_sql` binds bounded typed `SqlBatch`/`SqlResultSet` codecs. `SqlCell`
requires an explicit SQL-role target, publishes write batches and enforces
read-only minimum-receipt queries. Its integration test proves LTX publication,
query mutation rejection and exact-root restore. `QueueModule` extends
`MaintenanceModule` and supplies a fixed namespace plus send/claim/lease/query
IDs; the supertrait supplies the Tick ID and optional typed dead-letter target.
`register_queue` binds canonical bounded codecs and Tick as one startup unit.
Registry freeze verifies the source descriptor and the target Queue's module,
namespace, shard count, send command ID and codec version. `QueueNamespace`
derives producer shards, loads the immutable
shard count from the registry, publishes claims before returning payloads,
revalidates exact tokens at a minimum receipt and exposes token-bound ack/retry/
extend commands. Every dead transition emits at most one owner-independent
typed Queue-send effect in the same SQLite transaction and links the source row
for retention. Unit coverage proves atomic linkage, rollback, singleton Tick
emission and pending-effect payload retention; integration coverage proves
durable producer conflict, publication, validation, exact-root restore and
acknowledgement.

The compiled Queue contract is intentionally one Rust type and one registration
call. The Tick ID is mandatory because expiry and lease death must run even when
no consumer calls `claim`:

```rust,ignore
impl MaintenanceModule for BuildQueue {
    const MODULE: &'static str = "build-queue";
    const TICK_COMMAND_ID: u32 = 4;
    const QUEUE_DEAD_LETTER: Option<QueueDeadLetterTarget> = Some(
        QueueDeadLetterTarget::new(
            "build-dead-letter",
            BUILD_DEAD_LETTER_NAMESPACE,
            16,
            1,
            1,
        ),
    );
}

impl QueueModule for BuildQueue {
    const NAMESPACE: NamespaceId = BUILD_QUEUE_NAMESPACE;
    const SEND_COMMAND_ID: u32 = 1;
    const CLAIM_COMMAND_ID: u32 = 2;
    const LEASE_COMMAND_ID: u32 = 3;
    const VALIDATE_QUERY_ID: u32 = 1;
}

impl CellModule for BuildQueue {
    const NAME: &'static str = "build-queue";

    fn register(self, registry: &mut RegistryBuilder) -> Result<()> {
        register_queue::<Self>(registry)
    }
}
```

The `build-dead-letter` module separately implements the same Queue traits for
`BUILD_DEAD_LETTER_NAMESPACE`. Its namespace descriptor must declare 16 shards
and send command `(1, 1)`. The source descriptor must include that namespace in
`effect_targets` and `dead_letter`. Any drift aborts `RegistryBuilder::finish`
before the server opens listeners.

`WorkflowModule` binds one namespace, one current definition for new runs, a
static retained-definition inventory, and fixed start/signal/cancel/state IDs;
`register_workflow` installs their typed codecs and every exact definition-digest
binding. Signal and activity completion load the stored run digest before
selecting transition code, so a rollout can start new runs without stranding old
ones. Each definition declares its effect-target namespace set; registry freeze
rejects descriptor, transition-function, definition/activity-matrix and target-union
drift. `WorkflowNamespace` derives its shard solely
from the workflow ID and registry topology, binds the start run identity to the
runtime mutation identity, returns durable typed rejection for non-applied
outcomes and supports bounded minimum-receipt state reads. Its integration test
proves start, signal, duplicate replay, conflict rejection, exact-root restore,
state and cancellation. `WorkflowActivities<M>` and
`ActivitySupervisor<M>` now implement the first native execution unit: registry
freeze verifies the exact definition/type/handler matrix, claims and validation
cross a published receipt, the statically linked future runs without a SQLite
borrow, heartbeat extensions publish as independent commands, and completion or
retry feeds the pinned state machine. The activity context exposes stable run,
activity and external-idempotency identities, the current durable lease deadline
and cooperative cancellation. Pending mutations retain their exact identity for
resolution. `register_workflow_activities::<M>` also installs a type-erased
namespace runner in the compiled registry. The server calls that runner from
catalog-derived work without carrying `M` through its product composition
types. The maintenance Tick owns timer dispatch.

`MaintenanceModule` binds one private Tick operation ID. A scanner submits the
published root's commit sequence; `MaintenanceTickCommand` no-ops stale scans,
uses one budget across every installed primitive, and relies on the executor to
publish the new scheduler summary. Application routes never construct Tick
requests. `CatalogShardScan` keeps a fixed head revision while reading one
digest-verified page at a time. `DueCellScan` bounds each step to 32 control
reads; a zero-result batch still advances the scan. Rendezvous selection is pure
and independent of node-list ordering. `RepositoryCellScheduler` performs that
orchestration every second: it loads at most 10,000 exact live sessions, assigns
all 256 shards, processes at most 128 due Cells, and routes each one to the
existing local or authenticated remote owner. It may acquire an Idle Cell or
perform the normal observed stale-owner takeover; activations created solely for
the scheduler drain after the Tick/activity/effect cycle, while already-active
Cells stay owned. A zero-item Tick consults the registry's type-erased
activity/effect runner table. Activity execution uses CPU-derived admission
capped at 16 jobs, a one-job-per-Cell reservation and a tracked `JoinSet`, so
the scan completes without waiting
for the native future; scheduler shutdown aborts and joins every job, triggering
cooperative cancellation. Effect-only work executes inline, and an activity job
runs one effect step after its activity outcome. Each successful full cycle advances the shared node progress used by the
heartbeat. `SchedulerFleet` remembers the last progress change for every live
session and excludes a stalled session after 15 seconds, allowing the next
rendezvous candidate to scan; progress recovery readmits it. A session with zero
free memory, free disk or job credits is also excluded until a later signed
refresh restores all three; these fields remain hints and never replace local
admission. Local readiness and
Prometheus scheduler health/progress/lag use the same completion timestamp, and
readiness cannot open before the first completed cycle. Exact compiled operation
IDs/codecs map peer requests to scheduler, activity or effect grants;
unregistered and product operations fail closed. Transactional maintenance now
protects every installed work class from starvation and passes unused capacity
forward. Scanner cycles preserve one revision-pinned cursor per assigned shard,
offer every shard one attempt before filling unused capacity, rotate their
starting shard and reload the newest catalog head only after a complete pass.
Failed processing leaves `next_due_ms` durable and is retried on the next pass,
so neither an early Cell nor a busy namespace can pin later entries. Runtime
integration now proves an unchanged-owner takeover after node/local-volume loss,
exact-root restore, lease reclaim and safe activity retry under a second
session. Real multi-Pod process/network fault qualification remains. For a
locally owned Cell,
`CellRuntime::local_handle` asks the dispatcher for a capability and
returns one only if the scanned control's session/incarnation/code/schema still
match an unfenced, non-draining active entry. Callers never inspect the runtime's
Cell map.

```rust,ignore
pub trait WorkflowModule: Send + Sync + 'static {
    const MODULE: &'static str;
    const NAMESPACE: NamespaceId;
    const CURRENT_DEFINITION: &'static dyn WorkflowDefinition;
    const DEFINITIONS: &'static [&'static dyn WorkflowDefinition];
    // Fixed codec and operation IDs omitted.
}
```

`DEFINITIONS` is executable code, not metadata: keep an older entry until a
maintenance inventory proves no retained run, activity or timer references its
digest. `CURRENT_DEFINITION` must be one of those entries. Adding a new current
definition is an ordinary fleet release; removing old code is a separately
gated cleanup release.

`WorkflowDefinition::transition(state, event, WorkflowContext) -> WorkflowDecision` is
synchronous; decisions/actions are native owned Rust values. Activities are
registered asynchronous functions returning bounded bytes and typed failures.
Their context contains run/activity IDs, stable external idempotency key, attempt,
lease token and cancellation signal. No SQLite transaction spans their future.

```rust,ignore
pub trait WorkflowDefinition: Send + Sync + 'static {
    fn digest(&self) -> Digest;
    fn effect_targets(&self) -> &'static [NamespaceId] {
        &[]
    }
    fn transition(
        &self,
        state: &[u8],
        event: &[u8],
        context: WorkflowContext,
    ) -> Result<WorkflowDecision>;
}
pub struct WorkflowDecision {
    pub status: WorkflowStatus,
    pub state: Vec<u8>,
    pub result: Option<Vec<u8>>,
    pub actions: Vec<WorkflowAction>,
}
pub enum WorkflowAction {
    Activity {
        activity_type: String,
        input: Vec<u8>,
        due_at_ms: i64,
        expires_at_ms: i64,
    },
    Timer { due_at_ms: i64 },
    Effect { intent: EffectCommandIntent },
}
pub trait ActivityHandler: Send + Sync + 'static {
    const TYPE: &'static str;
    fn execute(
        context: ActivityContext,
        input: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = ActivityExecution> + Send + 'static>>;
}
pub trait BlockingActivityHandler: Send + Sync + 'static {
    const TYPE: &'static str;
    fn execute(context: ActivityContext, input: Vec<u8>) -> ActivityExecution;
}

register_activity::<RepositoryWorkflow, FetchMetadata>(&mut registry)?;
register_blocking_activity::<RepositoryWorkflow, BuildPack>(&mut registry)?;
```

The registry binds a definition digest to the exact transition implementation
and its state/event codecs. Activity is registered generically; private
type-erasure is an implementation detail, not a dynamic-library ABI. Validate
`WorkflowDecision` bounds before any writes and apply all actions in the same
transaction. Action IDs come from `WorkflowContext`, never random callback-local
state. `WorkflowContext::source()` exposes the validated source target.
Definitions construct effect targets with the same tenant/application and
declare every allowed destination namespace through `effect_targets()`; a
cross-tenant, cross-application or undeclared target is rejected before
event/state writes. The action contains only `EffectCommandIntent`, so
definitions cannot persist pre-encoded peer requests or pin an owner
incarnation.

The current node-wide Tokio supervisor admits at most `min(vCPU, 16)` running
activity jobs and one job per Cell. It retains both reservations until the
future terminates, preventing a scheduler-only activation from draining under a
second activity on the same actor. `ActivitySupervisor` renews at lease/3 and
delivers completion through the command actor. Lease loss requests cancellation and prevents further
completion with that token; cancellation cannot undo an already issued network
effect. Scheduler shutdown aborts and joins every tracked future, which drops
the attempt guard and signals cooperative cancellation. Before claim, the
server reserves the maximum 256 KiB input plus 256 KiB output from the same
node-wide byte semaphore as queued Cell commands and holds it through
completion; exhaustion leaves the durable activity unclaimed. Fair
cycling uses retained revision-pinned cursors and rotates across assigned shards
so a hot namespace cannot monopolize the cycle.

`BlockingActivityHandler` uses the dedicated `BlockingActivityPool`, never
Tokio's detached `spawn_blocking` path. For a namespace with any blocking type,
the scheduler reserves one fixed worker slot before durable claim and transfers
that reservation into the actual callback job. The job keeps it after waiter
cancellation until the callback returns. Pool shutdown stops admission, drains
submitted jobs and joins every named OS thread. Panic is isolated per attempt.
Because Rust cannot force-stop an arbitrary native CPU loop, handlers must still
observe `ActivityContext::cancellation`; a non-returning callback causes the
server's bounded shutdown to terminate the process rather than recycle its slot.

```mermaid
sequenceDiagram
    participant S as RepositoryCellScheduler
    participant A as Node/byte/Cell admission
    participant B as BlockingActivityPool
    participant C as Cell actor
    participant R as Compiled Registry

    S->>A: try reserve job + 512 KiB + Cell
    alt namespace contains a blocking binding
        S->>B: try_reserve()
        B-->>S: owned slot or exhausted
    end
    Note over S,C: Exhaustion stops here; no durable lease exists
    S->>C: claim and publish activity lease
    C-->>S: claim receipt
    S->>C: validate exact published lease
    S->>R: dispatch module + definition + type
    alt asynchronous handler
        R-->>S: await native Future
    else blocking handler
        R->>B: submit callback with owned slot
        B-->>R: result; slot released only here
    end
    S->>C: publish completion or retry
    Note over S,B: Cancellation drops the supervisor, but a submitted blocking job retains its slot
```

Queue consumers follow the same lease supervision. Side effects use stable
message ID or (run_id, activity_id) at the destination, never attempt/token.
Pure workflow transitions are tested with fixed state/event/context fixtures;
Rust cannot prevent hidden clocks, randomness or network calls in trusted code.

## Crab integration entry points

| Existing source | Integration change to implement |
| --- | --- |
| [server.rs](../../../../crates/crab-http-server/src/server.rs) | Constructs one runtime with the existing resolved Store, starts the catalog-driven Cell scheduler after node publication, invokes compiled maintenance/activity/effect runners, cancels/joins tracked activity work during shutdown and then drains/joins the runtime |
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
7. Publish the signed node session and start the repository due scanner using
   the same registry, directory, owner and router.
8. Become ready only after the management listener and public listener can use
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

The peer module in `crab-cell-runtime` is the implemented wire and dispatch
boundary. It generates private Rust messages from the checked-in descriptor and
signs canonical
authorization bytes, but preserves and hashes the exact nested Protobuf bytes.
Its strict pre-decoder rejects unknown fields, duplicate singular fields and
duplicate oneofs before Prost can discard that evidence. The verifier binds the
enrolled session public key, release digest, original principal/actions, current
time, decreasing deadline and operation tag. The HTTP route must use this
verifier and must not decode `PeerRequest` directly.

`PeerRoundTrip` is the only network ownership boundary exposed to the embedded
server. `crab-http-server::PeerHttpRoundTrip` implements it by reloading the
Cell control record, rejecting a self owner, loading the exact live enrolled
session, requiring endpoint equality, and constructing or reusing an mTLS client
pinned to the advertised CA-valid hostname, leaf SHA-256 and Ed25519 SPKI. It
sends the already-signed bytes to the fixed private path with the remaining
deadline and returns exact response bytes. `CellClient::peer` translates typed
describe/command/query/Resolve calls
through this boundary and strictly validates the response. `PeerDispatcher`
accepts only a `VerifiedPeerRequest`, invokes `PeerAuthorizer` before resolving
the target, asks `PeerCellResolver` for a currently active local `CellHandle`,
and executes through the same `LocalCellTransport` as an in-process call. This
keeps registry selection, operation digests, receipts, durable rejections and
unknown-outcome behavior identical across ingress nodes.

Generic compiled Cell-command effects now use the same strict boundary.
`EffectPeerClient::deliver` accepts only an `EffectClaim` whose caller has
validated the published lease, decodes its canonical stored `EffectRequest`,
rechecks source Cell/incarnation/sequence, target Cell, expiry and digest,
performs a fresh Describe, then copies the described incarnation into the
transient signed request. The durable request and inbox digest keep that field
empty, so destination takeover does not change effect identity. `PeerDispatcher`
rechecks the derived effect ID and current target incarnation before calling
`CellHandle::deliver_effect`. Ambiguous transport or
publication returns `EffectOutcomeUnknown`; `EffectPeerClient::resolve` queries
the destination inbox with the same identity and digest. The source-side Rust
API is:

```rust,ignore
pub trait EffectModule: Send + Sync + 'static {
    const MODULE: &'static str;
    const CODEC_VERSION: u32 = 1;
    const CLAIM_COMMAND_ID: u32;
    const LEASE_COMMAND_ID: u32;
    const VALIDATE_QUERY_ID: u32;
}

pub fn register_effect_delivery<M: EffectModule>(
    registry: &mut RegistryBuilder,
) -> Result<()>;

impl EffectBatch {
    pub fn new(
        transaction: &Transaction<'_>,
        source: &CellTarget,
        command_sequence: u64,
        now_ms: i64,
    ) -> Result<Self>;
    pub fn insert_command(
        &mut self,
        transaction: &Transaction<'_>,
        intent: &EffectCommandIntent,
    ) -> Result<[u8; 32]>;
}

impl<M: EffectModule> EffectSource<M> {
    pub fn new(client: CellClient, target: CellTarget) -> Self;
    pub async fn claim(
        &self,
        identity: MutationIdentity,
        request: EffectClaimRequest,
    ) -> Result<Committed<Vec<EffectClaim>>, InvocationError<Vec<EffectClaim>>>;
    pub async fn validate(
        &self,
        claimed: Vec<EffectClaim>,
        minimum: Receipt,
    ) -> Result<Observed<bool>, InvocationError<bool>>;
    pub async fn ack(
        &self,
        identity: MutationIdentity,
        claim: EffectClaim,
        result: Vec<u8>,
    ) -> Result<Committed<EffectLeaseOutcome>, InvocationError<EffectLeaseOutcome>>;
    pub async fn retry(
        &self,
        identity: MutationIdentity,
        claim: EffectClaim,
    ) -> Result<Committed<EffectLeaseOutcome>, InvocationError<EffectLeaseOutcome>>;
}

impl<M: EffectModule> EffectSupervisor<M> {
    pub fn new(
        source: EffectSource<M>,
        peer: EffectPeerClient,
        lease_ms: u32,
    ) -> Result<Self>;
    pub async fn run_once(&self)
        -> Result<EffectRunOutcome, EffectSupervisorError>;
}
```

`EffectCommandIntent` carries a validated `CellTarget`, fixed command ID/codec,
bounded encoded input and expiry. Its stored `EffectRequest` deliberately has no
destination incarnation. Queue DLQ and Workflow effects use this API. Raw
insertion is private; there is no public pre-encoded effect application surface.

`WorkflowAction::Effect` is applied through the same `EffectBatch` as other
transitions in the command. Batch creation proves its source `CellTarget`
derives `sys_meta.cell_id`; Queue and Workflow cannot emit under another Cell's
identity. Ordinary workflow commands reconstruct the next
Cell commit sequence from `sys_meta`; Tick constructs the batch from its exact
`CommandContext::sequence()` and shares it across every due transition. A
terminal decision may contain only effect actions, never a new timer or
activity. This preserves atomic state/effect publication and prevents ordinal
reuse when one Tick advances multiple runs.

`run_once` owns the complete one-item protocol: publish a source claim, validate
the lease after that receipt, deliver to the destination, Resolve an ambiguous
delivery, then publish either the exact acknowledgement or a bounded retry.
Destination business rejection is a delivered result, not a transport failure.
Known transient owner, capacity, deadline and transport failures publish a
retry. Non-transient authorization, registry and malformed-protocol failures
return `EffectSupervisorError::Runtime` and leave the lease to expire/reclaim.
An unknown source mutation returns `Pending` with its original resolution
evidence. A node scheduler may call `run_once` only after selecting and routing
an explicit due source Cell. `RepositoryCellScheduler` supplies that outer loop;
the reusable effect type deliberately does not own catalog or product routing.

The claim command's complete encoded output and lease command's complete input
must each remain within the registry's 1 MiB operation limit. Therefore the
lease transition wire value contains only `{effect_id, attempt, token,
expires_at_ms}`; it never repeats the destination operation. Fixed codec
overhead leaves 1,048,412 bytes for one claimed operation and 1,048,503 bytes
for one acknowledgement result. Boundary tests pin both exact sizes and reject
one byte beyond either limit.

Before constructing `PeerVerifier`, the HTTP receiver calls
`claimed_peer_session` to obtain only a structurally validated lookup key. That
value remains untrusted. It must load a current `NodeDirectory` advertisement,
match the mTLS leaf certificate digest and Ed25519 public key, then construct
the verifier with the advertised session and the server's selected release.
No API returns an authenticated principal or operation before that final verify.

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

The receiving server slice now maintains a repository UUID index, rechecks the
current configured issuer, subject membership, access level and exact registered
repository action, and builds a `LocalCellResolver` from the authoritative
application identity at startup. That resolver reloads catalog proof and control
and delegates to `CellRuntime::local_handle`, so it cannot serve a stale local
incarnation. The management listener now requires a CA-verified Ed25519 client
certificate for every connection. Its private forwarding route checks the exact
media type and byte limit, matches both the leaf SHA-256 and SPKI to the live
signed node advertisement, verifies the peer envelope, reauthorizes it and then
dispatches through that resolver. Health, readiness and metrics use the same
mTLS listener and the binary healthcheck supplies the configured identity and
CA. The outbound implementation reads authoritative control on every attempt;
local requests go to `CellHandle`, while remote requests go directly to the live
enrolled owner's advertised endpoint. It retains at most 1,024 TLS clients and
their connection pools, keyed by session, certificate digest and SPKI; it does
not cache ownership authority. Maximum two forwards; reject a third. A connect
failure, HTTP 429/503, or protocol `UNAVAILABLE/NOT_STARTED` reloads control at
most once within the original deadline. Timeout, response-stream failure,
malformed success, or HTTP 5xx after connection is an ambiguous mutation and is
never automatically retried. Never use the public Service for private forwarding
or send credentials to an endpoint copied from unverified control data.

`RepositoryCellRouter` is now constructed once by `serve` with the selected
application identity, storage layout, compiled registry, runtime, boot-session
signer, peer round trip, exact local owner and the node's already-created session
directory. Its repository route algorithm is fixed:

1. Accept one of the exact repository read/create/update actions and derive the
   `CellTarget` from the catalog UUID, never owner/name.
2. Require catalog proof, control and a published root. Absence is an unavailable
   repository application, never authority to initialize an empty database.
3. Return an exact local handle if the current
   runtime still owns it; construct a signed `CellClient::peer` when another
   live session owns it; fence a same-session/different-endpoint observation.
4. Serialize a cold path with one of 4,096 Cell-ID shards, then reload both
   records. For `Idle`, win owner CAS and restore the exact
   published root. For a same-session handle lost from memory, restore the exact
   root without changing authority. If the remote owner's canonical signed node
   advertisement is absent or expired, require the exact control to remain
   unchanged for 15 seconds, CAS a new owner/epoch, and only then restore. A
   malformed or foreign advertisement fails closed and cannot authorize takeover.
   Every attempt uses a unique SQLite path
   below `cells.data_dir/sessions/<session>/<cell>/`; failed files remain
   quarantined from later activation.
5. Return only a typed `CellClient`; HTTP handlers never receive a SQLite
   connection, replica, control token or peer endpoint.

`repository create` is the only online empty-bootstrap authority. It writes an
`empty_cell_pending` catalog state, provisions through the ready compiled
release, publishes the migration and repository UUID as the initial LTX root,
drains the temporary owner, then CASes the application state to `cell_ready`.
Retries resume the same catalog UUID. `repository adopt` and legacy records start
as `import_required`; only verified import completion may mark them ready. A
ready record with a missing control/root fails verification rather than
bootstrapping again. `serve` verifies this state and root for every repository
before binding either listener.

The router and authenticated issue/comment HTTP routes are integration-qualified
for explicit bootstrap, local reuse, clean idle release, source-independent
exact-root restoration and stable submission replay. The maintenance CLI imports
the legacy issue/comment object tree with bounded two-pass source verification,
immutable evidence, LTX publication and exact crash-resume checks. Remaining
collaboration-domain import and route cuts remain. A live remote owner is
forwarded to rather than stolen; absent/expired active owners use the bounded
ownership procedure above. Tests cover live mTLS forwarding, idle restoration,
the full 15-second no-progress observation and exact-root takeover.

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
change requires a protocol/digest version change. Tests now prove identical
digest/dedup/result behavior when one command first uses `CellClient::local` and
then reaches the same actor through `CellClient::peer`, strict verification,
authorization and dispatch. They also cover reordered Protobuf fields. No JSON
primitive transport or cross-language value conversion is needed.

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
