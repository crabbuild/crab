# Cross-Cell transaction protocol

## Contract and current boundary

This is the implementation contract for `TransactWriteItems` and
`TransactGetItems` when their primary keys route to different Cells. It is a
design, not a claim that the API works today. The adapter currently rejects
cross-partition requests in `src/backend/data.rs`. A routed single-Cell write
is one `PartitionTransactWrite` command and a single-Cell read is one
`PartitionTransactGet` query. The account token claim records a retry
destination, not a transaction outcome. Data Cells now have internal prepare,
lock, and resolution commands. Sharded coordinator Cells store immutable
participant sets and terminal decisions. The ExtendDB adapter does not yet
drive this protocol or enforce its cross-Cell read rules, so the API remains
unsupported.

Each coordinator now indexes records with unresolved participants and exposes
bounded cursor pages. A new owner can discover both undecided and decided
work after restoring its published Cell state. An internal resolver can now
finish a terminal decision across data Cell participants, using participant
state after an ambiguous reply and recording each resolution durably. A
bounded sweep can abort unfinished `BEGIN` records and complete terminal
decisions after an owner is fenced. Automatic coordinator discovery at
startup, account Cell participants, and the adapter path remain to be built.

The ExtendDB `DataEngine` contract requires all writes, the account-scoped
client token, and stream capture to commit together. Its engine validates up
to 100 unique items and 4 MiB across tables before calling the backend.
BeyondDB currently rejects transaction writes with stream capture, so that
separate gap must be closed before claiming full compatibility. The target
is atomic writes and serializable `TransactGetItems` relative to transactional
writes and individual `GetItem`/`PutItem`/`UpdateItem`/`DeleteItem` calls.
`Query`, `Scan`, and batch operations need per-item read-committed visibility;
they need not expose one snapshot for the whole response.
These targets follow the [DynamoDB transaction isolation contract](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/transaction-apis.html)
and its [100-item, 4-MiB, ten-minute token limits](https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_TransactWriteItems.html).

## Ownership and durable records

Use a deterministic, account-scoped coordinator Cell chosen from the client
token, or from a generated transaction ID if no token was supplied. Its Cell
target is fixed when the transaction begins and independent of table range
splits. Spread coordinator targets across admitted Cells; routing every
transaction through the account directory Cell would retain its single-writer
bottleneck. A participant is one table range data Cell, or the account Cell
for an unrouted table. Sort participants by stable Cell ID before preparing.
Each participant owns all keys for its range, including their local index,
TTL, and stream effects. The coordinator stores the bounded request payload
or enough immutable per-participant input to recover it; a digest alone does
not permit recovery after the original HTTP handler disappears.

Coordinator durable record:

| Field | Purpose |
| --- | --- |
| Transaction ID, account ID, token, request fingerprint | Stable identity and token mismatch/replay detection. |
| Ordered operation references and participant Cell targets/epochs | Recovery uses the original ownership, not current routing. |
| State: `BEGIN`, `COMMIT`, or `ABORT` | One monotonic, authoritative decision. `BEGIN` is published before any prepare. |
| Prepare receipts, failure reason, decision receipt | Prove every participant prepared before `COMMIT`; return ordered cancellation reasons. |
| Creation, decision, and retention times | Bound admission and token replay without expiring undecided intents. |

Participant durable record:

| Field | Purpose |
| --- | --- |
| Transaction ID, coordinator target, request digest, route epoch | Match a prepare to its immutable decision authority. |
| Locked table ID and canonical item keys | Exclude conflicting writes and transactional reads. |
| Old and proposed item images, conditions, operation indexes | Prepare evaluates against one serialized local state; apply needs no expression re-evaluation. |
| `PREPARED`, `COMMITTED`, or `ABORTED` and decision proof | Make resolution idempotent across retries and owner recovery. An `ABORTED` tombstone also fences a delayed prepare. |

The per-key lock has a unique `(table_id, item_key)` constraint and points to
its transaction. A prepared record and all its locks must publish in **one**
participant command. Proposed images are separate from live item rows. No
ordinary read, TTL sweep, index writer, or stream reader may expose them before
commit. A commit-resolution command applies base item, local indexes, TTL
metadata, stream intent, and the applied marker together. An abort-resolution
command removes intent and locks together. It records `ABORTED` even if the
prepare has not arrived, so a delayed prepare cannot acquire locks after
recovery declares the participant resolved. A rejected command cannot leave a
partial prepare: the runtime rolls its application savepoint back while
recording the rejection.

## Write state machine

```text
validate request / authenticate / route and group by participant
  -> publish BEGIN (immutable participant set and fingerprint)
  -> prepare participants in Cell-ID order
       conflict or failed condition -> publish ABORT -> resolve all prepares
       all prepared and published -> publish COMMIT
  -> resolve all participants from the durable decision
  -> return success only after every apply is published
```

The coordinator never publishes `COMMIT` from a count of sent requests: it
requires each participant's published prepare receipt. It can publish
`ABORT` only while still at `BEGIN`. `COMMIT` and `ABORT` are terminal and
mutually exclusive. `ABORT` resolution targets the entire immutable
participant set, including participants that have not yet prepared. Persist
the original operation indexes so a failure maps
to ExtendDB's ordered `CancellationReason` vector, including old item values
only when requested. A client timeout or lost HTTP connection does not change
the decision. A coordinator recovery worker resumes `BEGIN` by durably
aborting and resolving the entire participant set, or by completing prepares
under a fenced coordinator owner; it
resumes `COMMIT`/`ABORT` by driving all resolutions. A participant resolver
queries the coordinator when its intent is old. It may **never** infer abort
from elapsed time or coordinator unavailability. The durable `BEGIN` record
is the authority even after the original host is gone.

Every phase uses a stable `MutationIdentity` and identical input across
retries. `InvocationError::Pending` must be resolved, or the participant or
coordinator state queried, before selecting a new mutation identity. The
runtime request ledger is time-bounded; the transaction records, rather than
that ledger, are the long-lived recovery evidence. Use generation-fenced
coordinator ownership so two workers cannot change a `BEGIN` independently.
An uncertain decision must stop the request with a retryable error and leave
the resolver running; it must never be reported as a clean cancellation.

Acquire all participant prepares in a stable order and fail or back off on
lock conflict. Do not wait while holding one participant's locks for another
conflicting transaction to release its locks. This avoids a distributed wait
cycle. Conditions and updates are evaluated **after** each local lock is
acquired; preflight expression evaluation is advisory only. Single-item writes,
same-Cell transactions, TTL deletion, and table deletion must consult the
same lock table. A separate prepare path without those sibling changes is
unsafe.

A stale routing epoch or sealed participant before prepare forces a durable
abort of this transaction. Retargeting its published participant set would
undermine decision recovery; a client can submit a new transaction after
refreshing routes.

| Failure point | Durable evidence | Recovery action |
| --- | --- | --- |
| Before `BEGIN` publishes | No transaction record or intent | Retry may start with the same token. |
| After `BEGIN`, before all prepares | Immutable participant set, zero or more prepared intents | Fence the coordinator worker, publish `ABORT` or finish preparing, then resolve every participant. |
| Prepare reply lost | Participant command receipt or pending mutation identity | Resolve/query the participant before deciding; never count a sent request as prepared. |
| `COMMIT` reply lost | Coordinator's terminal record or pending mutation identity | Resolve/query the coordinator; never publish an opposing `ABORT`. |
| During commit apply | `COMMIT` plus participant `PREPARED`/`COMMITTED` records | Reissue idempotent resolution until all effects publish. |
| Abort races a delayed prepare | `ABORT` plus participant `ABORTED` tombstone | The later prepare rejects; keep the tombstone until its identity cannot arrive. |
| Coordinator unreachable | Prepared intent with unknown decision | Hold locks and fail affected requests retryably; recover the owner. |

The client token maps to one coordinator record for its account and
fingerprint. Replay reads the terminal decision and returns the existing
outcome without reapplying writes. A mismatched fingerprint fails. Retain the
token outcome for at least the external ten-minute replay window measured
from completion; retain undecided records and participant resolution evidence
until every participant is resolved, regardless of age. After the replay
window, safe garbage collection requires a terminal decision, all-resolution
proof, and no split or backup pin. Reuse after expiry starts a new transaction.

## Visibility and transactional reads

The coordinator `COMMIT` publication is the logical write linearization
point. Before it, prepared values are invisible. After it, a strong keyed
read that encounters an intent must fetch the decision and make that
participant resolve before returning. If the decision is unavailable, it
waits within its request budget or fails retryably; it cannot return the old
item after a known commit. This rule also applies when a prepared write
creates or deletes an item. `Query` and `Scan` need to check intents for each
returned key and for range positions affected by prepared creates/deletes;
otherwise a row absent from the live index could be silently skipped.
Those APIs may mix committed versions across the response, consistent with
their read-committed contract, but must never expose a prepared value.

Running independent `PartitionTransactGet` queries is insufficient: a write
can commit between them and produce a mixed result. A cross-Cell
`TransactGetItems` therefore acquires shared read locks on all requested keys
in the same participant order, after resolving conflicting prepared writes.
Each participant returns values from one local snapshot while its read locks
remain held. Once all locks are held, the collected values have a common
serialization point; release occurs only after the coordinator has durably
closed or fenced the read. Read locks are bounded by a durable read lease,
but expiry must fence a late reader before it can return values. A failed
participant or split aborts the read and releases all acquired locks. This
protocol needs a real read coordinator or equivalent durable lock owner;
using host memory alone can strand locks or allow stale responses.

## Splits, recovery, and capacity

The current split seals a source, exports live item rows, then opens children.
It does not export intents or locks. Therefore `SealPartition` must reject a
source with unresolved prepared writes or read locks, and the split controller
must retry after resolution. A committed token replay may still address the
retained old Cell until the token window ends. The route switch and old-Cell
retention policy must account for both transaction records and token receipts.
Moving an active intent to children is a later protocol change that needs an
atomic handoff and coordinator participant rewrite; it is not implied by the
current item-copy split.

Admit a transaction only if coordinator, every participant, and their
prepared images fit their Cell budgets. The external request limit is 4 MiB,
but staging, old images, locks, receipts, and LTX capture consume additional
space. Bound concurrent prepares per Cell and the total unresolved age; shed
load before a Cell reaches its capture or database limit. Distribute recovery
workers by coordinator Cell and scan bounded due indexes rather than walking
all accounts or 10,000 data Cells per tick. Track prepared count, oldest
intent age, decision-to-resolution lag, conflict rate, retries, and capacity
rejections. A permanently unavailable coordinator is an availability issue,
not permission to discard a prepared transaction.

## Required implementation and proof

1. Add coordinator schema/commands and a bounded recovery cursor. The
   coordinator now records a per-transaction unresolved count, indexed cursor
   pages, immutable `BEGIN`, and one terminal decision. The direct Cell test
   covers discovery of unfinished work after owner restart. A worker still
   needs to consume those pages and drive resolution.
2. Add participant prepare, resolution, and key-lock records. Wire all
   mutation siblings and strong keyed reads through the conflict check before
   allowing cross-Cell requests. Preserve current one-Cell fast path only if
   it obeys the same conflict and token rules.
3. Make split seal reject outstanding intents, retain old owners until replay
   and resolution are safe, and prove restart at every split boundary.
4. Add the adapter coordinator driver, ambiguous-reply resolution, ordered
   failures, transactional read locking, and the ExtendDB stream/index effects.
5. Exercise a signed AWS SDK request across two primary keys on distinct
   Cells, then restart coordinator and both participants from object storage.
   Inject failure after each prepare, immediately before/after decision
   publication, during each apply, and during split. Verify no partial
   outcome through `GetItem`, `TransactGetItems`, `Query`, and `Scan`; verify
   replay and mismatch across restart. Load-test coordinator distribution and
   bounded recovery with the 10,000-Cell, multi-TB target.

Until that proof passes, keep the explicit cross-Cell `Unsupported` response.
Returning success after a coordinator decision alone would leave an SDK
caller able to observe partial application, and returning cancellation after
an ambiguous decision could hide a committed transaction.
