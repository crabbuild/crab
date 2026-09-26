# Cross-Cell transaction protocol

## Contract and current boundary

This document distinguishes the implemented write protocol from the remaining
read and recovery work. Public `TransactWriteItems` now routes Put, Delete,
Update, and ConditionCheck through a durable coordinator, including requests
confined to one Cell. Account and data Cells prepare, lock, and resolve their
operations. The adapter returns success only after every participant apply is
published. Token lookup precedes current table routing and uses that same
coordinator authority. The earlier account claims and local token receipts
have been removed.

`TransactGetItems` still uses one local snapshot when all requested keys share
a Cell; cross-Cell reads remain explicitly unsupported. Account and data
reads reject unresolved intents instead of returning live images that could
predate an already-published commit. This is write-path enablement, not full
DynamoDB compatibility or fleet-scale qualification.

Each coordinator now indexes records with unresolved participants and exposes
bounded cursor pages. A new owner can discover both undecided and decided
work after restoring its published Cell state. An internal resolver can now
finish a terminal decision across account and data Cell participants, using participant
state after an ambiguous reply and recording each resolution durably. An
write driver can also resume a published `BEGIN`: it reads the
immutable participant payloads, prepares in Cell order, records receipts,
publishes one decision, and finishes resolution before returning that decision.
Concurrent resumes and lost prepare/decision replies use durable state as the
authority. Definitive condition, lock, or routing failures request an abort;
an already-published terminal decision wins. Transport uncertainty leaves
recoverable work and never becomes cancellation. Shard admission now registers a fixed shard number in the account Cell before it
returns to a caller. On startup, the server pages that account-owned registry,
recovers shards previously served at its endpoint, then aborts unfinished
`BEGIN` records and completes terminal decisions before accepting traffic.
It first reacquires the participants named by those records, including retained
split sources absent from the current table route. Participant payloads are
stored separately, so target discovery does not read item images.
The private peer listener is available during resolution so recovering nodes
can reach one another; the public DynamoDB listener starts after recovery.
Changed-endpoint coordinator takeover and continuous recovery remain to be built.

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

Each dispatched command has a `MutationIdentity`; retrying that exact
invocation must preserve its identity and input. `InvocationError::Pending`
must be resolved, or the participant or coordinator state queried, before
selecting a new mutation identity. The internal driver reads durable phase
state before repeating prepare, and revalidates the immutable payload through
the participant's stored request digest. The runtime request ledger is
time-bounded; the transaction records, rather than
that ledger, are the long-lived recovery evidence. Coordinator commands run
through one generation-fenced Cell owner, so concurrent drivers compete for
the same immutable decision.
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
fingerprint. `ReadCoordinatorToken` now resolves that identity before any table
routing. A matching token/fingerprint in `BeginCrossCellTransaction` returns
the original record even when a retry proposes different partition epochs or
Cell targets; its stored participant set is never rewritten. The fingerprint
is supplied by ExtendDB, computed from `TransactItems` independently of data
Cell routing. Replay reads the terminal decision and returns the existing
outcome without reapplying writes. A mismatched fingerprint fails. Retain the
successful token outcome for the external ten-minute replay window measured
from completion; retain undecided records and participant resolution evidence
until every participant is resolved, regardless of age. After the replay
window, safe garbage collection requires a terminal decision, all-resolution
proof, and no split or backup pin. Reuse after expiry starts a new transaction.

The coordinator records `completed_at_ms` atomically when the last unresolved
participant receipt is recorded. Repeated resolution receipts do not move it.
Token lookup and admission retain every unresolved record, regardless of age.
After ten minutes from successful completion, admission may unlink the old
token slot and bind a new transaction/fingerprint. A fully resolved `ABORT`
unlinks its token immediately: ExtendDB's SQLite backend rolls token storage
back with canceled writes, so a corrected condition or released lock must
allow the same request to retry. Releasing before all aborts resolve could
leave two attempts competing with unfinished intents.

Both cases keep the old decision and participant rows, so delayed phase
requests retain their original identity. History garbage collection is still
outstanding. Signed cross-Cell replay across a live split remains a qualification
gate; host tests already verify route-independent token replay.

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

The implemented barrier fails closed until a participant resolves. `GetItem`
checks its canonical key; a same-Cell `TransactGetItems` checks every requested
key and returns an ordered `TransactionConflict` cancellation reason.
`Query` probes an intent index using the same HASH key, numeric sort bounds,
direction, and continuation as its live-row query. `Scan` checks locks after
its continuation. Both detect pending creates even when no live row exists.
The lock check and item reads execute in one serialized Cell query. Ordinary
read conflicts map through ExtendDB to retryable `ServiceUnavailable` errors.

These range barriers are conservative: they check the remaining range before
applying the page limit, and compound RANGE predicates may fence extra keys
within the same HASH group. Unrelated keyed reads and disjoint indexed query
ranges remain available. Read-triggered decision lookup/resolution is not
implemented; retry success currently depends on the transaction driver or
startup recovery completing resolution. An outage never permits an old-value
fallback. This is an internal safety prerequisite, not full DynamoDB read
availability or cross-Cell snapshot support.

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

## Driver evidence and limits

`CellStorage::resume_cross_cell_transaction` starts from an already-published
coordinator record. It never reroutes participant keys or reconstructs the
request from an HTTP retry. It reads one participant payload at a time and
prepares them in their persisted Cell-ID order. Participant-local failures
map back to the original operation index before the abort decision is stored.
The same indexed conflict outcome feeds single-Cell public requests and
returns `TransactionCanceled` with ordered reasons instead of the single-item
`TransactionConflictException`.

`tests/elastic_cells/transaction_driver.rs` drives two real data Cells through
commit, replay, a prepare without a recorded receipt, concurrent resumes,
condition failure, competing locks, and stale routing. The signed peer
transport drops replies after a published prepare and commit decision; the
driver still completes from durable state. Aborted transactions release their
locks and preserve both original item images. The SDK test checks ordered
write cancellation and rollback alongside transactional read cancellation.

`tests/elastic_cells/public_transactions.rs` exercises the public storage
adapter against mixed account/data participants. It drops replies after BEGIN,
prepare, decision, and resolution; verifies durable recovery, token replay and
mismatch; and retries a canceled token after its condition becomes satisfiable.
`tests/server_binary.rs` sends signed AWS SDK writes to two primary keys in
different data Cells through the serving binary against RustFS, then checks
replay and both values after a hard kill and restart. The two-owner mTLS test
also checks a transaction and token replay through a replacement frontend.
These tests do not establish continuous recovery or fleet-scale qualification.

Coordinator token tests restore historical BEGIN, partially resolved COMMIT,
and completed COMMIT snapshots. They verify indefinite pinning of unresolved
work, a fresh replay window after delayed completion, mismatch rejection,
route-independent replay, and reuse with a new fingerprint after expiry.
The owner-restart test discovers the pending token, then verifies its release
after fenced recovery resolves every abort.
SQL query plans use the token and transaction-ID indexes rather than scanning
coordinator history.

## Account participant boundary

`src/participant.rs` owns the shared durable phase state machine and schema for
both participant types: immutable prepare identity, replay/mismatch, abort
before prepare, terminal decision conflicts, and staged-image retention.
Account and data wrappers own their image format, local index updates, and
lock release. Their completion callbacks and terminal marker run in the same
Cell command savepoint. This replaces the data-only phase implementation;
there is no second account decision protocol.

Account prepares and account-local TransactWrite share staging and validation
in `src/items/transaction.rs`. Put, Delete, Update, and ConditionCheck each
lock `(table_id, item_key)`, including absent items. DeleteTable and initial
route activation check the table's lock prefix: otherwise a prepared create
could commit into a deleted table or an obsolete account destination. Table
updates do not change primary-key schemas; tags and TTL configuration do not
mutate account item images. Data Cell split and TTL write fences are unchanged.

`tests/elastic_cells/account_participant.rs` runs a mixed account/data driver,
restores all three Cell owners from their object-store roots with an account
prepare pending, then races two resumes through COMMIT. It verifies staged
Put/Delete/Update/ConditionCheck behavior, table-scoped conflicts, ordinary
read/write and same-Cell transaction rejection, scan continuation, deletion
and empty-table route fences, replay/mismatch, abort tombstones, prepared
abort cleanup, and an account condition failure rolling back the data write.
Existing data driver and read-barrier tests exercise the shared state machine
through the other wrapper. This is host-level proof, not a signed public
cross-Cell DynamoDB API acceptance test.

## Read-barrier evidence and limits

The visibility policy belongs in the account and data Cell query handlers, where the
lock lookup and item lookup share the serialized SQLite execution. The runtime
executor refuses queries while its logical head is unpublished
(`crates/crab-cell-runtime/src/cell/executor.rs`, `CellExecutor::query`).
Checking only in the HTTP adapter would leave direct/peer Cell calls exposed
and introduce a check/read race.

| Surface | Entry and enforcement | Evidence |
| --- | --- | --- |
| Keyed read | `CellStorage::get_item` → `PartitionGet` → canonical lock lookup | Two-Cell commit with only the first participant applied; the other returns a transient error. |
| Transactional read | `CellStorage::transact_get_items` → `PartitionTransactGet` | Ordered cancellation through a signed AWS SDK request, then successful read after abort resolution. |
| Query | `PartitionQuery` probes the intent range index before live rows | Pending create/delete, numeric equivalence, forward/reverse cursors, unrelated HASH and sort ranges. |
| Scan | `PartitionScan` probes unvisited intent keys | Pending create without a live row, continuation past locks, and restored locks after owner restart. |
| Split export | `SealPartition` refuses locks; `PartitionExport` requires sealed state | Existing seal, export, split, and recovery checks exercise this boundary. |

These checks run in `tests/elastic_cells.rs` and its
`elastic_cells/transaction_visibility.rs` module. Account Get (also used
by hash-only Query), Scan, and same-Cell TransactGet apply the same lock barrier, keyed
by table ID and canonical item key. Account Scan conservatively fences the
unvisited range, including pending creates without live rows. The account
participant test checks these barriers and their persistence across owner
restart; SQLite query plans use covering primary-key lookups for item, range,
and table fences and an owner index for lock cleanup. Cross-Cell transactional
reads remain unsupported. TTL candidate reads are internal hints; deletion still uses the
lock-aware item command. Usage/statistics queries do not expose item images.

The prior branch behavior returned live images without checking intents.
`origin/main` has no BeyondDB transaction implementation. The new barrier
addresses that unsafe visibility path, while the following API and recovery
gates remain necessary.

## Remaining implementation and proof

1. Add bounded continuous recovery, coordinator passivation and activation,
   fleet placement, and changed-endpoint coordinator takeover. The serving
   binary admits 64 active Cells per node, while token routing can select 4,096
   coordinator shards per account. Current shards remain resident; ordinary
   transaction traffic can exhaust that pool. Startup registry recovery alone
   is insufficient for the 10,000-Cell, multi-TB target.
2. Add cross-Cell transactional read locking and read-triggered write
   resolution. Preserve the account and data read barriers while doing so.
3. Integrate ExtendDB stream and index effects with participant commit. The
   current public adapter rejects unsupported capture/index behavior.
4. Qualify signed token replay across live splits, restart at each split
   boundary, and coordinator loss before/after each decision and apply. Host
   tests cover immutable participants, split fences, and dropped replies;
   signed process tests cover two-Cell writes and hard restart. These are
   complementary evidence, not the complete distributed failure matrix.
5. Bound retained coordinator/participant history without removing evidence
   needed by delayed invocations, splits, or backups. Measure distribution,
   recovery backlog, throughput and latency at 1,000 and 10,000 active Cells
   with multi-TB data.

Keep cross-Cell `TransactGetItems` explicitly unsupported until its snapshot
protocol is verified. Public writes must continue to wait for all participant
resolutions: success after the decision alone could expose partial application;
cancellation after an ambiguous decision could hide a committed transaction.
