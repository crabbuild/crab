# Cell runtime implementation

[Index](README.md). Formats are in [storage](storage.md); install the
[runtime migration](contracts/runtime.sql) before registering handlers.

Implementation status: `CellExecutor` implements mutation lifetime checks,
`sys_requests` dedup/conflict handling, application savepoint rollback, sequence/
logical-clock advancement, post-commit capture ownership, prepared-root binding
and exact-root confirmation. `CellPublisher` performs immutable preparation,
control CAS, exact-root reconciliation after a lost response, and safe renewal
refresh without SQL replay. `SqlWorkerPool` provides fixed worker ownership,
bounded shard queues, stable Cell routing, global activation admission, and
cancellation-safe completion of accepted SQL commands. `CellRuntime` now owns
the node-wide dispatcher, per-Cell and node byte admission, FIFO single-flight
execution/publication, pure-renewal retry, unknown-outcome classification,
per-Cell drain and node-wide terminal shutdown. Terminal shutdown atomically
rejects new admission, closes the dispatcher ingress after messages already
accepted by it, settles those messages through normal publication, then closes
every active SQLite handle and releases every owned control. A second shutdown
call fails closed with `RuntimeClosed`. Catalog-driven `activate_restored`
reserves active-Cell capacity before
remote reads, opens only control's exact immutable root through the sparse VFS on
the assigned SQL worker, verifies `sys_meta` and root position/sequence, reloads
authority after recovery, and accepts only the same control or pure renewals.
`bootstrap` reserves capacity first, exclusively creates the local file through
the replica's host, installs runtime and compiled application schema in one
worker-owned transaction, captures the initial LTX cut, publishes a sequence-zero
root and only then exposes a handle. Failed initialization rolls back; failed
publication closes the worker-owned database while leaving its local artifacts
quarantined. A reconciled exact root is accepted only while the resulting
control is the expected publication or its pure renewal; a later takeover fences
the old executor. `CellHandle::query` uses the same request/byte admission and
per-Cell FIFO as mutations, so it cannot observe a locally committed root before
publication. Its worker callback runs with SQLite `query_only`, enforces the
declared output bound, and leaves the Cell usable after a proven query error.
`CellHandle::resolve` is also FIFO ordered. It returns the exact stored outcome
only for the original digest, `ABSENT` only after earlier accepted work drained,
`UNKNOWN` when publication fenced, and `EXPIRED` for a structurally valid expired
identity. A successor that restores a later authoritative root resolves the
predecessor's ledger without replay. Fenced completion now closes and removes the
worker-owned executor, reloads authority, conditionally releases the newest
control still owned by the same epoch to `Idle`, and leaves a replacement owner
untouched. A later idle acquisition restores only that authoritative root. A single
dispatcher timer currently scans every 100 ms and admits at most 32 concurrent
owner renewals. Each idle owner renews every 3 s without a permanent per-Cell
task. Renewal uses the same strict control CAS, reconciles exact lost responses,
and fences the worker and mailbox if authority names another owner or cannot be
proven within the 10 s renewal attempt. Immutable-root preparation interleaves
renewal so retrying object I/O does not silently stop owner progress. SQL work
now uses one `rusqlite::InterruptHandle` captured from the worker-owned connection
at activation. A 5-second actor watchdog interrupts SQLite, atomically closes
Cell admission and returns `OUTCOME_UNKNOWN` for mutation or a deadline/unknown
read result. The accepted work and its byte/request permits remain owned by the
same task until the synchronous callback actually exits. The worker is then
fenced before any returned pending cut can prepare or publish. This also handles
native Rust that ignores the interrupt: the caller and admission stop at five
seconds, but Rust is never unsafely terminated. The actor now computes one
absolute deadline and carries it through the worker command. The worker scopes
that deadline around command, query and Resolve callbacks; nested scopes retain
the earliest value. A sparse VFS page fault applies the same instant to both the
asynchronous object-store `timeout_at` and its blocking `recv_timeout`, so queue
or provider delay cannot extend the SQLite wait to the former 30-second default.
The VFS retains a typed `CrabError::Deadline`; `CellExecutor` recovers that
source from SQLite, fences the connection, and maps it to the runtime deadline
contract. Mutations therefore return `OUTCOME_UNKNOWN`, queries return deadline,
and Resolve returns `UNKNOWN`, regardless of whether the inner VFS timer or the
outer actor watchdog observes the instant first. The 30-second default remains
only for standalone `crab-ltx` sparse calls without a caller scope.

`Registry::next_migration` now selects exactly one transition for a Cell whose
code/schema pair is either current or explicitly retained by the compiled Rust
module. Below `schema_max`, it returns the next digest-verified `N→N+1` SQL
migration and targets the current module code. At `schema_max`, a retained
predecessor code receives a same-schema code-only transition. `CellHandle::migrate`
reserves the bounded operation budget, atomically closes the old capability's
admission, and queues the transition behind work the actor already accepted.
The assigned SQL worker either executes trusted SQL and records
`sys_migrations`, or for code-only rollover updates system metadata without
application SQL. Both forms advance sequence/time in one managed transaction
and must produce a captured system cut. The executor retains that cut as
`PendingMigration`; no new capability is returned until immutable preparation
and the `Migrate` control CAS publish the exact root, schema and code together.
Success installs a fresh admission token and leaves all old `CellHandle` clones
permanently closed. Any transaction, capture, deadline or publication failure
fences the local executor and leaves authority at the last published root. The
server release scheduler performs bounded, resumable catalog-wide orchestration
and stores monotonic terminal progress for each migration operation.

Fencing is a two-phase boundary. Admission closes immediately, but the runtime
retains the accepted operation, worker slot and byte permits until synchronous
Rust has actually returned. It then uses the worker's recovery-only `discard`
command to close SQLite even when a pending cut exists; the database and LTX
files are not reused or deleted. Only after that close succeeds does
`CellPublisher::release_after_fence` reload control. It may adopt an ambiguous
publication or intervening pure renewals, but releases only when Cell,
incarnation, code, schema, owner and epoch still identify this executor. A new
owner or epoch is already the recovery authority and is never overwritten. The
closed admission keeps old handles fenced while the ordinary idle-acquisition
path opens a fresh local file from the retained authoritative root.

`CellRuntime::acquire_idle_restored` handles an `Idle` control immediately;
`CellRuntime::takeover_restored` handles a published `Recovering` or `Serving`
control owned by another session. Both validate the catalog and destination
session and reserve local active-Cell capacity before the ownership CAS. Active
takeover sleeps 15 seconds, reloads the exact control, and restarts the complete
observation interval after any change. Only an unchanged record may advance the
epoch and owner. A lost CAS response is adopted only if origin equals the exact
successor. Root download and sparse-writer activation begin after that proof, so
a losing contender does not hydrate database data. A CAS conflict reloads and
restarts observation; it never reuses elapsed time from a prior owner/progress.
Normal drain now closes the worker-owned SQLite handle and then transitions the
same observed control to `Idle` with no owner. Release retries provider-classified
transient failures across pure renewals and reconciles an exact lost response;
owner/root divergence fails fenced. Activation whose result receiver disappears
uses the same close-then-release path instead of leaving a ghost owner.

## Rust interfaces and ownership

`CellId`, `RootRef` and `Control` are specified in storage.md. Operations and
results map to the checked Protobuf descriptor. `RuntimeError` preserves
SQLite/LTX/storage sources and carries the protocol outcome classification.

```rust,ignore
pub struct AcceptedCommand {
    pub identity: CommandIdentity,
    pub digest: [u8; 32],
    pub operation: Operation,
    pub reply: tokio::sync::oneshot::Sender<MutationReply>,
}
pub struct PendingCommit {
    pub identity: CommandIdentity,
    pub predecessor: Control,
    pub commit_sequence: u64,
    pub encoded_reply: Vec<u8>,
    pub cuts: crab_ltx::CaptureBatch,
    pub prepared: Option<PreparedRoot>,
}
pub enum CellState {
    Recovering, Serving, Publishing(PendingCommit),
    Reconciling(PendingCommit), Draining, Fenced,
}
pub enum CommandIdentity {
    Application(MutationIdentity),
    Effect(EffectIdentity),
    Internal { request_id: [u8; 16], expires_at_ms: i64 },
}
pub struct VersionedControl {
    pub value: Control,
    pub token: crab_storage::ETag,
}
pub struct OwnedControl {
    pub value: Control,
    pub token: crab_storage::ETag,
    pub renewal_started: std::time::Instant,
}
impl CellAuthority {
    pub async fn load(&self, id: CellId) -> Result<Option<VersionedControl>>;
    pub async fn acquire(&self, observed: VersionedControl, session: SessionId)
        -> Result<OwnedControl>;
    pub async fn transition(&self, old: &OwnedControl, next: Control)
        -> Result<OwnedControl>;
}
impl CellRuntime {
    pub async fn shutdown(&self) -> Result<()>;
    pub fn is_shutting_down(&self) -> bool;
}
impl CellHandle {
    pub async fn submit(&self, command: AcceptedCommand) -> Result<()>;
    pub async fn read(&self, query: ReadRequest) -> Result<ReadReply>;
    pub async fn drain(&self) -> Result<()>;
}
```

`CellHandle` is a cloneable mailbox sender. Its actor owns current control,
state and pending cuts. A SQL worker owns ManagedDb; the actor holds an executor
key, never a connection shared between threads. Losing a reply waiter does not
drop the accepted command or release its resource reservations.

Application/internal outcomes use sys_requests; private Effect outcomes use sys_inbox.
The actor's lookup/store-outcome helpers dispatch on CommandIdentity, so a
7-day effect is not accidentally subjected to the application 24-hour request limit.
Effect lookup includes its 32-byte ID and owner-independent operation digest.
The destination incarnation is freshly described for each attempt, appears only
in the transient signed request and is fenced before destination admission; it
is not part of durable inbox identity. Internal
requests have a 60-second admission validity and are never accepted from the
public listener. The same pending-publication state machine supervises all three.

## Conditional transitions

One coordinator executes all transitions. Validate these predicates, encode
the replacement, then call `Store::update` with exactly the observed token.

| Operation | Preconditions | Replacement |
| --- | --- | --- |
| Create | Catalog entry exists; control absent | recovering, epoch/revision 1, new incarnation/session, null root |
| Acquire idle | idle; root present | recovering, epoch + 1, new session; preserve root/code/schema |
| Takeover | Same epoch/session/progress observed for 15 s | As acquire, preserving exact root |
| Renew | Same incarnation/epoch/session; before local deadline | progress/revision + 1; retain all other fields |
| Publish | Same owner identity and predecessor root; serving/recovering | prepared root and due summary; retain code/schema; progress/revision + 1 |
| Migrate | Same owner identity and predecessor root; serving/recovering; nonzero target code; either target schema is current + 1, or schema is unchanged and code changes | prepared root, due summary and target code/schema; progress/revision + 1 |
| Release | Drained SQL/mailbox; no pending cuts | idle, owner null; retain root/code/schema/epoch; revision + 1 |
| Tombstone | Maintenance authority; drained writer | tombstoned, owner null, epoch/revision + 1; root retained |

Every transition increments revision; all increments are checked. Overflow
fences the Cell. A token conflict reloads origin and revalidates every predicate.
Renewal goes through the coordinator so it cannot overwrite a newly published
root. During long preparation, renewals can replace the token; pending work
compares predecessor root/owner, then uses the latest validated renewal token.

Self-fence deadline is renewal request start + 10 s. Late success cannot revive
Serving. A contender restarts its observation whenever progress changes and
after its own process restarts. Node heartbeats supply routing hints only.
A late acknowledgement for an already published operation remains valid after
takeover; initiating another operation requires a current ownership session.

## Command transaction procedure

`actor::execute` is the sole mutation path, including internal timer/outbox work.
The SQL worker runs steps 3–9 synchronously. The steps below name the client
ledger; effect commands substitute sys_inbox and its longer retention:

1. Authorize the resolved namespace capability; validate role, identity, expiry
   and size. Reserve encoded bytes and a mailbox entry. Failure before acceptance is NOT_STARTED.
2. Wait for Serving with no pending publication. Recheck owner deadline and
   expiry. Transfer reservations to the supervised command.
3. Read the identity's outcome ledger. A matching ID/digest returns its stored result only after
   published-root proof. Digest mismatch returns REQUEST_ID_CONFLICT.
4. BEGIN IMMEDIATE. Compute `n = commit_sequence + 1`, bounded by i64::MAX.
   Set `now = max(system_utc_ms, sys_meta.logical_time_ms)`.
5. SAVEPOINT application. Invoke the handler. A business rejection rolls back
   to this savepoint and releases it, then becomes a bounded rejection result.
   Infrastructure error or SQL interruption rolls back everything. An unwinding
   native panic fences the activation; never reuse that worker's connection.
6. On success RELEASE savepoint. Encode and size-check result before COMMIT.
7. Insert ID/digest/outcome/result at n with identity-specific retention. Update sys_meta
   sequence/time. A recorded business rejection advances n without domain writes.
8. COMMIT. Any commit error fences the local writer and enters origin recovery;
   it cannot be treated as proven rollback.
9. Capture all cuts and transfer them into PendingCommit. Post-commit capture
   failure fences admission; no result escapes as durable success.
10. Before appending, inspect the exact published graph every eight appends or
    whenever it is within one segment of its admission ceiling. Prepare and CAS
    bounded eight-input level promotions; under segment or byte pressure prepare
    a full level-nine replacement. Each representation-only CAS preserves the
    application sequence, schema, due summary and database endpoint. Owner
    renewal continues while compaction reads, verifies and writes immutable data.
    If exact append preparation still reaches the segment/graph-byte limit, force
    one full replacement and retry that same captured batch once; unrelated limits
    and a one-segment graph fail without this retry.
11. Prepare immutable LTX/root objects against the resulting exact predecessor.
    Compute due summary from the committed indexed tables, including outstanding
    lease deadlines.
12. CAS the control transition. Confirm exact root equality on the SQL worker,
    reverify and delete only that pending batch's local paths, then emit the
    stored reply. A prune error leaves the result unknown and fences the local
    activation; it never changes the already published root.

Only durable success and stored business rejection carry receipts. Format,
authorization and admission failures carry NOT_STARTED/REJECTED without one.
Runtime sequence counts user and internal commands; LTX TXIDs are independent.

## Schema migration transaction and capability replacement

Migration is a terminal operation on one `CellHandle`, not an application
command. The caller must obtain `MigrationPlan` from the frozen `Registry`; raw
SQL, target schema and code digests are not accepted from HTTP or application
handlers.

1. Select `Registry::next_migration(namespace, handle.code(), handle.schema())`.
   Registry construction has already verified the current code, every explicitly
   retained predecessor code/schema range, the contiguous schema range and each
   SQL migration's exact BLAKE3 digest and bounded nonempty body. The selected
   plan fixes both source and target code/schema.
2. Reserve one request plus the plan's bounded operation budget. Close the old
   admission gate and enqueue the migration. Work already accepted by the actor
   remains ahead of it; later work through any old handle is rejected.
3. On the fixed SQL worker, verify `sys_meta` still names the exact Cell,
   incarnation and source schema. For an SQL step, reject an existing
   target-version migration with another digest. For a code-only step, require
   unchanged schema, different source/target code and no SQL or migration digest.
4. In one managed transaction, an SQL step executes the trusted migration,
   inserts the target `sys_migrations` row and updates schema. A code-only step
   executes no application SQL and creates no migration-ledger row. Both increment
   `commit_sequence`, advance logical time, update `sys_meta`, and compute the
   durable scheduler deadline before commit.
5. Capture a nonempty LTX cut. Capture or ambiguous SQLite failure fences the
   local connection; an unpublished local schema must never serve traffic.
6. Prepare immutable LTX dependencies against the published predecessor using
   the target schema, bind that exact root to `PendingMigration`, then CAS a
   `Transition::Migrate` successor. Ordinary `Publish` rejects code/schema drift.
7. Confirm the exact published root on the SQL worker, replace actor admission
   with a new capability carrying target code/schema, and only then return
   `MigratedCell`. A subsequent owner restores that exact root and verifies
   `sys_meta` before admission.

The migration path has the same five-second absolute SQL/sparse-I/O deadline and
the same lost-CAS reconciliation rules as commands. It does not run arbitrary
customer SQL and cannot be retried on a locally modified connection after an
unknown publication result. Retained code is an explicit reviewed compatibility
claim in the statically linked module descriptor; it is not downloaded code or
a dynamic dispatch target.

## Reconciliation and Resolve

Retain PendingCommit and its reservations. Retry origin observations after
100/200/400 ms then at 1 s intervals. Caller timeout emits OUTCOME_UNKNOWN with
request ID, never tentative output; supervision continues until fenced or proven.

1. Origin root equals prepared root: publication succeeded.
2. A later root exists: inspect its request record through its current owner or
   verified read-only recovery. Exact ID/digest/result proves inclusion; sequence
   or TXID comparison alone does not.
3. Root remains predecessor and owner is valid: retry the same prepared transition
   with a newly validated token, without rerunning SQL.
4. Owner changed: finish supervised jobs, fence this activation and ask successor
   to resolve. Never append old local WAL to a successor's database.

Resolve returns ABSENT only after the owner drains accepted work for that ID,
or a newly fenced owner restores the published root and checks it. If an old
publisher may still commit, absence at a historical root is UNKNOWN. Expired
identities return EXPIRED; a client must not silently issue them as new work.

## Reads

V1 current reads run on the owner behind pending publication. Observe origin
control, compare its root to the local published endpoint, then materialize
the bounded query on the SQL worker without a concurrent local writer. If the
root/owner changed, refresh routing and retry within the request deadline.
The successful origin observation is the read's linearization point.

Minimum receipts must match Cell/incarnation; wait until authoritative sequence
reaches the requested sequence. Restore changes incarnation and rejects old
receipts. Return the observed receipt with every read result. Historical snapshot
streaming and replica routing are outside the v1 API.

The worker's existing ManagedDb read callback supplies SQLite `query_only` and
the five-second interrupt boundary. Inside it, `sql_query_batch` now installs the
read-only authorizer, materializes at most 1,000 rows/1 MiB and clears that scoped
authorizer on every exit before trusted runtime SQL can resume. The typed query
context and `SqlCell` make this the only application query entry point. SQL
cursors never survive the synchronous callback or a network round trip.

## Workers and drain

Create `max(1, min(available_vcpu, 16))` worker shards. A bounded channel feeds
each shard; Cell ID hashes to one worker that owns its ManagedDb map. Every
command/query/transition is synchronous within that worker. Asynchronous
activities run on Tokio outside SQLite; registered blocking activities run on a
separate fixed OS-thread pool. Both return through queued commands. Session movement
requires drain and exact-root reopen. The page-fault I/O driver runs independently
of SQL workers. No worker thread or permanent activity task is allocated per Cell.

Per-Cell mailbox ceiling: 64 requests and 8 MiB, further constrained by node
byte admission. A dispatched job retains permits until actual completion even
after waiter cancellation. SQL/page waits have a 5 s transaction wall deadline.
The actor installs the SQLite interrupt and passes the same absolute deadline
through the worker into `crab-ltx`; the sparse VFS bounds both its blocking wait
and asynchronous provider read by it. Check the deadline before and after native
callbacks and before COMMIT. Native
Rust has no safe forced interruption: a watchdog fences admission and ownership,
but retains job permits/connection ownership until the job really ends. A
callback stuck outside SQLite requires supervisor termination of this process;
this can affect every Cell in it. Do not detach blocked work and recycle permits.
Native handlers must not perform blocking network I/O or unbounded computation.
The fixed worker catches unwind panics around bootstrap, command, destination
effect and query callbacks. It fences the affected Cell before returning, while
the same worker thread continues serving unrelated Cells. Actor recovery closes
the fenced connection, releases only the still-owned control to Idle and restores
from the authoritative root; a mutation/effect caller receives unknown outcome.
With `panic=abort`, process restart follows the normal source-loss recovery path.
Neither policy turns a panic into a business rejection.

`BlockingActivityPool` contains `max(1, min(available_vcpu, 16))` named OS
threads and an equally bounded submission channel. Its non-waiting reservation
is acquired together with the node activity, byte and per-Cell reservations
before the scheduler issues the durable claim. The reservation moves into the
submitted job, not the awaiting Tokio future. Aborting that future therefore
signals cooperative cancellation but cannot make the slot reusable while an
uncancellable Rust callback is still running. Each callback is wrapped in
`catch_unwind`; a panic fails that attempt and the same worker accepts later
jobs. Pool shutdown closes admission, drains the channel and joins every thread
on a joinable Tokio blocking task. An arbitrary callback that never returns
cannot be safely killed; the server's 110-second shutdown deadline then forces
process termination and origin-based lease recovery.

The implemented `SqlWorkerPool` supplies the SQL ownership half of this contract. Construction
accepts one through sixteen workers and one through 10,000 active Cells. Server
composition derives the active limit from its 35% page-cache pool, three 64 KiB
caches per managed database and the process's currently available file
descriptors, then retains 10,000 only as an absolute safety ceiling. Each OS
thread owns a `HashMap<CellId, CellExecutor>` and consumes a Tokio MPSC channel
with capacity 256 through `blocking_recv`; no Tokio runtime is created on the
worker. The first eight Cell-ID bytes select the worker, so activation and every
later command use the same connection owner. A node-global atomic reservation is
acquired before activation enters a queue and is released only when activation
fails, the fully drained Cell is deactivated, or the pool closes. Dropping an
awaiting task can discard only its oneshot receiver: the queued command, handler,
commit, captured cuts and pending publication remain worker-owned.

`CellRuntime` implements the other half with one node-wide Tokio dispatcher and
no permanent task per Cell. Each activated `CellHandle` shares semaphores for 64
requests and 8 MiB. Submission declares encoded operation and maximum result
sizes, each at most 1 MiB; their checked sum is reserved at both Cell and node
scope before the command enters the 1,024-message ingress channel. The declared
result limit is also enforced inside the SQL transaction, so a caller cannot
under-reserve output. Permits live in the queued command through SQL, immutable
preparation, authority CAS, exact-root confirmation and dropped reply delivery.

The dispatcher keeps a FIFO per Cell and moves its sole `CellPublisher` into at
most one transient task. Other Cells can prepare/publish concurrently while the
stable SQL workers remain synchronous. Preparation and ambiguous authority
observations retry provider-classified transient failures after 100/200/400 ms,
then every 1 s. A stale CAS caused only by renewal rebuilds the successor from
the latest token without rerunning SQL. Exact-root observation confirms success;
owner/root divergence or a permanent post-commit failure fences the worker and
returns `OutcomeUnknown { request_id, operation_digest, source }`. A proven
handler rollback consults worker state and leaves the Cell usable.

The dispatcher uses `pending`, `bind_prepared` and `confirm_published`; direct
access to worker-owned executors is impossible. Reads enter the same FIFO and
execute only after the publisher returns from every preceding mutation. Fenced
task completion automatically enters the discard-and-conditional-release path;
the same task participates in terminal shutdown instead of becoming detached.

Per-Cell drain closes admission, resolves accepted publications, closes SQLite,
then releases ownership. Fenced cleanup instead discards local pending state,
closes SQLite and reloads authority before a conditional release. Node shutdown first closes node admission and runtime
ingress, drains every message accepted before the shutdown marker, applies the
same close-before-release ordering to all Cells, and returns only after every
deactivation task has settled. It then closes the shared `SqlWorkerPool` and
joins every fixed worker thread on Tokio's blocking pool; all pool clones remain
closed. Direct pool shutdown rejects active Cells, preventing SQLite close from
bypassing control release. Fenced sessions only
finish proven replies and cleanup. If shutdown budget expires, unresolved clients
receive unknown outcomes and the successor recovers origin state.
