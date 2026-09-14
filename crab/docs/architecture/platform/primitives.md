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
names, not a regex over SQL text. Reject user table/view/trigger/index names
starting sys_. Disable triggers/views that indirectly reach protected tables
by enforcing authorization for their underlying accesses too.

`SqlBatch` executes 1..128 prepared statements in one command transaction,
binding every parameter with the typed SqlValue codec. Return one ResultSet per
statement. Limit aggregate returned rows to 1,000 and encoded bytes to 1 MiB;
overflow rolls back before runtime result persistence. Read statements run under
the read authorizer and cannot include RETURNING from a mutating statement.

## Request dedup and outbox

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

Queue state: 0=ready, 1=leased, 2=acked, 3=dead. Send hashes producer_id to the
shard, validates payload <=256 KiB and available_at within now..now+7 days.
Message ID is first 16 bytes of BLAKE3(namespace || producer_id), with a stored
payload digest to detect identity conflict. Existing queue_dedup returns the
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
internal request ID and expected summary revision. Tick rechecks row state and
processes at most 128 due items, then republishes a new summary. A timer tick
appends event ID=BLAKE3(run_id || timer_id || "fired"), changes pending→fired,
and applies the transition atomically. Deadline races are resolved by the same
serialized command loop. Losing all notifications cannot strand work because
the catalog scanner revisits published summaries.

Wall-clock time is sampled once per command and clamped against sys_meta.
Ownership uses monotonic time. Nodes detect wall/monotonic divergence >5 s
between periodic samples, fence admission and require clock correction/restart.
Scheduler liveness depends on qualified host clocks; duplicate external activity
execution remains possible even with correct timing and is covered by idempotency.
