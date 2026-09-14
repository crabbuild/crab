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
execution/publication, pure-renewal retry, unknown-outcome classification and
drain. Catalog-driven `activate_restored` reserves active-Cell capacity before
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
Deadline/watchdog enforcement, later-root request resolution and the fenced
recovery supervisor remain to implement.

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
Effect lookup includes its 32-byte ID and destination incarnation. Internal
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
| Publish | Same owner identity and predecessor root; serving/recovering | prepared root, due summary, code/schema for migration; progress/revision + 1 |
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
10. Prepare immutable LTX/root objects. Compute due summary from the committed
    indexed tables, including outstanding lease deadlines.
11. CAS the control transition. Mark cuts published before emitting the stored
    reply and allowing verified local pruning.

Only durable success and stored business rejection carry receipts. Format,
authorization and admission failures carry NOT_STARTED/REJECTED without one.
Runtime sequence counts user and internal commands; LTX TXIDs are independent.

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

Add a ManagedDb read callback that installs the read-only authorizer, opens a
read transaction, materializes at most 1,000 rows/1 MiB, and restores the trusted
authorizer on every exit. SQL cursors never survive a network round trip.

## Workers and drain

Create `max(1, min(available_vcpu, 16))` worker shards. A bounded channel feeds
each shard; Cell ID hashes to one worker that owns its ManagedDb map. Every
command/query/transition is synchronous within that worker. Activities run on
Tokio outside SQLite and return through queued commands. Session movement
requires drain and exact-root reopen. The page-fault I/O driver runs independently
of SQL workers. No worker thread or permanent activity task is allocated per Cell.

Per-Cell mailbox ceiling: 64 requests and 8 MiB, further constrained by node
byte admission. A dispatched job retains permits until actual completion even
after waiter cancellation. SQL/page waits have a 5 s transaction wall deadline;
install a SQLite progress handler/interrupt and propagate deadlines into page I/O.
Check the deadline before and after native callbacks and before COMMIT. Native
Rust has no safe forced interruption: a watchdog fences admission and ownership,
but retains job permits/connection ownership until the job really ends. A
callback stuck outside SQLite requires supervisor termination of this process;
this can affect every Cell in it. Do not detach blocked work and recycle permits.
Native handlers must not perform blocking network I/O or unbounded computation.
The runtime fences before cleanup on unwind; panic=abort uses normal source-loss
recovery. Neither policy turns a panic into a business rejection.

The implemented `SqlWorkerPool` supplies the SQL ownership half of this contract. Construction
accepts one through sixteen workers and one through 10,000 active Cells. Each OS
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
execute only after the publisher returns from every preceding mutation. The next
supervision work must add wall deadlines and SQLite interruption and retain
fenced Cells for takeover/later-root resolution instead of only stopping admission.

Drain closes admission, resolves accepted publications, captures/publishes any
checkpoint cuts, closes SQLite, then releases ownership. Fenced sessions only
finish proven replies and cleanup. If shutdown budget expires, unresolved clients
receive unknown outcomes and the successor recovers origin state.
