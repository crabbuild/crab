# Primitive handlers and SQL procedures

[Index](README.md). Install runtime.sql in all Cells and exactly one of kv.sql,
queue.sql or workflow.sql in the corresponding primitive role. User SQL Cells
install application migrations instead. No namespace column is necessary:
Cell identity already selects exactly one namespace and shard.

## Handler boundary and SQL authorization

```rust,ignore
pub trait CommandHandler {
    fn execute(
        &self, tx: &mut CommandContext<'_>, op: &Operation,
    ) -> Result<MutationResult, CommandError>;
}
pub struct CommandContext<'a> {
    tx: &'a rusqlite::Transaction<'a>,
    cell: CellId,
    incarnation: [u8; 16],
    sequence: u64,
    now_ms: i64,
    next_effect: u32,
}
```

Only the runtime constructs CommandContext. Primitive modules have trusted SQL
access; application SQL uses a scoped SQLite authorizer. Reject ATTACH,
DETACH, transaction/savepoint commands, PRAGMA, extension loading and access to
sys_* or primitive-owned tables. Match authorizer operations and resolved object
names, not a regex over SQL text. Application migrations run only during trusted
bootstrap/maintenance; request batches cannot create, alter, analyze, reindex or
drop schema objects. Compiled migration validation must reject table/view/
trigger/index names starting sys_ or the reserved kv_, queue_ and workflow_
prefixes. Disable
triggers/views that indirectly reach protected tables by enforcing authorization
for their underlying accesses too.

`SqlBatch` executes 1..128 prepared statements in one command transaction,
binding every parameter with the typed SqlValue codec. Return one ResultSet per
statement. Limit aggregate returned rows to 1,000 and encoded bytes to 1 MiB;
overflow rolls back before runtime result persistence. Read statements run under
the read authorizer and cannot include RETURNING from a mutating statement. Each
`SqlStatement.sql` contains exactly one statement without an unquoted semicolon;
semicolons inside quoted values, identifiers or comments remain ordinary bytes.

Implementation status: `crab-cell-runtime::sql` exposes `SqlValue`,
`SqlStatement`, `SqlBatch` and materialized `SqlResultSet` plus separate command
and read-only entry points. It validates 1..128 statements and a 1 MiB typed
input before preparation, requires an exact parameter count, rejects non-finite
real values, mutating `RETURNING`, more than 1,000 aggregate rows and more than
1 MiB of materialized columns/values. A `rusqlite` authorizer is installed for
the complete prepare/execute interval and removed by a guard on every return.
It denies non-main databases, runtime/primitive names case-insensitively,
protected trigger/view accessors, DDL, PRAGMA, transactions/savepoints,
ATTACH/DETACH, virtual tables, ANALYZE/REINDEX, unknown operations and
`load_extension`. `tests/sql.rs` covers typed mutation/read order, direct and
indirect protected access, read-only enforcement, separator/parameter/input/
row bounds and authorizer cleanup. `SqlModule` supplies compile-time batch/query
IDs and `register_sql` binds their canonical bounded codecs. `SqlCell` validates
the registered SQL namespace and explicit target before routing, publishes
write batches through the normal actor/LTX path and returns receipted read-only
results. Integration coverage also proves that a published batch survives
drain, source-local database loss and exact-root restoration on a new owner.

## Request dedup and outbox

Implementation status: `crab-cell-runtime::effects` implements the reusable
source `sys_effects` and destination `sys_inbox` mechanics. It derives effect
IDs from source Cell/incarnation/sequence/ordinal, binds the destination and
exact operation bytes in a stable digest, enforces 128 intentions/1 MiB per
command, claims at most 32/1 MiB, and revalidates attempt/token/deadline only
after publication. Delivery results use a nested savepoint so a business
rejection rolls back destination writes while its inbox result remains durable.
Lost responses retry the same effect bytes; the inbox returns the stored result
without invoking the handler. Source retry/extension/delivery and both retention
cleanups are bounded. `CellHandle::deliver_effect` now applies destination work
through the fixed SQL worker and ordinary LTX/control publication path;
`resolve_effect` returns committed, absent, expired or unknown state from the
published inbox, including after exact-root restoration. Accepted delivery
continues after caller cancellation. The signed peer transport strictly validates
and dispatches generic compiled Cell-command DeliverEffect/ResolveEffect messages;
`EffectPeerClient` validates a canonical stored request against its published
claim before signing it. Workflow/Queue effect insertion and codecs, source
claim/ack orchestration and the node delivery supervisor remain.

Runtime request outcome values: 1=success, 2=business rejection. Its result is
the encoded MutationResult or Error, not a transport header. The stored sequence
constructs a receipt on replay; root digests are not embedded in request rows.
Expired requests are rejected before dedup lookup even if a row remains.

Effect state: 0=ready, 1=leased, 2=delivered, 3=failed. Generate
`effect_id = BLAKE3("crab.effect.v1\0" || cell || incarnation || u64(sequence)
|| u32(effect_ordinal))`. Increment ordinal for each intention in a command.
Destination and operation bytes are immutable once inserted. Enforce at most
128 effects and 1 MiB aggregate effect bytes per command.

Persist the exact encoded EffectRequest from peer.proto, including resolved
destination incarnation, in sys_effects.operation. Resolve destination metadata
from compiled namespace capabilities before the source transaction; a stale incarnation causes
delivery conflict, never silent redirection into restored data. Delivery validates
effect_id against source Cell/incarnation/sequence/ordinal. Its digest uses the
canonical typed codec with domain `crab.effect-op.v1\0`; include destination
Cell/incarnation, EffectIdentity and selected operation. Transport headers are
excluded, and the digest never changes between delivery attempts.

If a command requests a destination missing from its resolved capability cache, return
UnresolvedTarget before inserting an effect. Roll back the whole transaction,
resolve the target outside the SQL worker, reconstruct the typed command context, then
retry at most twice. This is allowed only after proven rollback; a commit or
capture ambiguity uses reconciliation instead of SQL replay.

The source dispatcher uses the same claim/lease procedure as Queue, with a
30 s lease and 7-day maximum delivery horizon. It sends a private authenticated
DeliverEffect envelope containing effect_id, destination, operation digest,
expiry and operation bytes. This is a peer-only operation; public callers
cannot invoke arbitrary trusted primitive SQL.

At the target, BEGIN IMMEDIATE, reject expired effect, then lookup sys_inbox.
Same ID/digest returns stored result; mismatch rejects. Otherwise apply the
primitive operation, insert inbox/result and commit through normal publication.
Retain inbox through effect expiry+7 days. After target publication, source
publishes state=delivered. Every retry preserves ID and payload. Retry delay is
`min(60s, 100ms * 2^min(attempt, 10))`; expire to failed rather than deleting
an undelivered intention. Redrive allocates a new explicit effect identity.

If the next retry would reach/past expiry, mark failed and clear the lease
instead of storing a due_at beyond expires_at. Private Resolve uses the same
drain/fence-before-ABSENT rule as application Resolve, with sys_inbox as evidence.

## KV procedures

Implementation status: `crab-cell-runtime::kv` embeds and verifies the normative
KV migration. `kv_atomic` validates at most 128 items/1 MiB, checks every live
precondition before writing, rejects duplicate mutation keys, derives the exact
28-byte incarnation/sequence/ordinal version and preserves mutation order.
`kv_get` and `kv_list` apply logical expiry; list uses binary prefix bounds and
caps both count and materialized page bytes. `kv_cleanup_expired` deletes at most
128 rows. Integration coverage executes a KV mutation through typed
`KvNamespace`, publishes its LTX root, validates a minimum-receipt point/list
read and durable precondition rejection, releases the owner, restores on a new
owner and reads the same value. `KvModule` supplies compile-time operation IDs;
`register_kv` binds stable bounded codecs for atomic, get and list without
exposing handler selection. Scope determines the fixed shard before routing,
and query TTL uses owner-sampled logical time. Scheduler invocation of cleanup
remains to implement.

V1 KV is scoped. `Target.partition` must equal u32(hash(scope) % shard_count);
Rust recomputes and rejects mismatches. Keys are 1..1024 bytes, scope <=1024,
values <=65536 bytes. Reject duplicate mutation keys in an atomic operation.
At least one check or mutation is required; a check-only command still gets a
durable dedup outcome.

`kv_atomic` runs all checks before writes in the same transaction. For each key:

```sql
SELECT version, value, expires_at_ms FROM kv_entries
WHERE scope = :scope AND key = :key
  AND (expires_at_ms IS NULL OR expires_at_ms > :now);
```

Absent check requires no row; version check requires an exact 28-byte match.
Failure returns PRECONDITION_FAILED, and runtime records that rejection without
KV changes. Version for mutation ordinal i is
`incarnation[16] || u64(sequence) || u32(i)`; delete/recreate cannot reuse it.
Expiry on put must be greater than sampled now, or omitted; it is an absolute
time stored in the request, so retries do not extend TTL.

```sql
INSERT INTO kv_entries(scope, key, version, value, expires_at_ms)
VALUES (:scope, :key, :version, :value, :expiry)
ON CONFLICT(scope, key) DO UPDATE SET
  version = excluded.version, value = excluded.value,
  expires_at_ms = excluded.expires_at_ms;
```

Delete removes the row and returns `{key, deleted=true}`; the version field is
empty for deletion. Put returns its new token. Results preserve mutation order.
An absent unconditional delete succeeds; checked delete uses the same initial
logical-expiry test as checked put.

Get returns KvPage with zero or one entry. List is scope-local and reads live
keys in binary ascending order after after_key, matching a byte prefix. Compute
the exclusive prefix successor by incrementing the last non-0xff byte; all-0xff
or empty prefix has no upper bound. Query LIMIT limit+1 with limit in 1..1000;
return at most limit and the last emitted key. Each page is a current read with
its own receipt: concurrent writes can change later pages. No cross-page snapshot
or global hashed-namespace listing is promised.

TTL cleanup selects at most 128 expired keys via kv_expiry and deletes them in
an internal published command. It affects physical storage only; foreground
operations already treat those rows as absent.

## Queue send and claim

Implementation status: `crab-cell-runtime::queue` embeds and verifies the
normative Queue migration. `queue_send` binds a stable producer ID to the exact
payload and schedule digest and derives `message_id` as the first 16 bytes of
`BLAKE3("crab.queue-message.v1\0" || namespace || producer_id)`. `queue_claim`
reclaims at most 128 expired leases, orders ready rows, caps each claim at 32
messages/512 KiB and obtains 16-byte tokens through an injectable source whose
production implementation uses the process cryptographic RNG. The runtime caller
publishes the encoded claim before `queue_validate_claim` permits emission.
`queue_apply_lease` implements conditional ack/retry/extend and attempt/expiry
death; cleanup removes at most 128 dedup and 128 terminal rows. Integration
coverage sends and claims through typed `QueueNamespace`, validates only after
publication, then restores another owner, validates the same lease and
acknowledges it through the typed API. `QueueModule` supplies stable send/claim/
lease/query IDs and its fixed namespace; `register_queue` binds bounded codecs.
The capability derives producer shards, requires an explicit consumer shard and
loads the immutable shard count from the compiled registry. Producer conflicts
and lost leases are durable typed rejections. DLQ effect insertion and scheduler
polling remain to implement.

Queue state: 0=ready, 1=leased, 2=acked, 3=dead. Send hashes producer_id to the
shard, validates payload <=256 KiB and available_at within now..now+7 days.
Message ID is first 16 bytes of BLAKE3(`crab.queue-message.v1\0` || namespace ||
producer_id), with a stored payload digest to detect identity conflict. Existing queue_dedup returns the
same message identity if payload and scheduling attributes match. Its digest
therefore includes payload and available_at. Retain it for 30 days.

New send inserts ready, attempt=0, expiry=now+30 days, null token/deadline.
Reply contains one QueueMessage with ID, empty token/payload, attempt=0 and
lease_until=0. Send does not grant a consumer lease.

Claim accepts limit 1..32 and lease in 5..300 seconds. Inside one transaction:

1. Reclaim at most 128 expired leases using queue_leases. Rows at attempt 20 or
   message expiry become dead; remaining rows become ready with due_at=now.
   Clear token/deadline for every reclaimed row.
2. Select ready, unexpired rows with attempts <20 ordered by due_at,message_id.
   Accumulate at most limit and 512 KiB payload; stop before exceeding either.
3. Allocate an unpredictable 16-byte token per row. Set state=leased,
   attempt=attempt+1, lease_until=min(now+lease_ms, expires_at).
4. Runtime persists the claim result and publishes before emitting any task.

Representative per-row conditional update after selection:

```sql
UPDATE queue_messages
SET state = 1, attempt = attempt + 1,
    token = :token, lease_until_ms = :deadline
WHERE message_id = :id AND state = 0
  AND due_at_ms <= :now AND expires_at_ms > :now AND attempt < 20;
```

Require one changed row. Claim holds the Cell writer so failure implies an
implementation invariant violation, not a reason to return a partial batch.
An empty claim returns an empty list as a published command. Native supervisor polling backs
off 100 ms to 1 s on empties; v1 has no server-side long-poll stream.

Before emitting a claim, check every token against current published state and
require >=1 s lease remaining. A replay of an old request with expired tokens
returns LEASE_LOST, preserving its receipt. Persisted claim success is not a
promise that its transient lease remains valid forever. Clients issue a new
claim identity; they never process payloads from an expired cached reply.

Apply this emission check to normal replies, dedup replays and Resolve replies,
for both queues and activities. The error does not mean the historical claim
rolled back: its receipt remains present, but its lease is no longer usable.

## Queue ack, retry, extension and dead letters

Every lease mutation first checks state=leased, matching token and deadline>now.
Use the same predicates in its UPDATE and require exactly one changed row.
Ack sets state=acked and clears token/deadline. Retry clears lease and sets
ready/due_at=now+delay (delay <=1h), or dead if attempt/expiry limit is reached.
Extend sets deadline=min(now+extension, expires_at), with extension in 5..300 s;
it never shortens an existing valid deadline. Publish before acknowledging.

Duplicate lease mutations replay by runtime request ID. A different request ID
with a stale token returns LEASE_LOST; it cannot ack a replacement delivery.
A native consumer may execute twice after lease loss, so destination effects
use the stable message ID as an idempotency key.

If a namespace declares a dead-letter target, transition to dead and insert a
sys_effects row atomically using the normal source sequence/ordinal effect ID.
Only the first transition to dead creates this intention; repeated maintenance
cannot create a second one. Its destination QueueSend producer ID includes the
source Cell/incarnation/message ID, scoped by the target namespace. Retain dead payload until delivery completes
or an operator resolves the failed effect. GC cannot delete it at message expiry
while that effect remains pending. Without DLQ, retain dead rows until expiry
for inspection. Namespace graph validation rejects DLQ cycles.

## Workflow transitions and activities

Run status: 0=running, 1=completed, 2=failed, 3=cancelled. Activity state:
0=ready, 1=leased, 2=completed, 3=failed, 4=cancelled. Timer state: 0=pending,
1=fired, 2=cancelled. Full tables and constraints are in workflow.sql.

Implementation status: `crab-cell-runtime::workflow` embeds and byte-verifies
the normative Workflow migration. It implements atomic start, signal,
cancellation and timer firing through a pinned `WorkflowDefinition`; derives
run, event and action IDs exactly; validates state/result/action bounds before
writes; enforces the 128 outstanding-task and 100K Cell-event ceilings; and
cancels every pending local activity/timer on a terminal decision. Signal and
timer identities are idempotent and payload conflicts fail closed. Integration
coverage publishes a run, releases its owner, restores the exact root under a
new owner, claims and validates an activity only after publication, completes
it, republishes and observes the resulting event. Activity claim scans filter
the worker's exact type/definition pairs in SQL, lease tokens and attempts bind
all extension/completion predicates, retry completions are idempotent, and old
attempts lose authority after a new claim. Terminal cleanup deletes children
before at most 128 retained parents. `ActivitySupervisor::run_once` now claims
through the typed actor command, validates the published lease at its receipt,
dispatches the exact compiled `(module, definition, activity type)` Rust future
outside SQLite, durably extends the lease every third of its interval, and
publishes completion or retry. Stable external idempotency ignores attempt and
lease tokens; completion identity is deterministic per attempt. Pending command
outcomes retain their mutation evidence, and cancellation is signalled if the
cycle is dropped or loses its lease. Integration coverage holds an activity
past its first heartbeat, completes its state-machine transition, then restores
and reads that terminal result from the exact LTX root. Effect actions,
catalog-driven shard polling and bounded concurrent orchestration remain to
implement; the maintenance Tick now dispatches timers across retained definitions.

`WorkflowModule` binds one namespace, one current definition, a bounded static
inventory of retained definitions, and fixed start/signal/cancel/state operation
IDs. New runs use `CURRENT_DEFINITION`; signal and activity completion first
read the run's persisted digest and dispatch the matching entry from
`DEFINITIONS`. `register_workflow` installs every transition function together
with the typed codecs; activity registration expands the exact definition/type
matrix. Registry freeze rejects any descriptor whose digest or activity
inventory differs from those bindings, and startup rejects a current definition
absent from the retained inventory.

`WorkflowNamespace` hashes only the workflow ID through the registry-owned
shard count, binds start identity to `MutationIdentity.request_id`, maps every
non-applied outcome to a durable typed rejection, and exposes bounded
minimum-receipt `WorkflowRun` reads. Integration coverage proves start, signal,
duplicate replay, identity-conflict rejection, exact-root restore, state and
cancellation. Activity execution remains a separate node-owned capability;
application code never receives a lease or raw SQLite connection through the
Workflow handle.

Timer events passed to the definition are `timer\0 || timer_id`. Activity
completion events are `activity\0 || failed:u8 || activity_id ||
result_length:u32be || result`. These encodings are persisted event bytes; change
them only with a new compiled definition digest and an explicit migration.

Start requires absent workflow_id; an existing run returns PRECONDITION_FAILED
unless this is a replay of its original request. Allocate run_id from the
request identity and namespace via domain-separated BLAKE3 truncated to 16 bytes.
Pin the registered definition digest in workflow_runs. Insert event sequence 1
with event_id=BLAKE3(run_id || request_id), execute the definition's start
transition, then persist state and resulting activities/timers. Publication
makes the run and all scheduling intentions visible together.

Definition callback is `transition(state_bytes, event_bytes, Context) -> Decision`.
Context contains run_id, current event sequence and sampled now; no network,
ambient clock, randomness or SQL in its interface. These are programming
restrictions on trusted Rust, not enforced sandbox isolation; test deterministic
re-execution and review handlers for hidden I/O or mutable global state.
Decision contains next status/state/result and
at most 128 activity/timer/effect actions totalling <=1 MiB. Activity and timer
IDs are allocated from run_id, event sequence and action ordinal. The Rust transition
can reference allocated IDs from persisted state on later transitions.

Use native Decision/Action values in the SQL worker; they are not peer RPCs.
`context.action_id(i)` returns first16(BLAKE3(`crab.action.v1\0` || run_id ||
u64(event_sequence) || u32(i))); i must match the action's output ordinal. This
lets the callback store IDs in next_state before returning. Terminal decisions
may emit effects but cannot schedule new timers/activities; cancel outstanding
tasks in the same transaction. Enforce at most 128 outstanding tasks per run
so terminal cancellation has bounded work.

Signal requires matching run_id and running status. Its event ID is
BLAKE3(run_id || signal_id); same ID with a different event_digest conflicts.
Inside one transaction, append event, invoke transition, apply decision and
increment event_sequence. Reject unknown definition digest before opening a
writer transaction. Cancellation appends a cancellation event, sets terminal
status, marks outstanding activities/timers cancelled and clears leases. It
cannot reverse external side effects already executed.

ActivityClaim uses the queue claim algorithm over workflow_activities, joined
with running workflow_runs. Filter activity_type and definitions the worker
advertises as supported. Return run/activity/type/input/definition/attempt/token.
Keep claim payload <=512 KiB. Workers heartbeat through ActivityLease EXTEND;
extension predicates match current run state, attempt, token and deadline.

Complete/fail first checks a stored completion_token/digest: identical duplicate
returns the previous applied result; different bytes conflict. Otherwise require
running run, leased activity, matching attempt/token and unexpired deadline.
Persist completion_token/digest before clearing the active lease. Completion
appends an event and executes the transition in the same transaction. Retryable
failure below attempt/lifetime limit schedules another delivery with exponential
delay capped at 60 s; terminal failure emits ActivityFailed to the definition.
New claims clear old completion tokens; earlier attempts then fail LEASE_LOST.

External idempotency is stable `(run_id, activity_id)`, never attempt/token.
Activity completion payload <=256 KiB. Workflow state/results <=1 MiB. Retain
terminal run/event rows for 30 days, then delete child rows before the parent in
one bounded cleanup procedure. Running histories cap at 100K events/Cell; stop
new transitions with RESOURCE_EXHAUSTED instead of silently pruning required
history. Capacity remediation is an operator action, not arbitrary stack replay.

## Scheduler commands

Implementation status: `crab-cell-runtime::scheduler_next_due_ms` now computes
the summary inside bootstrap and every committed command transaction. It covers
request/inbox retention, source effects, installed KV/Queue/Workflow schemas,
ready work, live lease deadlines, expirations, pending timers and terminal
retention. Overdue values clamp to the command's logical time. `CellHandle`
does not accept a caller-provided summary; the exact computed value follows the
pending LTX cut into the same control publication. `scheduler_tick` and
`MaintenanceTickCommand` now enforce one 128-item budget across request/inbox
cleanup, effect/queue expiry and lease recovery, KV expiry, Workflow retention,
terminal activity events and due timers. The command rejects a stale scanned
root position before doing maintenance, and its resulting summary publishes
through the ordinary actor/LTX/control path. `CatalogShardScan` pins one head
revision and verifies one immutable page per call; `DueCellScan` inspects at
most 32 controls per step, and `preferred_scanner` implements deterministic
rendezvous assignment. Node advertisements and fallback, remote/idle routing
and Tick retry supervision remain to implement. Active-local routing can already
recover a capability from the dispatcher only when session, incarnation, code
and schema match the scanned control and the Cell is not fenced or draining.

After every commit, compute minimum outstanding due time using indexed minima
for ready effects, leased-effect deadlines, KV expirations, ready queue rows,
queue lease deadlines, ready activities, activity deadlines and pending timers.
Clamp already-due values to sampled now; null means no scheduled work. Terminal
retention cleanup contributes its expiry too. Persist this summary in the same
control CAS as the database root.

Include sys_requests_expiry and sys_inbox_expiry in that minimum. Each Tick
deletes at most 128 eligible ledger rows, with strict expiry checks; it cannot
shorten either advertised dedup horizon.

Assign catalog shards to nodes using rendezvous order; only the preferred live
scanner polls normally, and another scans if its advertisement stops progressing
for 15 s. Scanner ownership is advisory; duplicate scans are safe. Load catalog
pages by digest and inspect each Cell control with bounded I/O. Complete a pass
within 5 s at admitted load; expose scan lag and reject further provisioning if
that budget cannot be sustained.

For a due Cell, route/acquire ownership and submit Tick containing a 16-byte
internal request ID and the commit sequence of the root that carried the due
summary. Tick returns `Stale` if that root position has already advanced;
otherwise it rechecks row state, processes at most 128 due items, then
republishes a new summary. A timer tick
appends event ID=BLAKE3(run_id || timer_id || "fired"), changes pending→fired,
and applies the transition atomically. Deadline races are resolved by the same
serialized command loop. Losing all notifications cannot strand work because
the catalog scanner revisits published summaries.

Wall-clock time is sampled once per command and clamped against sys_meta.
Ownership uses monotonic time. Nodes detect wall/monotonic divergence >5 s
between periodic samples, fence admission and require clock correction/restart.
Scheduler liveness depends on qualified host clocks; duplicate external activity
execution remains possible even with correct timing and is covered by idempotency.
