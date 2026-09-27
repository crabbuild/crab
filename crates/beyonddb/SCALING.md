# BeyondDB elastic Cell topology

Global indexes now use independent initial ranges and a durable projection
journal. Automatic index range splitting, bounded tombstone retention, projection
throughput, and index-owner fleet recovery remain open scale gates. See
[global indexes](GLOBAL_INDEXES.md).

## Horizontal scaling delivery gates

The agreed target is 10,000 active Cells and multi-TB stored data. The following
six workstreams are required; increasing database budgets or node count alone
does not satisfy them. These are implementation requirements, not delivered
capabilities. Per-Cell and per-node resource admission remains bounded.

| Constraint | Required ownership change | Acceptance evidence |
| --- | --- | --- |
| Placement and rebalancing | BeyondDB composes signed measured capacity, the runtime placement planner, and durable fenced transfers into a distributed controller. | Add and remove nodes under SDK load without manually assigning Cells; observe ownership and traffic redistribute, reject stale owners, and recover interrupted transfers. |
| Account metadata | A sharded table catalog and independently partitioned table directories replace the account Cell's growing route inventory. | One account and one table can each outgrow a metadata Cell; route lookups and split updates stay bounded, and unrelated directory owners write concurrently. |
| Large partition-key groups | Tables without LSIs support ordered sort-key subranges within a partition-key group. | One partition key with many sort keys grows beyond one data Cell; forward/reverse Query, pagination, conditions, transactions, and restart preserve results across splits. |
| Transaction coordination | Coordinator ownership can expand without changing admitted transaction identity; recovery is distributed and retained state has a safe collection protocol. | Expand while tokens and transactions are live, lose owners, replay old requests, and run sustained churn with bounded retained bytes and recovery lag. |
| Index growth | GSI directories and range owners split independently, transferring version and tombstone state as well as projected items. | Force splits during writes, deletes, key changes, delayed projection, and owner loss; the final index converges with no resurrected entries or lost journal acknowledgements. |
| Operational capacity | Measured resource budgets and fleet admission cover restoration, publication, scratch disk, network, and object-store work. | Exercise 1,000 then 10,000 active Cells with real multi-TB data; record throughput, tail latency, overload behavior, backlog age, recovery time, and storage cost on declared hardware. |

### Fleet controller boundary

`crab-cell-runtime/src/fleet/placement.rs` already supplies signed-observation
validation, ranking, fleet balance, and transfer planning. BeyondDB must compose
these primitives with enrollment, scheduling, durable movement, and recovery;
duplicating a placement algorithm inside its table adapter is unnecessary.
The binary now signs fresh memory, disk, Cell/job, and backlog observations on
each lease renewal. OS availability is capped by runtime admission and current
reservations. Probe failures and shutdown advertise no placement capacity; old
samples are never re-signed with a fresh timestamp. These observations now select destinations for request-driven restoration of
idle, published data/GSI Cells. A distributed movement controller remains
unimplemented.
Reservations also reduce OS-available bytes because they can include future
allocation. This conservatively counts already materialized reservations twice
and protects unallocated bytes already held by accepted work.

Linux sampling uses process membership and mount metadata to walk the complete
cgroup-v2 memory hierarchy. Ancestor usage can exhaust headroom even when the
leaf is unlimited. Usage above a limit saturates available capacity to zero.
The probe requires the real root's memory controller and absence of its own
`memory.max`; subtree mounts and namespace roots with hidden ancestors are
ineligible. Cgroup-v1 and platforms other than Linux/native macOS currently
advertise no placement capacity. This restricts automatic placement eligibility,
not existing explicitly owned serving Cells. Container deployment needs an
observable hierarchy or an additional qualified host measurement contract.
See the kernel's [memory and namespace contracts](https://docs.kernel.org/admin-guide/cgroup-v2.html)
and [mount-root semantics](https://man7.org/linux/man-pages/man5/proc_pid_mountinfo.5.html).

The lease signer runs on a blocking worker with at most one probe in flight per
publisher. Its timestamp precedes dispatch; waiting on a probe cannot refresh
the lease deadline. The async lease guard can fence the publisher while its
probe is stalled, and the late worker result cannot publish another renewal.
The HTTP server's separate resource probe is unchanged; this implementation
belongs to BeyondDB's product composition and does not add a runtime scheduler.

Controllers need bounded ownership of discovery/scheduling ranges, controller
lease fencing, and durable progress. One fleet leader must not scan every Cell
or become the writer for every move. Admission and ownership CAS remain the
authority even after a planner selects a destination. Existing Cell identities
survive movement; moving an owner does not itself repartition table keys.

### Metadata topology

Keep account bootstrap descriptors small. Put table-name lookup and table
lifecycle state in a sharded catalog. Put each table's routing records in
directory Cells that can split recursively with bounded fanout. A single
per-table directory Cell would only move the same size and writer constraint
to a different owner.

Keyed requests resolve one directory path, and split publication updates the
affected source/children under a fenced directory epoch. Cached routes require
owner/epoch validation and bounded refresh after rejection. A cache must not
weaken table deletion or the existing immediate credential/policy revocation
contract. Listing and scan continuation must remain correct through directory
splits; no request may fetch all table routes as its normal path.

Current owners are `src/routing.rs`, `src/routing/split_state.rs`, and the account
schema. Their callers include keyed routing, Query/Scan, TTL, GSI projection,
split publication, table deletion, and transaction admission. Each must use the
new directory contract. Existing transactions continue resolving their original
participants rather than rerouting through a changed directory.

### Large collections and index ranges

`src/partition/key.rs::data_key_hash` deliberately hashes only the partition
key. Hash-range splitting therefore cannot divide a large item collection.
For tables without LSIs, ownership needs a composite ordered boundary containing
the partition-key hash, canonical partition-key identity, and encoded sort key.
Retaining the actual partition identity avoids treating hash collisions as one
collection. Query must visit intersecting sort-key ranges in order with bounded
pagination; point requests route the full primary key. Reuse the existing
canonical numeric and binary/string ordering rather than introducing another
key encoding.

DynamoDB can distribute collections without LSIs across partitions. With LSIs,
its item collection stays colocated and is limited to 10 GB, including projected
index data ([partition distribution](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/HowItWorks.Partitions.html),
[LSI limits](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/LSI.html)).
BeyondDB needs an explicit LSI storage/admission design that supports that
contract; its current 512-MiB Cell budget is an earlier implementation limit.
One indivisible hot item still requires a single serialized write authority.

GSI growth uses the directory and fenced range-migration protocol but has an
additional replay invariant: migrate entry versions, deletion tombstones, and
projection progress so old journal work cannot undo newer results. Journal
acknowledgement must follow durable destination application across cutover.
Base-table transaction atomicity does not turn asynchronous GSIs into an atomic
multi-key read view.

Index Cells now implement fenced, version-preserving transfers with fingerprints
that include tombstones. Account plans survive publication until both children
open, and automatic sweeps visit base and index ranges. SDK proof covers a full
node, adding capacity, remote child placement, independent plans, and restored
read availability. Single-HASH-group growth and fleet-scale qualification remain
open; see [global indexes](GLOBAL_INDEXES.md).

### Coordinator expansion and retirement

Changing the current 4,096-shard modulus would strand token and decision lookup.
Expansion needs versioned ownership that preserves lookup for admitted tokens,
immutable participant/coordinator identities, and decisions during migration.
New admission must check the authoritative token owner before publishing BEGIN.
Recovery scheduling must partition unfinished work across owners and give active
backlogs priority over settled history; startup cost cannot grow indefinitely
with every transaction shard ever used.

The ten-minute client-token window alone does not authorize garbage collection.
Collection must account for unresolved participants, delayed phase messages,
runtime replay receipts, saved read images, and backup/restore references. A
retirement fence must prevent a delayed old prepare from recreating a transaction
after its terminal records are removed. Demonstrate both safe deletion and a
bounded steady-state footprint before claiming sustained scalability.

### Delivery order

1. Establish recovery/admission correctness and measured node observations.
   Linux SDK hard-restart qualification passes at `3e6b9072ede`; local failures
   and fleet recovery under sustained pressure still require qualification.
2. Partition metadata and integrate fleet placement through the existing runtime
   authority and movement contracts.
3. Extend fenced range migration to large non-LSI collections and GSI growth.
4. Deliver coordinator expansion, distributed backlog recovery, and safe history
   retirement; retain transaction identity through every topology change.
5. Qualify the composed service at 1,000 and 10,000 active Cells with multi-TB
   stored data, node additions/removals, skew, object-store delays, and owner loss.

Record workload mix, durability mode, hardware, and latency/recovery acceptance
thresholds before interpreting fleet measurements. The existing 45-second small
fixture readiness gate is not a measured fleet RTO. Successful tests of one
workstream do not close the other five or establish full DynamoDB API parity.

Include maintenance traffic in the storage budget. The runtime publisher
currently schedules an otherwise idle Cell's authority renewal three seconds
after the previous successful renewal/publication. For 10,000 continuously
resident, quiescent Cells, that implies a nominal ceiling near 3,333 renewal CAS
operations per second, before client publications, node leases, and recovery
traffic. Scheduling and storage latency reduce the achieved rate. This is a
source-derived workload estimate, not measured fleet throughput; extending the
interval or removing renewals requires proving the runtime fencing contract.

## Scope and present boundary

"Unlimited" is not a literal capacity promise. DynamoDB requests, items, one
writer for one key, node disk, object storage, and the Cell catalog all have
finite bounds. The target is elastic aggregate capacity: add data Cells and
hosts as a table grows, keep each Cell within its admission budget, and make
capacity or hot-key pressure visible as throttling instead of data loss.

The current implementation does **not** meet that target. Unactivated tables
still store items in one account SQL Cell (`src/lib.rs`, `src/schema.sql`). Once
an empty table's initial route is published, the ExtendDB adapter uses
independently owned data-range Cells for keyed CRUD and Scan. Account item
commands are then fenced. Every public TransactWriteItems request now uses
an account-scoped coordinator shard, including requests confined to one Cell.
Token lookup precedes current routing and preserves the original participant
set. Account and data participants durably prepare and lock keys; the
coordinator publishes one decision and the driver resolves every participant
before returning. Successful tokens replay for ten minutes after completion;
canceled tokens are released only after all abort resolutions publish.
The signed SDK process test writes two keys in distinct Cells and verifies
replay and values after an unclean server exit. A serving recovery worker now
visits one locally admitted coordinator and at most one pending transaction per
tick, including coordinators admitted after it starts. It resumes unfinished
requests, advances past failures, and bounds each pass by an indexed durable cursor so new
arrivals cannot starve earlier retries. Its backlog rate remains unqualified.
Cross-Cell TransactGetItems now captures immutable images under shared key
locks through the same coordinator. Get, Query, and Scan now help one blocking
transaction per underlying Cell query when its durable decision is terminal, then repeat
the read. BEGIN, unavailable decisions, and further blockers fail retryably;
transactional conflicts return ordered cancellation reasons. Committed read
images remain retained without collection, another production capacity gate.
BEGIN and prepare now upload bounded 768-KiB binary pieces before the phase
atomically consumes and validates the complete input. Coordinator recovery reads
operations in bounded pieces too. Temporary uploads expire and have a 32-MiB
per-Cell aggregate payload ceiling; this does not reserve eventual apply capacity
or remove the HTTP request-body limit. See the transaction protocol for wire,
SQL, retention, and admission boundaries. A host-backed provisioner can create 1–256
independent, evenly spaced initial data Cells during CreateTable and retry
interrupted setup. This raises initial aggregate capacity and write parallelism.
The host can plan a midpoint split of a serving range and repeat it on an
opened child: admit children, seal the source, copy and verify items, publish
the replacement route, and open the children. A host-invoked, cancellable
account loop inspects one table range per tick, resumes pending splits,
and splits at most one range above a SQLite database-image threshold. The
loop retains its cursor and split plan across transient admission or movement
pressure, retrying on the next tick without terminating node readiness. The
ordinary sweep selects the next range by indexed lower boundary and checks
ownership by partition ID without loading the full route. The
measurement includes indexes and runtime tables but excludes WAL/LTX files.
TTL has a separate per-account worker. Its fixed data Cell expiry index accepts
new writes immediately and backfills existing items with a durable cursor in
bounded Cell commands. It only deletes an item if its TTL attribute is still
expired at deletion. Each worker tick now visits one route page of at most 64
Cells per enabled table and at most 16 tables per account, then commits both
route and table cursors in the account Cell. The cursors survive owner restart.
Enabling TTL primes one 64-Cell page in the request handler; the worker finishes
the remaining pages. Disabling TTL removes the table from subsequent worker
sweeps through account metadata and leaves the fixed data Cell index in place.
Data Cells can continue indexing the old attribute until TTL is re-enabled
or a future cleanup pass reconfigures them. A distributed scheduler and
measured catch-up rate remain necessary for 10,000-Cell qualification.
The provisioner can install this loop in a node task group, and the serving
binary installs it for every locally admitted account. There is no merge controller, and the account
directory remains bounded.
The runtime client can now select a locally owned Cell or forward an operation
through an authenticated peer round trip after reading catalog and authority.
BeyondDB's HTTP state accepts that client. Its signed SDK test now forwards
account, credential, and data Cell operations through the peer protocol from
a separate runtime to the local owner, then verifies recovery. This is a
loopback transport test. `BeyonddbPeers::client` binds the shared HTTP owner
transport to the BeyondDB application. The product now composes a verified
peer receiver and a fleet-scoped principal. A two-node test sends signed AWS
SDK table and item requests through ExtendDB's public listener; its public
node owns the data Cell while a second node owns account and credential Cells.
After the second node's lease expires, its replacement fences the boot session
and restores the account and credential Cells. Another public endpoint reads
the data over pinned mTLS. The replacement rejects data takeover while that
owner is live, then fences its expired boot session, restores the data Cell,
and reads the committed item from object storage. Placement,
fleet-wide unattended crash takeover, and multi-node capacity control remain unimplemented,
so this is not production multi-node service proof. A separate process smoke
now proves signed SDK writes and recovery at a changed peer endpoint through the
serving binary against RustFS after an unclean exit. Startup recovers configured
account and credential Cells, routed data Cells, registered coordinators, and
their original participants. Live remote owners remain in place. A recurring
worker also discovers one registered coordinator per tick across configured
accounts and fences expired owners during serving, including original
participants. An exact-root empty-work cache prevents unchanged Idle history
from repeatedly consuming active slots. At a 250-ms tick, scanning 4,096 shards
takes over 17 minutes before I/O and recovery work; recovery time at the target
scale is unqualified. Data-only nodes, general Cell activation, fleet placement,
and capacity control still need a scheduler. This does not prove aggregate
capacity or fleet-wide failover.
Increasing a Cell's database budget does not increase
write parallelism or provide online repartitioning. Both Cell types declare a
512 MiB database budget and 64 MiB capture budget; host admission supplies
and enforces its own limits.

The current framework has further bounds that affect the design:

- `crab-cell-app` declares fixed namespace shard counts of 1–4,096. BeyondDB's
  data Cell uses entity-partition mode to validate distinct partition targets.
  The shard count and entity mode are part of the application descriptor, not
  autoscaling knobs.
- `crab-cell-runtime`'s catalog has 256 shards with at most 65,536 entries in
  each. Provisioning now reads and rewrites one affected page plus the shard
  head in the common case; a full head may require repacking its pages. A large
  deployment still needs a measured catalog-capacity plan and, before that
  bound is reached, a versioned catalog expansion. More database bytes per
  Cell do not solve that catalog bound.

SigV4 access keys are currently mapped to 256 fixed credential Cells by key
ID. This avoids one global credential writer but remains a finite directory;
credential-shard expansion needs a versioned key-routing migration before any
shard reaches its database or writer limit. Revocation is committed in the
credential's Cell and remains effective after owner recovery.
Cross-Cell transaction decisions use up to 4,096 account-scoped coordinator
shards selected by client token or transaction ID. Only used shards are
admitted. This is another finite per-account writer budget; shard expansion
needs a versioned routing and token-replay migration before saturation.

The serving binary configures 64 active Cells per node, shared by data,
account, credential, and coordinator Cells. Cell admission now reclaims
one settled local coordinator when the pool is full, preferring least-recently
used candidates without pending transactions. Runtime generation checks,
worker close, and authoritative owner release complete before capacity is
reused. Busy Cells and the runtime movement budget apply retryable backpressure.
Registration is discovery rather than residency: token lookup restores an idle
shard's published root, while another active owner remains authoritative.

After release, matching incarnation and commit sequence prove that no BEGIN
raced the empty-work query; only then does admission retire the local recovery
entry. Unproven releases stay scheduled, and the worker reactivates them. Startup resolves registered coordinators one at a
time after private peer routing starts. It no longer needs every historical
coordinator simultaneously resident. Per-Cell directories and fresh activation
paths honor the runtime's resume/restore contract.

Startup and serving discovery now share durable settled-root observations in
the account registry. A skip requires an authoritative Idle control with no owner
or recovery overlay and matching root digest, incarnation, ownership epoch, code,
and schema. Observations are recorded only when an empty-work query receipt
matches the published root. Recovery uses its routed client to write the account
metadata; capacity reclamation never requires a locally owned account. Missing
or stale observations require ordinary recovery, and token lookup always restores
the coordinator. A late older observation can cause extra recovery, not hide a
changed root. The bounded registry has one optional observation per used shard,
but its Cell command receipt history still needs the general retention solution.

This establishes bounded coordinator residency, not elastic fleet placement.
Startup still checks historical shards against authority; settled roots can avoid
restoration, while unproven releases remain in the recovery schedule. A capacity-refused coordinator release waits one runtime movement window and
retries once, preserving generation/settled-work checks. This lets a foreground
multi-range admission span the two-per-second movement budget. Active requests
can still encounter release or sustained pressure and retry from durable state. General
data/account/credential activation, distributed placement, retained-history
collection, and measured overload/recovery behavior remain scale gates.

## Target ownership

```text
authenticated request
  -> account catalog Cell (table name -> immutable table ID)
  -> table directory Cell (routing epoch, ordered partition ranges)
  -> data Cell(s) (items, local indexes, stream intent, TTL state)
  -> published LTX root / object storage

cross-Cell transaction
  -> coordinator Cell (token, participants, durable decision)
  -> participant data Cells (prepared writes, locks, resolution)
```

Account and table metadata stay small. A data Cell owns a bounded range of
hashed partition keys for one table. The directory stores stable Cell IDs,
range boundaries, state, and a monotonically increasing routing epoch. Each
data Cell also has its own epoch, so a split can replace one range without
reinstalling unaffected Cells. Every item command carries table ID and its
target Cell's epoch; a Cell rejects a stale route. Table
IDs never change when a partition splits. Routing hashes a canonical encoding
of the HASH key attributes only, so all sort-key siblings share one Cell owner.
Query can then read that Cell in sort-key order. Scan reads the directory in
64-range pages and pins its epoch while advancing through pages in one request.
Parallel Scan assigns each segment a contiguous hash interval, starts at that
interval, and skips data Cell ranges outside it. A Cell that straddles an
interval boundary still scans and filters its local items; throughput at the
10,000-Cell target remains unmeasured.
Keyed requests use an indexed owner-row lookup in the account Cell, so their
route result stays constant in size as the range count grows. The account Cell
stores one indexed row per range and updates only the split source and children
on publication. Complete-route callers reconstruct from bounded indexed SQL
pages. Split plans store only the source range, two children, and expected
epoch. The host selects ranges, checks publication, and returns split results
through bounded indexed reads and compact plans. Table status checks also use
bounded directory reads. Full-route reconstruction remains available for
diagnostics and tests, with a result-size ceiling.
Continuation across separate Scan requests does not pin a route epoch, so
online split and pagination semantics still need work. A single hot HASH key
cannot be split by this hash-range scheme. The directory itself needs
partitioning before it reaches the account Cell's budget.

The data Cell module uses application-validated entity targets. Its immutable
install contract checks table ownership, range and epoch. The account Cell
validates and publishes an initial, gap-free route, trusting the provisioner
to install each data Cell first. A real host test proves two Cells with distinct
targets, adapter CRUD and Scan across their ranges, and object-store recovery
of data and route state. A second host test covers initial Cell admission and
recovery from an interrupted CreateTable after two data Cells were installed,
and owner restart. It stores more than 64 MiB of item payload across both Cells
and verifies their combined byte count after recovery. The account Cell now
durably records a validated one-range split plan with its source, children,
and expected epoch, and recovers it after owner restart. A
source data Cell can durably seal its old epoch, reject subsequent ordinary
reads and writes, and serve bounded export pages after owner restart. Split
children accept idempotent imports while hidden from normal reads and writes;
they activate only when the imported count and digest match the sealed export.
Activated children remain hidden until route publication and a durable open
command. Late imports are rejected after activation, including after owner restart.
The account directory switches the route with a predecessor compare-and-swap
and retains the exact durable plan until both children open; owner restart
retains both the route and unfinished plan.
The host-backed controller verifies the sealed source and both children before
publication and resumes idempotently after interruption. A host test runs the
account capacity loop to trigger a split, performs a second split, sends a
signed AWS SDK write through ExtendDB's HTTP
handler, and reads the item after owner restart. The signing credential is
stored encrypted in a separate Cell and restored from object storage. Inline
user policy is also stored in the account Cell and authorizes the signed
request with developer mode disabled. This SDK test now uses a published and
renewed node lease and the production HTTP state constructor. A
Cell-backed authorization catalog satisfies ExtendDB's authorization gate;
its management methods still fail explicitly. Full IAM and production serving
remain unverified. The
seal currently pauses access to the source range. Serving-loop integration,
live copying, and a no-downtime cutover remain to be implemented. This
is not a call to `partition_for_shard` with a larger number. Bypassing
application target validation with a raw `CellClient` would leave product
routing outside the compiled topology.

## Online split and merge

1. Observe database size, write queue delay, capture size, and key-range heat.
   Choose a split boundary that moves actual traffic. A single hot item cannot
   be split into simultaneous strong writers; throttle that key when needed.
2. Record a split intent and new route epoch in the table directory. Provision
   two destination Cells with their schemas and owner leases before copying.
3. The source Cell enters a durable splitting state. Copy a published snapshot
   into the destinations, replay the source's ordered changes, then fence new
   source writes and drain the final delta. Reads may continue on the source
   while its final state remains authoritative.
4. Verify key coverage and checksums, publish both destination roots, then
   atomically switch the directory route. Destination Cells accept writes only
   for the published epoch. A stale source request is rejected and retried
   through a fresh route lookup.
5. Retain the sealed source until in-flight requests, transaction intents, and
   backup pins no longer reference it. Merge is the reverse migration and uses
   the same fencing and verification rules.

No request is acknowledged between a source commit and its LTX publication.
Owner loss at each step resumes from durable split state. If route publication
is ambiguous, resolve the directory command by its mutation identity before
admitting writes at either destination. A split never changes key ownership
for a committed request without a durable fence.

## Transactions and reads

The full state machine, visibility rules, split fence, and recovery proof are
specified in [the cross-Cell transaction protocol](CROSS_CELL_TRANSACTIONS.md).

Low-level local writes use one Cell command. All public transactional reads
and writes use the coordinator protocol, including requests confined to one
Cell. General fleet recovery remains incomplete:

1. Order participants by Cell ID; each prepares operations and locks affected
   keys under transaction/coordinator identities. Data participants validate
   the original routing epoch. Prepared writes remain invisible; reads capture
   immutable existing/absent images under shared locks.
2. Once every prepare is published, the coordinator publishes exactly one
   commit or abort decision. Client tokens and request fingerprints live in
   that coordinator's durable state, scoped to the account.
3. Participants resolve the decision idempotently. They never infer abort
   solely from a lease timeout: an unreachable coordinator might have
   committed. Recovery and a background resolver finish abandoned intents.
4. Keyed reads encountering an unresolved intent consult or wait for its
   decision. `TransactGetItems` acquires a consistent read boundary across its
   participants so it cannot see half of a committed transaction.

Splits refuse to seal a source with prepared intents. TTL metadata commits
with the base item. Local secondary indexes with ALL projection now share the
base command and prepare-capacity accounting; import rebuilds their ordered
entries. KEYS_ONLY/INCLUDE require the engine read-contract work in
[LSI_CONTRACT.md](LSI_CONTRACT.md). Stream records must still join the atomic
boundary. Global secondary indexes would require a durable per-partition outbox and idempotent projections with
eventually consistent reads; that path is also not implemented. Base-table
Query reads the HASH key's owner Cell in sort-key order; Scan fans out over a
pinned directory epoch and returns a bounded continuation token naming
per-partition cursors.

## Recovery, backups, and admission

Every serving owner uses a node lease, a fenced Cell takeover, and the current
published LTX root. An account-wide backup pins one directory epoch and a
coordinated published cut of all participating data Cells; independent Cell
snapshots do not form a consistent backup. Restore creates a new directory
generation and publishes it only after every required Cell is available.

Autoscaling must create partitions *before* any Cell reaches its LTX or local
disk admission limit. Per-Cell bounds remain finite and are measured against
recovery time and object-store cost. At capacity, fail or throttle the request
with an explicit retryable error. Never silently route writes to another Cell,
increase an admission limit, or acknowledge an unpublished write.

## Completion proof

The agreed scale target is 10,000 active Cells and multi-terabyte stored data.
The topology is complete only when tests drive the same ExtendDB HTTP endpoint
used by an AWS SDK and establish all of the following:

- Growth beyond one Cell and automatic split under sustained writes, including
  a hot table with multiple active owners and a correct Query/Scan page across
  a split.
- Conditional writes and same-Cell/cross-Cell transactions remain atomic
  through owner loss, ambiguous replies, split fencing, and coordinator
  recovery. No acknowledged write disappears after restart from object storage.
- Streams, TTL, local and global indexes, and backup/restore remain consistent
  across a split and a change in table routing epoch.
- Multi-node load tests measure throughput, p99 latency, split duration,
  recovery time, catalog occupancy, disk usage, and object-store request cost.
  Tests include 1,000 and 10,000 active Cells and verify overload behavior.

Until these gates pass, BeyondDB is a bounded prototype, regardless of the
declared database budget or the number of Cells the framework can address.

## Table deletion and active residency

The independent ExtendDB Python item suite exposed a serving limit after 31
successful tests: the next CreateTable returned ServiceUnavailable on a node
with 64 slots and two initial ranges per table. Earlier tests had deleted their
tables, but their data Cells remained resident. Admission could reclaim only
settled coordinators. A five-slot integration reproduction with one live indexed
table and one repeatedly recreated indexed table failed in 0.17 seconds without
any coordinator transactions.

Foreground table/index provisioning and coordinator admission now inspect bounded
local catalog entries when slots are exhausted; the admission mutex covers
release after metadata lookup. A
candidate must belong to the requested account, have an installed data or GSI
specification, and refer to an exact table ID absent at the current account
owner. The account lookup uses the routed client, so the account can live on a
peer. Reusing a table name does not reuse its immutable ID. An unavailable account or failed
query does not prove deletion; only a successful absent result does.

Reclamation waits up to five seconds for the runtime to report an eligible
settled candidate. This covers asynchronous inventory refresh after recent
commands. The runtime then rechecks generation, work, owner authority, and
movement admission before closing the worker and publishing Idle. The existing
one-window retry for movement capacity is shared with coordinator release.
Published roots, item data, prepared transactions, and index journals remain;
this releases residency and does not implement storage garbage collection.

Serving ranges of live table generations, other tenants, account/credential
Cells, and unfinished installations are ineligible for deletion proof. Sealed
sources have the separate route-removal proof described below. This does not make a fleet of live
ranges fit in one node's 64 slots; general activation/placement and durable
history reclamation remain required. The independent tests ran unchanged from
ExtendDB revision `bdb7b3df4ace3b80a6e928f144036d056aec0327`, with signed boto3
requests against the compiled BeyondDB process and a fresh local RustFS store.
Its 16 transaction tests passed in 59.93 seconds before this residency fix.

This pressure proof runs in foreground table/index provisioning, split-child
admission, and coordinator admission. Background recovery admission still
uses coordinator reclamation; integrating range reclamation there remains
follow-up work. Startup does not reactivate deleted
tables because their directories are absent from the table listing.

The signed SDK recreation regression also exposed a stale table-key cache in
HTTP composition. After deleting and recreating a table name, Scan still used
the previous table ID and returned ResourceInUseException. The server now uses
ExtendDB's existing pass-through table-key lookup, matching its authorization
lookup policy. Local invalidation alone would leave other servers stale. This
adds account-owner reads per request; a future metadata cache needs generation
validation or a fleet-wide invalidation contract before it can safely return
cached table identities. No dependency patch or new setting is required.

### Residency evidence map

| Surface | Evidence / ownership |
| --- | --- |
| Public entry | ExtendDB CreateTable/DeleteTable and item handlers call `CellStorage`; HTTP table-key lookups consult the owner. |
| Admission caller | Initial data/GSI provisioning and foreground coordinator `ensure` supply their routed client. |
| Deletion proof | `DescribeTableById` on the current account owner; target derivation excludes other tenants and applications. |
| Release callee | Runtime `release_idle_cell` rechecks exact generation and settled work, closes SQLite, and publishes Idle. |
| Shared boundary | Settled coordinator release uses the same movement retry; split admission also reclaims obsolete ranges. Background recovery integration remains open. |
| Regression | Five slots, live indexed data, repeated table generations, preserved roots, and restoration of an old participant. |
| Peer proof | Signed SDK recreation with the account on another owner, followed by existing transaction recovery and restart assertions. |
| Baseline | The earlier draft retained deleted data/index owners and cached old table IDs. Current main includes deleted-table reclamation but retains sealed split sources. |

**Is this the best fix?** Reclaim residency through the runtime's existing
fenced release contract, using immutable table-generation absence as product
proof. Increasing the slot limit only delays exhaustion. Deleting Cell history
would break recovery, while a new eviction mechanism would duplicate runtime
authority. The roughly 150 net production lines add bounded discovery and share
the existing release path; uncached metadata uses an existing dependency API.

Verification on 2026-09-26:

- The unchanged upstream transaction and item suites passed together against
  one compiled-server process: **56 passed in 431.58 seconds**. This includes
  the item test that originally failed during table creation. The qualification
  runner records the exact binary digest, dependency revision, client versions,
  service logs, and JUnit output in its artifact directory.
- The five-slot regression passed in 3.68 seconds, including old-generation
  restoration; the expanded signed SDK peer/restart test passed in 108.95 seconds.
- Account integration passed; elastic integration had 29 passes and one SDK
  native-certificate initialization failure. The unchanged failing numeric test
  passed with `SSL_CERT_FILE=/etc/ssl/cert.pem` (8.45 seconds).
- Strict all-target Clippy, compiled server build, Rust formatting, Python Ruff,
  and diff checks passed. No fleet-scale or multi-TB qualification was performed.

## Concurrent request admission

HTTP composition opts into bounded Cell-client waiting. Clones used by storage,
credentials, and catalog share a 128-call budget and a 32-MiB retained-input
budget. Mailbox operations acquire FIFO weighted permits per Cell, reusing the
runtime's 64-request and 16-MiB limits. Their charge is encoded input plus maximum
result size. Routing and execution can overlap within those bounds; distinct
Cells remain independent. Weak gate entries are pruned on admission, bounding
memory by admitted calls rather than historical Cell count. Runtime admission
remains authoritative when other clients contend.

Describe shares client limits and capacity retries, but does not acquire owner
mailbox permits. The peer dispatcher returns that metadata directly. A signed
peer burst exposed HTTP admission errors when Describe bypassed all waiting;
timing probes then exposed waits of up to 30 seconds when it incorrectly joined
the write mailbox queue, followed by only milliseconds of metadata work. The
full-mailbox/Describe regression failed before correcting that distinction.

Known capacity refusals receive paced retries for at most 50 seconds per
transport stage. Commands retain identity, digest, and expected incarnation and
revalidate expiry before every attempt. The existing mutation lifetime is
60 seconds. Describe and execution have separate budgets; this is not one HTTP
request deadline. Fencing, durable rejections, and ambiguous outcomes are not
retried, including unknown publication with a capacity cause. The wait bound
never cancels accepted work. Caller cancellation releases client permits while
accepted runtime work retains its lifecycle.

Query and resolution calls share mailbox admission. Replica reads retain their
separate router. Unconfigured clients, provisioning, and background workers keep
existing admission. Owner mailbox, SQL, publication, and node-memory limits are
unchanged. The input budget covers retry-layer encoded inputs and overhead,
not HTTP bodies, caller-owned typed inputs, transport codecs, or result memory.

### Admission evidence map

| Surface | Evidence / ownership |
| --- | --- |
| Entry / caller | Signed ExtendDB handlers use the client shared by `build_http_state` storage, credentials, and catalog. |
| Mechanism | CellClient wraps its encoded transport; clones share request and input-byte limits and per-Cell weighted gates. |
| Callee | Runtime transport reloads catalog/authority; local handle and authenticated peer admission remain authoritative. Describe does not call mailbox admission. |
| Mutation safety | Prepared identity/digest/incarnation survive known refusals; peer ambiguity remains `OutcomeUnknown` and requires resolution. |
| Siblings | Commands, queries, and resolve share mailbox policy. Describe uses shared client policy. Replica/default/background clients retain their separate paths. |
| Regressions | Full-mailbox Describe, overlapping calls and FIFO, shared-budget cleanup, wait expiry, exact pending-mutation evidence; signed SDK burst and restart. |
| Baseline | `origin/main` has no BeyondDB subtree; its CellClient immediately returns owner capacity errors. The preceding draft failed 47 of 50 writes. |

**Is this the best fix?** The encoded client boundary owns reusable waiting,
while HTTP composition owns its budget. Matching each transport operation to
its actual runtime resource avoids both unbounded buffering and an unrelated
metadata queue. About 280 production lines provide this opt-in mechanism;
authority, publication, and outcome resolution retain their canonical paths.
This fixes tested bursts and the Describe admission mismatch. It does not yet
satisfy sustained-load or fleet-scale qualification.

### Qualification results

On 2026-09-26:

- Ten Cell-client tests passed, including the full-mailbox/Describe regression
  that failed before the source correction.
- Signed SDK peer/restart passed in 246.37 seconds: 50 increments with SDK
  retries disabled, exact counter after owner replacement, and existing
  cross-Cell transaction recovery assertions.
- Strict all-target Clippy for both changed crates, compiled-server build,
  formatting, Cell/LTX layout, and diff checks passed.
- The final unchanged upstream concurrency run against a fresh compiled-server
  and RustFS pair passed both 50-writer cases, but **55 of 1,000 updates returned
  ServiceUnavailable**. Total: two passes and one failure in 898.35 seconds.
  The final counter-value assertion and concurrent-delete test were not reached.
  These error codes alone do not identify whether every failed call was refused
  before execution or had an ambiguous outcome.
- A prior attempt with the same binary stopped during conditional writes with
  `node lease bounds are invalid`, followed by credential lookup failure. The
  log does not distinguish delayed publication from a wall-clock change.
  Lease duration and terminal fencing were not relaxed for the unchanged rerun.
- Additional unchanged upstream batch, Query/Scan, and continuation suites
  passed at the preceding commit: 68 tests in 225.85 seconds. The separate GSI
  and composite-key selections were not reached during the failing load runs.

Intermediate trials are retained as failed evidence: a five-second retry
window failed 35 of 50 writes; 30-second retry-only waiting failed 291 counter
updates; a one-call FIFO gate at 30 seconds failed 122 updates; its 50-second
variant and the initial weighted gate both hit SDK read timeouts. The final
Describe correction removes that reproduced mailbox dependency, but the latest
55-error result still requires diagnosis rather than another claim of success.

Remaining work includes request-stage timing for those errors, hot-key
publication throughput, operation-specific result bounds, ingress memory
admission, and fairness across independent clients. The pinned ExtendDB
`OpError` has no transient variant, so exhausted authorization admission still
maps to InternalServerError. Correct classification requires an upstream
contract change; no dependency patch is included. No 10,000-Cell or multi-TB
qualification was performed.

### Admission timing and lease shutdown follow-up

A diagnostic run of the unchanged 1,000-increment upstream test failed with
two `ServiceUnavailable` responses in 622.93 seconds. Both calls expired at
the client's 50-second Cell admission wait, before invoking the owner. The
final counter assertion was not reached. This classifies those two failures;
it does not establish the cause or execution outcome of the earlier 55 errors.

During that run, 208 measured lease refreshes took at most 1,793 ms, leaving
at least 5,206 ms on the previous guard. Wall-clock and monotonic elapsed time
differed by at most 1 ms. Thus the earlier lease failure did not reproduce.
The timing probes and SDK logging plugin were diagnostic only. Increasing
lease duration or admission deadlines is not justified by this evidence.

Inspection found a separate shutdown defect: `PublishedNodeLease::run` awaited
directory refresh without observing cancellation or fencing during that await.
The new regression counts entry into a PUT before a throttled store stalls it,
then cancels renewal or fences the guard. Before the fix, cancellation failed
its one-second completion bound (test failed in 4.03 seconds). An outer select
now covers the whole renewal loop, including storage and retry waits.
Cancellation retains the current deadline; fencing remains terminal. Dropping
the request cannot establish whether its remote CAS committed and never grants
a new local lease.

The binary and both leased integration fixtures also now register renewal with
`CellNodeTaskGroup::spawn_lease_maintenance` and the terminal node-shutdown
token. Previously they registered ordinary work, which stops before runtime
drain. The existing host phase retains the heartbeat until runtime publication
and log closure finish; no new lifecycle owner is added.

| Boundary | Evidence |
| --- | --- |
| Caller | Binary startup installs the published guard and retains renewal in the host's lease phase. Peer and numeric-query fixtures use the same ordering. |
| Cancellation owner | `PublishedNodeLease::run` selects shutdown, guard fencing, or the existing refresh/retry loop. |
| Storage contract | `Store::update` is an ETag CAS without blind retry; `NodeDirectory::refresh` resolves changed observations. Cancellation may leave an unknown remote write, so the local guard is never advanced on cancellation. |
| Host contract | `CellNode::shutdown_until` drains work and runtime before canceling lease tasks; `session_withdrawal_waits_for_runtime_drain` protects that ordering. |
| Regression | `stalled_lease_refresh_yields_to_shutdown_and_fencing` fails before the change and passes after it. Ordinary successful renewal and current-deadline preservation also pass. |
| Baseline | Main has no BeyondDB. The preceding draft awaited refresh directly and registered renewal as ordinary work. |

**Is this the best fix?** One cancellation boundary reuses the existing guard
and host drain phase. It removes duplicated cancellation branches without
changing the lease, retry interval, admission budget, or decision authority.
This addresses shutdown liveness; it does not fix sustained write throughput.

The upstream qualification runner now uses a separate 30-second setup client
for durable bucket creation, which previously reused the one-second readiness
probe client. Exhausted readiness polling fails explicitly and setup clients
are closed. DynamoDB client timeouts, retries, and upstream assertions are
unchanged. The diagnostic run above exercised the corrected setup path.

Verification for this follow-up:

- Three selected lease checks pass (9.05 seconds), including the stalled-PUT
  regression for cancellation and fencing.
- Signed SDK peer/restart passes (279.54 seconds), including cross-Cell
  transactions, owner replacement, and exact counter persistence.
- The compiled-server SDK/RustFS smoke fails (743.84 seconds) at the large
  transaction Put in `tests/support/mod.rs:168`, with `ServiceUnavailable`.
  It did not reach the historical-coordinator loop or restart assertions.
  The response does not distinguish a known refusal from an ambiguous pending
  invocation; the server log supplies no underlying error. This run does not
  prove compiled-server restart behavior for this revision.
- The serving binary builds; strict all-target Clippy passes (6.08 seconds).
  Python Ruff, Cell/LTX layout, formatting, and diff checks pass. No dependency,
  lockfile, or test assertion changes.

The draft remains unqualified for sustained load and production scale. The
large-transaction smoke failure needs underlying invocation evidence before
assigning a cause or changing capacity, deadlines, or retry policy.

### Large transaction transfer qualification

Temporary probes reproduced the compiled-server failure in 515.27 seconds at
`binary-transfer-put`. The coordinator had a durable COMMIT with an unresolved
participant; later calls reported a SQLite wall deadline and a fenced executor.
That run does not establish whether the deadline was spent in application work,
worker queueing, or capture/storage work.

A reduced SDK diagnostic retained the original binary/escaped transaction
assertions on a fresh server. Binary image application took 175 ms for the put
and 452 ms for the update. The subsequent escaped transfer repeatedly expired
its 60-second upload reference and failed after 308.45 seconds. Every 256-KiB
input piece required its own durable command before sealing.

Transaction uploads and stored transaction payloads now share a 768-KiB chunk
bound. This leaves 256 KiB for SQL text and typed parameter overhead under the
runtime's existing 1-MiB SQL-call limit, reducing the durable upload count by
about two thirds for large inputs. The reduced diagnostic passes in 166.44
seconds. Upload lifetime, SQL deadline, SDK retries, digest checks, and atomic
phase consumption are unchanged. Individual item and GSI journal chunk formats
are unchanged. The abort-during-upload fixture uses a larger escaped item so
its existing multi-piece assertion and abort-before-completion sequence still
exercise that boundary.

| Boundary | Evidence |
| --- | --- |
| Entry and callers | Signed TransactWriteItems reaches `admit_transaction`; BEGIN and account/data prepare all call `upload_transaction`. |
| Owner | `transaction_transport::CHUNK_BYTES` bounds temporary inputs and stored transaction payload pieces. Registry upload and recovery-query limits derive from it. |
| Callee contract | Runtime SQL batches bound both encoded inputs and results to 1 MiB. Upload SQL has one bounded payload plus fixed identity/position parameters. |
| Siblings | Coordinator operations/abort reasons and participant staged images share `transaction_payload`; capacity claims use the resulting chunk count. Item storage and GSI outboxes keep their independent bounds. |
| Baseline | Main has no BeyondDB. The preceding draft used 256-KiB upload and payload pieces; the reduced escaped-input workload exceeded their fixed lifetime. |

**Is this the best fix?** Keep bounded SQL and durable phase ownership, but use
more of the existing call budget for data. One shared chunk bound prevents the
recovery reader and stored-payload writer from disagreeing. This improves the
measured transfer case; it is not proof against arbitrary object-store latency,
and the earlier execution deadline and sustained-load failures remain distinct
qualification concerns.

The first full clean SDK rerun failed in 242.20 seconds during a small
TransactGetItems call, before the large-payload checks: the serving endpoint
refused the connection. Its temporary log was removed during unwinding, so the
exit cause is unavailable. The process fixture now prints child logs on panic
before temporary-directory cleanup; workload assertions, retries, and timeouts
are unchanged. This diagnostic change is required to classify any recurrence.

Final focused upload safety checks pass (two tests, 3.26 seconds). Signed
SDK peer routing and owner-replacement recovery pass (275.31 seconds), including
the large binary/escaped cases and an interrupted committed transaction.

The final clean server smoke failed in 1,188.30 seconds during the 70-token
coordinator-residency loop, after the preceding binary/escaped transaction
assertions. The public result was TransactionCanceledException with
ThrottlingError for the first participant; its source is not present in the
empty child logs. This is a capacity-refusal regression to diagnose, not a
successful full-server qualification. A diagnostic rerun retains the same
workload and records the exact capacity error at participant preparation.

The capacity diagnostic ended earlier in 269.94 seconds with connection refused
at the large-payload SDK Put. It did not reach residency churn or report a
participant capacity error. Empty logs do not prove that the child exited:
readiness loss can close the listener before node drain returns its error. The
fixture now records child status before cleanup, and the next diagnostic logs
supervised task failures and serving-loop exit before drain. This preserves the
unchanged workload and distinguishes listener shutdown from process termination.

The health diagnostic failed in 472.41 seconds during a large-payload Update.
The child was still running (`try_wait` returned None), while a supervised task
had returned Fenced and the serving loop reported lost node readiness. Thus
this connection refusal followed listener shutdown; it was not an observed
process crash. Lease renewal is the only installed task returning the runtime
Fenced error directly in this composition. Renewal-start delay, storage time,
clock deltas, and remaining lease time need measurement to distinguish why it
fenced. This result does not classify the separate participant capacity refusal.

The lease-timing run failed in 666.39 seconds on the first residency transaction.
Its final refresh began with 7,344 ms on the existing guard and remained inside
NodeDirectory storage refresh until fencing 7,348 ms later. Earlier successful
refreshes reached 5,081 ms; no refresh error or renewal validation error was
observed. The process sampler's maximum two-second sampling gap was 2.024
seconds, and measured wall/monotonic deltas agreed, so this run supplies no
host-pause or clock-step evidence. The child then exited with code 1 and Fenced.
The existing ten-second publication policy leaves about seven seconds after its
three-second heartbeat interval; slow storage can consume that entire window.
A delayed authoritative-CAS regression now reproduces this policy boundary
without the full SDK workload. It does not reproduce the separate participant
capacity cancellation from the earlier run.

The eight-second delayed-CAS regression fails with Fenced at 10.02 seconds
under the old policy. The serving lease now lasts 15 seconds, with the same
three-second heartbeat and terminal fencing rules. This allows roughly
12 seconds for the initial renewal and adds up to five seconds to owner-loss
detection. It remains below the provisioner's existing 30-second expiry wait
and within the runtime's 15-second signed-advertisement lifetime contract.
The peer fixture waits for
authoritative expiry instead of sleeping eleven seconds; takeover authorization
and its recovery assertions are unchanged. This is a measured availability
policy change, not a cure for unbounded storage latency or a fleet RTO claim.

The initial 20-second trial failed three lease tests at advertisement signing: the
60-second guard ceiling is not the advertisement lifetime contract. The runtime
limits signed advertisements to 15 seconds. The corrected product policy uses
that existing limit; no runtime validation or takeover rule is widened. Repeated
slow refreshes can still consume the remaining deadline and must fence serving.

With the corrected policy, all three lease regressions pass. The seven-test
selection finished with six passes and one GSI recovery read failure (owner
not locally available); that test passed alone in 1.67 seconds. This remains
unresolved intermittent evidence. Strict BeyondDB all-target Clippy passes
(20.41 seconds). The server now initializes warning/error tracing on stderr,
and participant capacity refusals retain their cause in server logs. The
lockfile change adds only the already-pinned tracing-subscriber dependency edge.
The unchanged signed peer SDK workload passes with the corrected policy
(273.05 seconds). The full compiled-server SDK workload against RustFS fails in 1,858.61
seconds during coordinator churn, before the hard restart. Its public error is
InternalServerError during authorization; the child remains alive. The retained
log first reports a SQLite wall deadline in the projection worker, followed by
fenced-executor and inactive-Cell errors, then authorization cache-load failure.
This run does not reproduce node-lease expiry or classify the earlier participant
capacity refusal. Large payload writes completed; the last unique transaction
snapshot had 47 churn commits and one pending BEGIN. The long run also exceeded
an earlier token replay window, but did not reach that later assertion.

A focused runtime regression now blocks a shared SQL worker with one Cell and
queues a read of another Cell until its deadline. It tests whether queue expiry
fences an untouched Cell; this is a candidate mechanism, not yet proof of where
the full workload spent its deadline. SQL/page I/O and lifecycle timing still
need distinction if that reproduction does not explain the observed path.

### SQL queue expiry and collateral fencing

The focused regression failed before the runtime change in 5.11 seconds:
a query waiting behind another Cell returned Deadline, then its untouched Cell
returned Fenced. The same unconditional timeout fencing exists on current
`origin/main`. This establishes a runtime availability defect; the full server
failure still needs a rerun to establish whether this mechanism explains it.

SQL workers now arbitrate queued cancellation against native execution with one
shared deadline state. Expired queued work cannot invoke its callback, and a
queued command/effect cannot write after its caller receives Deadline. Started
callbacks retain interruption, fencing, unknown-outcome, and recovery behavior.
Worker and request reservations remain held until cancellation is acknowledged
or the callback exits. Resolution returns Unknown without fencing on queue
expiry. Hydration defers queued expiry without claiming completion. Migration
still fences because its old handle has already been closed.

| Surface | Evidence / ownership |
| --- | --- |
| Entry | CellHandle query, execute, effect delivery, and resolve feed actor requests. |
| Start boundary | `SqlDeadline` in runtime worker; worker admission and native queue share the same state. |
| Callee | `run_native_callback` checks the deadline before touching the executor or sparse VFS. |
| Siblings | Commands, queries, effects, resolution, and hydration share the start guard; migration preserves closed-capability recovery. |
| Inventory | Background inventory and transfer inspection do not fence on their timeout; their existing error handling remains unchanged. |
| Regression | One worker exercises permit wait; two workers with same-lane Cells exercise native queue wait. Subsequent reads verify unchanged data and usable ownership. |
| Existing contract | Running SQL interruption and native late-mutation recovery tests remain green. Ordinary waiter cancellation must still preserve accepted commands. |

The eight handler tests passed together in 10.39 seconds. The final combined
runtime selection passed all 22 tests in 10.52 seconds, covering queued expiry,
accepted-waiter cancellation, migration, running-handler fencing, and hydration
failure/owner-loss paths. All three worker tests passed in 0.34 seconds, including
worker-side expiry before a callback starts and sparse hydration retry. Strict
all-target Clippy for runtime and BeyondDB passed in 33.07 seconds; format,
Cell/LTX layout, actor policy seams, and runtime documentation checks passed.
The signed two-owner peer SDK test passed in 273.41 seconds with the queue
fix. The compiled-server SDK workload failed in 2101.46 seconds after completing
all 70 coordinator-churn writes and killing the original server. Its replacement
remained alive but did not become healthy within the existing 45-second gate.
The serving binary's SHA-256 was unchanged through the restart. No SQL deadline
or fencing message was observed during pre-restart polling; this run does not
prove the earlier availability failure fixed under all load conditions.
Warnings now identify the Cell and whether SQL had started when a foreground
deadline expired; no request payload is logged.

During the queue-fix server rerun, all eight named large Put/Update transactions
reached COMMIT with zero unresolved participants, and the SDK advanced into the
70-coordinator churn phase. This is pre-restart evidence only. A read-only sample
of the first 19 completed churn coordinators found 25 extra `Replay` phase
receipts across 16 coordinators. Receipt decoding followed the runtime's
big-endian length-prefixed `Json<T>` codec. Each sampled coordinator held one
transaction; ordinary upload/BEGIN/decision/first phase receipts were counted
separately. These are repeated durable phase commands, not duplicate item writes.

Serving recovery can call `resume_cross_cell_transaction` while a foreground
driver is still active. The current driver queries unresolved participants,
including already-prepared participants, and each phase call uses a fresh runtime
mutation identity. These paths explain how repeated commands are possible, but
the sample does not identify the caller of every extra receipt. A three-transaction
early timing sample measured 14.085 seconds median BEGIN-to-completion. A brief
native stack sample found all four SQL workers waiting for jobs, so it does not
support attributing that entire latency to SQL execution. Isolate overlapping
drivers and storage publication before changing recovery scheduling or claiming
a throughput improvement. The subsequent restart failed its readiness gate.

A new driver fixture prepares and records only the first of two participants
before resumption. It checks that completion adds only the remaining prepare,
decision, and two resolution receipts. The existing lost-prepare-receipt and
concurrent-driver cases remain. The regression fails on the existing driver in
0.27 seconds: five coordinator commits instead of four. Compiling only the test
changes left the live server executable's SHA-256 unchanged. The initial test
filter selected zero tests; the fully qualified test was then run explicitly
and produced this failure.

The unresolved-participant query now returns whether a prepare receipt is
recorded, and resumption skips that participant's payload and prepare commands.
Query codec version 2 declares the changed response; the persisted schema is
unchanged. Recovery and terminal resolution still receive every unresolved
participant, including prepared ones. Their callers share the same query but
do not use the new flag to filter ownership recovery or resolution. The existing
partial-COMMIT visibility test additionally checks that its unresolved prepared
participant remains listed. CellStorage uses current-owner reads; COMMIT still
checks every participant's durable prepare evidence inside the coordinator.

This fixes redundant resumption after a recorded prepare. Concurrent drivers
whose snapshots both predate that receipt can still repeat work; their existing
idempotency and decision rules remain necessary. The change avoids a process-local
driver lock, which would not coordinate foreground and recovery clients on other
nodes. The targeted driver test passes in 2.80 seconds. Its new coordinator
scenario required one additional fixture Cell slot (17 instead of 16); the
initial green attempt passed the new receipt assertion, then exhausted slots
in the existing background-recovery setup. Production capacity is unchanged.
All 16 transaction/coordinator tests pass in 57.90 seconds, including lost
replies, reserved capacity, concurrent drivers, owner recovery, token expiry,
terminal resolution, index maintenance, and TTL locks. The separate data-range
visibility/restart test passes in 2.02 seconds, and strict all-target BeyondDB
Clippy passes in 22.96 seconds. These tests do not establish an SDK latency gain
or resolve the full server's restart-readiness failure.

The readiness failure supplied no replacement-process diagnostics. The fixture
previously truncated the shared log on each start, losing the former owner's
messages; it now appends within the fresh fixture directory. Startup phase
timing and a focused recovery reproduction are needed before attributing the
45-second failure to lease expiry, partition restore, or coordinator discovery.
The readiness deadline is unchanged.

A diagnostic rerun recorded elapsed times around account, credential,
partition, and coordinator recovery, plus coordinator shard progress and active
Cell count. Its temporary probes used `[DEBUG-beyonddb-startup]`. The fixture was
temporarily retained on the workspace
volume so a failed restart can be replayed without another full SDK write pass.
The new empty-store startup reached public-listener startup in 618 ms; this is
not recovery evidence. The same 45-second hard-restart gate remains in force.

The diagnostic replay helper copies stopped fixture storage into a separate
workspace directory, uses fresh serving addresses, records the binary digest,
and reports the original 45-second gate separately from a longer observation
window. Its first active-process check missed the Workspace symlink and began
copying the running store. That copy was interrupted before any replay server
started and discarded; the source fixture was not modified. The corrected check
recognizes both path spellings and refuses the live fixture. Copy I/O overlapped
the write phase, so this rerun cannot supply a clean write-latency comparison.
Replays start from the retained failure state, which may already contain
partial recovery; they do not reproduce the original crash cut or its remaining
lease lifetime by themselves.

The instrumented SDK run failed in 1,269.38 seconds, before its hard restart.
All eight named large transactions and 30 churn transactions completed; one
churn transaction remained unfinished in the retained local coordinator images.
TransactWriteItems returned HTTP 503 after repeated `CellNotActive` errors.
The upstream log target names `create_table`, but the failing SDK call is the
transaction loop. The test now includes the client token in that assertion so
future failures identify the exact request without inferring it from log labels.
This run does not demonstrate that the earlier availability failures are fixed.

A separate replay copied that stopped fixture's object storage, started a fresh
process with fresh addresses and local data, and used the same verified binary
digest. It missed the unchanged 45-second readiness gate and remained unready
through the 120-second observation window. Account and credential recovery
finished in 575 ms combined; partition recovery then took 20.03 seconds.
Coordinator restoration ran serially: the log reached shard 3,367 after
92.97 seconds of coordinator recovery, with 50 active Cells. The immediately
preceding restorations took approximately 21.51 and 7.54 seconds. These are
startup progress intervals, not isolated SQL or object-store timings.

The replay establishes a recovery reproduction without repeating the full SDK
write workload. It does not yet isolate why individual restores slow down as
residency grows, or why the original transaction encountered an inactive Cell.
Those remain separate diagnosis gates. Neither a larger readiness timeout nor
a larger Cell budget would establish bounded fleet recovery.

The next replay added per-shard phase boundaries and resource snapshots. It
became healthy after 110.69 seconds, still failing the 45-second gate. Across
35 restored coordinator shards, owner acquisition consumed 31.40 seconds,
participant discovery 0.62 seconds, transaction recovery 1.68 seconds, and
settled-root observation/registry publication 43.43 seconds. Partition recovery
took 31.75 seconds. Other builds were active on this workstation; these figures
locate work on the startup path, not a production throughput or latency claim.
Snapshots showed local disk reservations far below the 1-GiB budget, but do not
exclude transient SQL, hydration, or filesystem contention between samples.

Recovery now batches settled-root hints per discovery page instead of publishing
one account command per shard. The existing checkpoint/restart test gained an
account commit-sequence assertion: three recovered shards must require one
registry publication. Before the fix it fails with three publications versus
one in 0.29 seconds. Its existing checks still require unchanged settled shards
to remain Idle, a stale hint to trigger BEGIN recovery, participant locks to be
released, and old successful tokens to replay without applying writes again.
The private command's vector input changes its codec to version 2 and its input
bound to 64 KiB for at most 100 observations. See the transaction document for
the unchanged authority checks. The new module digest prevents using the old
retained fixture as post-fix end-to-end evidence; fresh-fixture SDK recovery is
still required. This change does not resolve the inactive-Cell SDK failure or
prove the overall restart gate passes.

The batched checkpoint/restart regression passes in 0.49 seconds, including its
stale-hint, raw participant recovery, and token replay assertions. Temporary
startup probes and the fixture cleanup override have been removed; stopped
diagnostic fixtures remain separate inputs on the workspace volume. The batch
change adds 22 net production lines across the shared observation helper, command,
and descriptor, replacing per-shard startup publication with bounded page writes.
All seven focused coordinator/recovery tests pass in 19.65 seconds, including
bounded residency, discovery convergence, admission failure, and healthy
participant resolution during failed startup. Strict all-target BeyondDB Clippy
passes in 50.79 seconds; format, diff, runtime layout, and policy checks pass.
The fresh signed two-owner SDK test passes in 287.17 seconds, including remote
routing and replacement-owner recovery. The standalone server's SDK test
completed all 70 churn transactions, killed the original owner, and failed the
replacement's 45-second readiness gate. Total test duration was 3,231.07 seconds.
The replacement was still alive at cleanup. Appended logs preserve mailbox-byte
pressure and inactive-Cell warnings from the original serving run, but do not
identify the replacement's slow startup stage. This run includes the settled-hint
batch fix and predates the resolver change below. Neither the focused tests nor
the peer result closes the standalone-server recovery gate.

The owner-routing audit identifies a separate availability gap that must be
tested before attributing the SDK failure to it. Runtime
`CellClient::runtime_with_peer` explicitly does not acquire Idle Cells, and
`PeerHttpRoundTrip::owner` rejects a control with no owner. BeyondDB's
The peer client previously used that transport directly. Foreground coordinator
admission could reacquire an Idle coordinator, but normal keyed reads, Scan,
credential lookup, and transaction phase requests had no shared admission path.
Background transaction and index recovery cover subsets of those targets.
Runtime pressure shedding can release a settled Cell independently of the
product provisioner's admission mutex. A solution therefore needs a shared
product resolver that validates catalog identity and scope, acquires only through
runtime authority/admission, preserves live remote owners, and never creates an
uncataloged Cell from a read. Adding retries solely to GetItem would leave the
sibling paths uncovered. The source trace alone did not prove the prior failure's cause.

A focused signed SDK regression now reproduces the missing reacquisition:
CreateTable and PutItem succeed, the data owner drains, and GetItem returns
503 ServiceUnavailable in 4.21 seconds. The test shuts down its listeners and
runtime before reporting the failed read. This isolates a released-Cell request;
it does not establish that pressure shedding caused the earlier churn failure.

The pending fix supplies a product resolver to the shared runtime transport.
Describe, command, query, and mutation resolution use the same local selection
path. The resolver validates scope and catalog identity, preserves existing
owners, and restores an Idle authority with a published root under the
provisioner's admission gate. A Recovering authority already claimed by this
session resumes its exact root and epoch after an interrupted acquisition. It never provisions missing catalog or authority
records. Authenticated peer receivers still require the owner selected by the
sender to be active; a raced release rejects instead of starting a second
placement decision. Deleted-range reclamation performs metadata reads outside
the admission gate so an idle account can be restored without a recursive lock.
The regression now also releases account and credential Cells. A second case
publishes only the ownership claim, then checks the SDK read resumes restoration.
The final all-target compilation check passes, including the interrupted-claim
case. Strict all-target BeyondDB/runtime Clippy passes in 1 minute 44 seconds.
The expanded fixture uses two ranges in one table. A third SDK regression
releases both data owners, conditionally updates one primary key in each through
TransactWriteItems, and verifies both values through TransactGetItems. All three
focused SDK regressions pass together in 1.23 seconds; focused strict Clippy passes
in 1 minute 47 seconds. The original two read cases passed in 0.56 seconds before
the fixture expansion. A temporary manifest
compiled the exact peer-network test source against the current BeyondDB library
without a server binary target; its dependency versions match the workspace
lockfile. The standalone restart run's binary fingerprint remains unchanged.
The existing two-node SDK scenario also passes against the new library in
250.30 seconds, including live remote ownership, signed mTLS forwarding, and
replacement-owner recovery. Standalone restart verification remains pending
for the resolver change; the earlier standalone SDK run contains the settled-hint
batch fix but predates the new resolver.
The runtime resolver-refusal regression passes in 0.20 seconds: refusal remains
NotStarted and the underlying owner sees no write. The existing local/remote
owner-routing regression passes in 0.06 seconds. These tests cover dispatch
selection, not the product's restore or interrupted-acquisition paths.

A separate process regression now isolates coordinator history from large
payloads and secondary indexes. It uses the same RustFS/bootstrap fixture,
creates two data ranges, and commits 70 transactions through distinct coordinator
shards before killing the server. The replacement must pass the unchanged
45-second readiness gate at a new peer address and return version 69 from both
data Cells. The fixture retains failed restart storage for replay. Strict Clippy
for the process test passes in 26.23 seconds. The first RustFS run failed the
replacement's 45-second readiness gate after all 70 transactions committed in
1,403.94 seconds; total test duration was 1,452.44 seconds. A read-only snapshot
during replacement startup found 48 coordinator databases and four other Cell
databases in the new session. A simultaneous stack sample reached
`takeover_restored` → `restore_exact` → `prepare_writable` → `load_checksums`,
including file and parent-directory synchronization. This identifies an observed
startup path, not a complete latency attribution. The failure therefore does not
require large payloads or secondary indexes. That run started before the final
test-only expression alias and failure-retention edits, so its temporary storage
was removed; logs and stack samples remain. The server binary fingerprint stayed
unchanged throughout. The full large-payload/index scenario remains a separate
required gate.

The full process test's recovery assertions now separate durability from token
replay. AWS defines a ten-minute window after the first request completes
([TransactWriteItems](https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_TransactWriteItems.html)).
The observed 53-minute workload cannot require deduplication of its oldest
conditional puts. It now checks historical payload and index state without
rewriting it, and commits a fresh two-Cell conditional put immediately before
the crash for replay immediately after readiness. Local/global index replay
checks remain in setup, inside the fresh-token phase. The earlier post-restart
rewrites could also conceal lost index or item state. Peer-owner coverage still
replays the large payloads across replacement. Strict Clippy for both process
and peer test targets passes in 20.12 seconds. The updated two-owner SDK test
passes in 247.49 seconds, including large-payload replay and replacement-owner
recovery. This corrects qualification semantics without changing the readiness
deadline or treating the existing standalone startup failure as fixed.

The dedicated BeyondDB SDK qualification workflow now includes both ignored
process scenarios and the peer-network suite on relevant pull requests, main
changes, and manual dispatch. It runs serially on Ubuntu 24.04 with the official
RustFS 1.0.0-rc.1 Linux archive pinned by SHA-256 and retains the SDK log on failure.
The archive contents and digest were verified locally, and Actionlint 1.7.11
(including shell checks) passes for this workflow. Linux execution is still
pending; adding the workflow does not establish an E2E pass.

The sampled checksum path exposed synchronous local filesystem work on an async
worker. A current-thread regression fails at the first filesystem existence
check before the fix. Writable checksum preparation now dispatches creation,
64-KiB buffered writes, durability barriers, and failure cleanup through the
existing bounded LTX host executor. Its cancellation contract retains admission
until dispatched work completes. The regression exercises more than 8,192 pages
with one executor slot, then opens the sparse writer and verifies stored data.
Injected write and synchronization failures retain their source errors and remove
the checksum sidecar. All nine preparation tests pass in 1.16 seconds; the
checksum-failure fencing test passes in 0.03 seconds, six sparse recovery cases
pass in 0.36 seconds, and the executor cancellation regression passes. This fixes
async-worker blocking; it does not establish the standalone readiness deadline
or eliminate serial coordinator recovery. Strict all-target Clippy passes for
LTX with `replica` in 14.39 seconds and for BeyondDB/runtime in 31.03 seconds;
format, layout, and policy-entry checks pass. Process qualification must be rerun.

The checksum-dispatch binary completed all 70 small-item transactions in
1,743.40 seconds. Its replacement exited with
`cell-coordination-tasks: Fenced` before readiness; the test failed in 1,759.85
seconds. The retained replacement session contained no SQLite databases. A
native stack sampler was attached during this attempt, so timing interference
must be isolated before attributing the fence to application recovery. The
failed storage fixture is retained. A startup-only replay on a clone of that
fixture, with the same binary and no sampler, became healthy in 34.47 seconds;
a signed AWS CLI Scan returned both items at version 69. The prior leases had
already expired by replay, so this does not prove the original hard-restart
deadline or classify the fencing failure. Linux qualification is running at
PR head `18c01469870`.

Linux qualification subsequently passed at that head in
[run 36293008636](https://github.com/crabbuild/crab/actions/runs/36293008636).
All four signed peer SDK cases passed in 408.95 seconds, including replacement
ownership and released/interrupted acquisition. Both standalone process tests
passed in 361.47 seconds total, including large payload/index recovery and the
70-shard history case beyond node residency. The latter committed its history
in 87.78 seconds. Both replacements satisfied the unchanged 45-second readiness
gate. This establishes those Linux scenarios, not an explanation of the local
fence or 10,000-Cell recovery. It predates the derived-file change below.

Activation also synced a freshly derived checksum base and a sparse or immutable
placeholder. Those files cannot acknowledge a Cell command: the published root
selects crash recovery, and clean local reuse separately writes a fresh synced
dense checksum sidecar and continuation, then verifies every reused page. The
activation path now omits these redundant barriers for both writable and
immutable views. SQLite WAL durability and warm-handoff synchronization remain.
Both new activation regressions failed on `sync_all` before the change. All 11
preparation tests pass afterward in 1.08 seconds; seven modeled crash tests pass
in 0.14 seconds, six sparse cases in 0.32 seconds, clean continuation across
process exit in 0.04 seconds, and checksum-write failure fencing in 0.08 seconds.
These tests do not establish physical power-loss behavior or a fleet latency gain.
Strict all-target Clippy passes for LTX with `replica` in 6.67 seconds and for
BeyondDB/runtime in 22.44 seconds. The rebuilt server recovered a fresh clone of
the retained 70-shard fixture in 25.91 seconds without a profiler; a signed Scan
returned both items at version 69. Its binary digest remained unchanged during
the replay. Prior leases had already expired, so this remains startup-only
evidence, not fresh hard-crash qualification or a controlled latency comparison.
Format, diff, Cell/LTX layout, and policy-entry checks pass.

Linux SDK qualification also passes with the derived-file change at
`3e6b9072ede` in [run 36294537538](https://github.com/crabbuild/crab/actions/runs/36294537538).
Four peer tests pass in 394.92 seconds and both standalone process cases in
278.51 seconds. The 70-shard case commits its history in 54.57 seconds and the
replacement becomes healthy in 18.31 seconds. This is a fresh hard-kill scenario
with the unchanged 45-second gate. These single-run timings are not a controlled
performance comparison or fleet recovery qualification.

### Measured node placement inputs

The binary now uses the signed capacity path described above instead of fixed
RAM/job hints and the full configured disk budget. A current-thread lease test
failed before dispatching its signer to a blocking worker. The renewal and slow
storage cases pass afterward in 11.02 seconds. Six capacity tests pass in 0.02
seconds with nested/disabled controllers, hidden ancestors, malformed or missing
measurements, over-limit usage, and live runtime disk/memory/job reservations.
The stalled-probe regression passes in 3.02 seconds: fencing finishes before the
probe is released, and its late result leaves advertisement generation one.
The existing stalled-storage shutdown/fencing case passes in 6.03 seconds.

A new real-server test bootstraps against RustFS, sends a signed SDK request,
loads the published node through `NodeDirectory` to verify its signatures, and
converts it into planner input. It observes admitted Cells and scratch bytes
subtracted from the configured budget. The initial macOS run passes in 5.15
seconds. After charging future reservations against OS-available bytes as well,
the six tests and strict all-target Clippy pass (10.09 seconds for Clippy).
The final process rerun also passes in 13.76 seconds. Its build took 7 minutes
4 seconds; a native sample found the compiler waiting in an archive-file write
on the mounted workspace volume. The existing build completed without restart.
Linux qualification now includes the capacity tests and this process case;
Linux measurement is not established by the macOS result. The added production
code owns host probing, cgroup parsing, and admission intersection; it supplies
measured inputs without introducing another scheduler or admission ledger.
The lockfile adds only BeyondDB edges to already locked `fs4` 0.13.1 and
`sysinfo` 0.38.4, with no package-version or source changes.


### Request-driven cold placement

Idle published data/GSI Cells use the runtime planner's signed capacity ranking
before restoration. Discovery accepts at most 1,024 live nodes and fails on
overflow; it never treats a partial scan as a fleet view. Missing placement
blocks are ineligible. Selected destinations still reserve runtime resources
and claim ownership through the existing control CAS. A failed destination
request does not silently place locally.

A separate `beyonddb.cell.activate` capability permits only Describe on data/GSI
Cells, over the existing pinned mTLS route. Session/fleet identity and operation
scope are checked before restoration. Ordinary invocation remains lookup-only
at the receiving peer. Activation cannot create catalog records, bootstrap a
root, steal a live owner, or acquire account/credential/coordinator Cells. If an
activation stops after claiming ownership, a later ingress resumes it on that
exact live session; expired-session takeover remains a separate fenced path.
Activation handling is bounded by the verified request deadline.

This connects capacity observations to real request routing. It does not
redistribute serving Cells, persist movement intents, or shard discovery and controller ownership. Those remain required for
the placement delivery gate and the 10,000-Cell/multi-TB target.

The signed SDK cold-placement scenario passes with SDK retries disabled. It
checks ordinary invocation against an idle Cell, invalid activation principals,
a query presented as activation, account activation denial, live-owner
protection, and remote recovery after a claimed-but-unopened boundary. It commits
a transaction across the two restored Cells, drains that owner, waits for its
lease to expire, and reads both changed items through the remaining node.
The final residency run passes all four SDK cases in 26.22 seconds, including
existing local restoration and released-participant transaction regressions.
Strict all-target BeyondDB Clippy passes in 32.54 seconds; format, layout,
policy-entry, and diff checks pass. These were local macOS results; Linux qualification subsequently passed at
`24d46a6b2a4` in [run 36296894016](https://github.com/crabbuild/crab/actions/runs/36296894016).
That run passed six capacity tests, five signed peer SDK tests (575.88 seconds),
and all three standalone process tests (293.97 seconds), including measured
Linux placement publication and fresh process-loss recovery. It predates the
routed capacity-controller change below. No fleet-scale throughput is claimed.


### Splitting ranges after remote placement

Capacity sweeps previously called local admission for every source and every
split child. An SDK regression with a remotely placed range reproduced
`Cell has another owner or is still activating` before the fix. The source had
a live valid owner; retrying local admission could not make progress.

The capacity controller now accepts the same routed `CellClient` as the serving
request path. Inspection, source export, completed-plan lookup, and split replay
reach current owners; a published child is never locally reacquired merely to
resume a split. Only missing child roots need bootstrap; the initial placement path below
selects their destination. The serving
binary passes its peer client into the supervised sweep. The implementation is
collected in `src/provision/capacity.rs`, replacing the local-handle composition
inside `provision.rs`. Metadata route CAS, sealed-source fingerprints, and
participant transaction barriers remain in their existing Cell commands.

This fixes automatic data-range growth after cold placement. Proactive ownership movement and distributed capacity
controller ownership remain unfinished.

The pre-fix remote-source regression fails in 1.95 seconds. After routing the
controller, the expanded SDK scenario passes in 16.11 seconds; the final version
starts the supervised loop and passes in 15.69 seconds with SDK retries disabled.
It verifies that the source stays remotely owned, moves an opened child remotely,
replays the completed plan without acquiring either remote owner, and reads both
transaction-updated items after owner removal. Local backpressure, numeric
Query/automatic split, and LSI mutation/transaction/split checks pass in 0.43,
2.90, and 1.09 seconds respectively. Strict all-target Clippy passes in 2 minutes
37 seconds. Format, layout, policy-entry, and diff checks pass. These timings
are functional evidence, not fleet performance qualification.
The standalone server build also passes (5 minutes 8 seconds); a live compiler
sample observed a directory read on the external build volume.


Linux qualification at `f9aecc0c9bc` passed in
[run 36298007432](https://github.com/crabbuild/crab/actions/runs/36298007432):
six capacity tests, five signed peer SDK tests (579.65 seconds), and three
standalone process tests (280.19 seconds). This includes the supervised remote
split regression. The 70-shard replacement became healthy in 20.87 seconds
against the unchanged 45-second gate. This run predates initial placement below.

### Initial range placement and unpublished-owner recovery

`BeyonddbPeers` owns one node identity and shared peer transport. The serving
binary binds it to the initial partition provisioner, the request client, and
the private listener. It owns no client or provisioner, avoiding reference cycles.
Embedded runtimes can still explicitly provision locally.

CreateTable provisions GSI and base ranges through the same placement boundary
as split-child bootstrap. It first persists the validated catalog identity,
then selects a destination from signed measured capacity. A distinct
`beyonddb.cell.provision` capability permits Describe only on cataloged data/GSI
Cells. The receiver derives the initializer from the compiled namespace and
checks the catalog's role, code, schema, and partition. It cannot catalog an
arbitrary requested Cell. Ordinary invocation remains lookup-only; cold
activation still requires a published root.

Placement bootstraps the root; routed commands install the range and perform
all subsequent work. A retry preserves published roots and live initial owners.
If bootstrap stopped after its ownership claim, the exact live session resumes.
If that session expired, a new destination must obtain the runtime's fenced
node takeover proof. Rootless claims use `takeover_unpublished`; published roots
use verified restoration, including roots not yet discoverable through table
routes. Authority CAS protects a racing publication and preserves the incarnation. Active failed-node logs still
require fleet recovery before takeover. A failed placement never falls back to
local bootstrap.

This removes the requesting-node affinity for initial ranges. Proactive
rebalancing, bounded distributed discovery/controllers, GSI range splitting,
and 10,000-Cell/multi-TB qualification remain open.


The initial-placement SDK regressions pass in 35.85 seconds with retries disabled.
One creates base/GSI ranges remotely, runs the serving projection worker, and
reads persisted base/index data after owner shutdown. The other resumes a live
initial claim, refuses to steal a still-live unavailable owner, fences an expired
rootless claim, and restores a populated root whose table route was never
published. Rootless and published-root recovery retain incarnation identity; the new range also
survives release and reacquisition. The scenarios each fit the fixture's
eight-Cell admission ceiling. Larger recovery demand is still rejected by the
unchanged resource gate.


The final local residency suite passes all six signed SDK scenarios in 56.83
seconds, including automatic remote-source splitting, authorization boundaries,
released/interrupted acquisition, and transaction participant restoration.
Embedded LSI mutation/transaction/split and admission-backpressure regressions
pass in 0.76 and 0.36 seconds. Strict all-target Clippy passes in 1 minute
10 seconds; the standalone server build passes in 29.47 seconds. Format,
Cell/LTX layout, policy-entry, and diff checks pass. These are functional local
results; current-head Linux and fleet-scale qualification remain separate gates.
Production Rust grows by 213 net lines for the shared peer composition,
scoped provisioning capability, and common range admission/recovery boundary.
No wire shape, storage schema, dependency, or configuration setting changed.

## Independent source-range splits

Pending data splits are keyed by `(table_id, source_partition_id)`. Each source
has one immutable pending plan; unrelated sources can copy and publish
concurrently. `ReadPartitionSplitPlan` is the point lookup used by explicit split,
threshold, and replay paths. `ReadSplitPlan` returns only the first pending
source in ID order for explicit table recovery. Account sweeps visit published
ranges in lower-bound order and use the participant lookup, which also finds
an unfinished plan through either published child.

Publication compares the exact source and child identities, bounds, table
snapshot, and partition epochs in the command's SQL transaction. A newer
directory epoch from an unrelated split no longer invalidates that comparison.
Each successful publication increments the current directory epoch, preserving
Scan page invalidation, while planned child epochs remain immutable. Publication
retains the source plan and all three reservations until verified child opening;
`FinishSplit` then removes only that plan and its members. Completed replay validates the exact children even
when subsequent unrelated publications have advanced the directory epoch.

The regression initially rejected the second disjoint source's plan. The signed
SDK test now persists both plans, releases and restores the metadata owner,
rejects a competing plan for the same source, publishes one plan, then replays
it concurrently with publication of the other. It verifies that the second
plan survives the first commit, the directory advances twice, stale page epochs
are rejected, and copied SDK items survive release/restoration of the metadata
and child owners. SDK retries are disabled for those recovered reads.

Ownership remains in `routing.rs` and `routing/split_state.rs`; callers are
`provision/capacity.rs` and `split.rs`. The runtime's existing application SQL
transaction/savepoint commits or rolls back the route rows, epoch, and plan
together. `DeleteTable` continues deleting all plans for the table and cascades
participant reservations. GSI routing has an equivalent lifecycle, described
in the automatic index growth section. Per-source prepare locks and
export/import fingerprints retain their existing guards.

Account metadata still uses one writer and a 512-MiB Cell budget.
Each account sweep attempts one range, advancing past transient failures.
Distributed scheduling and recursively sharded directories remain required.
The split-plan schema is unreleased and changed in place; development roots
must be reprovisioned. There is no legacy schema reader or upgrade claim.

These changes were verified on top of the tree merged by PR #469
(`311105eb864c`), which is byte-identical to the tested parent branch.
Seven signed SDK residency cases pass in 91.06s; the 1,025-range route-page
regression passes in 3.12s; LSI mutation/transaction/split passes in 0.72s;
admission-backpressure recovery passes in 0.36s; the data-range transaction,
split, and owner-restart regression passes in 1.52s. The latter retains both
wrong-source rejection (`PlanNotFound`) and same-source payload rejection
(`PlanMismatch`) under the new source-keyed lookup.

These are small fixtures. Fleet throughput and recovery remain unmeasured.
Production code grows by 65 net lines, including SQL, to add the source lookup
and replace table-wide exclusion. No dependency or configuration setting changed.
Strict all-target BeyondDB Clippy passes in 28.60s; the standalone server builds
in 51.83s. Format, Cell/LTX layout, policy entry points, and diff checks pass.
The prior build directory lost compiler files during verification; a fresh
checkout-specific target directory completed the clean build and tests.


## Capacity progress around unresolved source work

The account capacity sweep visits each published range in lower-bound order.
Its caller retains a mutable cursor that advances after range selection,
including when that range's inspection or split returns a transient error.
Metadata discovery failures retain the prior cursor. At the end of a table the
sweep proceeds to the next table, then starts another account pass. A pending
split's source remains published until atomic cutover, so its durable plan is
rediscovered on the next pass without a separate first-pending-plan priority.

`SealPartition` still refuses prepared transaction locks and pending GSI
projection journals. The split controller now maps those two typed rejections
to transient deferral. Invalid contracts and unexpected results retain their
existing error handling. Expected source work must neither stop the supervised
serving task nor prevent another range's independent split from progressing.

`reconcile_account_capacity` takes `&mut Option<CapacityCursor>` and now returns
a boolean indicating that a base or index split completed. The cursor carries
an optional index position as well as the range boundary. The redundant `CapacitySweep` result type
was removed; all workspace callers use the canonical cursor ownership. These
APIs are unreleased. The explicit one-shot table reconciliation method retains
its first-pending-plan behavior and reports a blocked attempt to its caller.

The signed SDK regression first timed out with both plans pending: a prepared
read intent on the first source stopped the capacity task. With only cursor
changes it still failed, exposing the fatal classification of the seal refusal.
After both fixes, the other range publishes while the original intent remains
Prepared and its source plan remains durable. Explicitly resolving that intent
allows a later pass to finish its split. SDK reads with retries disabled return
both original items, and node readiness remains healthy throughout.

Verification: eight signed SDK residency cases pass in 66.71s; numeric
Query/Scan, supervised splitting, and SDK restart coverage pass in 14.21s;
admission-backpressure recovery passes in 0.48s; data-range transaction/split
restart coverage passes in 3.01s. Strict all-target BeyondDB Clippy passes in
9.55s and the standalone server builds in 24.36s. This refactor removes 32 net
production Rust lines. It adds no storage, wire, dependency, or configuration
change. Distributed controller ownership, metadata sharding, and fleet-scale
throughput qualification remain open.


## Automatic index-range growth

The account sweep visits base ranges and then each GSI directory, applying the
same occupied-page threshold. GSI planning reserves the source and both children
in account-local indexed rows. A source or child can discover its pending plan
without scanning another range's history. Publication replaces exactly one
source, advances the current directory epoch, and retains the plan until both
children are open. Retention closes the crash window between route publication
and child availability. Participant reservations prevent overlapping transfers
and premature child splits; table deletion cascades through the plan rows.

The controller copies versions and tombstones through the fenced index lifecycle,
verifies fingerprints, publishes, opens, and finishes. Recovery can resume through
a published child. The existing base and new index planners share midpoint
calculation. Initial placement and foreground requests continue through the
existing peer admission boundary; no new configuration or dependency is added.

The new SDK test exposed a supervisor failure: an exhausted fleet returned
`LimitExceeded`, stopping its task and making the node unready. Capacity limits
now defer a selected range just like transient owner pressure. Explicit one-shot
calls still report the limit. The cursor advances and the durable plan remains;
adding a node lets the next pass place children remotely and finish the split.

Evidence: the regression failed before this classification change in 3.26s and
passed after it in 13.06s. It covers an eight-slot node, a second node joining,
independent plans across epoch changes, overlapping participant rejection,
administrative metadata updates, stale route pages, SDK Query/Scan after owner
restoration, tombstone replay protection, later projection key moves, and table
deletion with a pending split. The transfer test also restores during import and
after route publication before either child opens. These are bounded fixtures;
10,000 active Cells, multi-TB storage throughput, unclean fleet recovery, and
cutover latency qualification remain outstanding. An indivisible HASH group can
still exhaust one Cell; sort-key subranges remain necessary.


## Sealed-source residency after splits

A completed split previously left its source in the active SQL pool. With eight
slots and seven resident Cells, admission could not open two new split
children even though one resident source was no longer routed. Local pressure reclamation now covers base and GSI split sources
through the existing runtime release path.

A candidate must have a durable `Sealed` state and a successful current-account
lookup showing its exact partition ID absent from the published directory.
For base ranges, `Unrouted` is insufficient: the directory must exist and report
`Missing`. For both base and index sources, the pending participant reservation must also
be absent; it remains present until both children open. Table deletion still supplies its
independent immutable-generation proof. Serving and importing Cells are never
eligible solely because their route row is absent.

Initial table/index creation, transaction coordinator admission, and both split
controllers share this pressure path. Each unfinished split excludes its own source from reclamation.
Completed-plan replays perform no admission. The runtime rechecks the selected
local generation and settled work before publishing Idle. Object-store roots,
seals, tombstones, and transaction history remain recoverable. This does not
collect storage, proactively rebalance remote nodes, or guarantee admission against stale remote fleet advertisements. The local
Cell count now comes directly from the runtime admission ledger; other resource
observations retain their signed-advertisement gates. Recovery of historical sources still needs
available runtime capacity.

The signed SDK regression `sdk_split_sources_release_capacity_and_retain_recoverable_roots`
uses eight slots: SDK CreateTable/PutItem, base split, GSI projection and split,
then another SDK CreateTable. It checks that both obsolete sources lose owners
without losing roots, restores their durable seals, closes current children,
and reads base/index images through the SDK after restoration. SDK retries are
disabled. Before the fix, the second index child failed admission; the initial
regression passed after adding reclamation. Deleted-table and existing split
regressions cover the shared admission callers.

**Is this the best fix?** Extend the existing product proof and runtime release
boundary. No new eviction mechanism, schema, dependency, or capacity setting is
needed. Discovery is bounded by local residency; metadata sharding and distributed
rebalancing remain separate scaling gates. This increment adds about 80 net
production lines to identify obsolete sources and share reclamation with splits.


Verification on 2026-09-27:

- New signed SDK regression: PASS, 2.45s with retries disabled.
- All 11 `peer_network` SDK cases: PASS, 284.41s, including two-owner transaction
  recovery, remote placement, independent split replay, and GSI tombstones.
- Existing deleted-table residency and historical restoration: PASS, 5.33s.
- Strict all-target Clippy: PASS, 11.59s; server build: PASS, 21.18s.
- Format, diff, Cell/LTX layout, and policy entry-point checks: PASS.

These scoped results do not resolve the previously recorded intermittent
idle-owner test or establish fleet-scale qualification.


## Base split recovery through child opening

The base split previously deleted its intent at route publication. A crash before
`OpenPartition` left published children in `Activated`, while automatic sweeps
could see neither the old source nor its plan. The signed SDK fault regression
reproduced this: after all participant/account owners were released and restored,
four capacity steps left the published child unreadable.

`ddb_split_members` now reserves the source and both child IDs in the same
account command as the plan. `ReadPartitionSplitPlan` replaces the unreleased
source-only query and performs an indexed member lookup. Publication retains the
plan; repeated publication returns the same success without advancing the epoch
again. The controller accepts a matching pending plan on either side of cutover,
uses current-owner reads, verifies each durable open state, and calls
`FinishSplit` only afterward. Completion atomically deletes the plan and cascades
its memberships. A published child remains reserved against a nested split
until its parent finishes. Deleted tables cascade all unfinished memberships.

Account capacity sweeps therefore rediscover unfinished base work through a
published child even when its occupied pages are below the split threshold.
Source residency reclamation also requires absence of the pending reservation,
matching index behavior. Prepared transactions and undelivered index journals
still block source sealing; the source root and immutable import fingerprints
remain the recovery authority. A newer write to an already-open child is not
replaced when the controller replays the original sealed source.

Evidence map:

| Boundary | Proof |
| --- | --- |
| Caller | Account capacity sweep → partition lookup → host provisioner → `CellSplitController`. |
| Metadata owner | `routing/split_state.rs`: Begin/Commit/Finish and indexed participant reads; route/schema types remain in `routing.rs`. |
| Atomic callee | Runtime application savepoint commits plan/member/route mutations with the command result; rejected commands roll back. |
| Data owner | Install/import/activate/open keep exact source seals and frozen import summaries; ordinary writes stay fenced until opening. |
| Sibling paths | GSI already retains its plan through both opens; initial routes, source transaction guards, table deletion, and residency continue through their canonical paths. |
| Baseline | Main and the previous PR head consume base intent at CommitSplit and cannot rediscover a published unopened child. |
| SDK regression | Inject loss before the first or second child opens, release all participants/account, resume from a fresh sweep, preserve an intervening write, reject a nested plan until completion, then delete a table with a later pending plan. |

**Is this the best fix?** Retain discoverable intent through the last required
cross-Cell side effect. This follows the existing GSI protocol and avoids
heuristic scans of historical sources or opening children without verifying
imports. Split metadata handlers now live beside their state queries, bringing
both routing modules below 700 lines. The new table and query name are unreleased;
development roots require reprovisioning. No dependency, configuration, or
released-data compatibility path is added.


A completed replay now returns after matching the exact child directory and
observing that Finish removed the intent. This applies to both base and GSI
controllers: it does not reacquire historical sources, mutate children, or
consume another active slot. The eight-slot SDK regression checks that completed
base/index replays leave a released source idle before creating another table.
Unfinished plans still require their sealed exports and verified fingerprints.

The same capacity regression also exposed a stale local placement count: after
release published Idle, the last heartbeat could still advertise eight occupied
slots. Local placement now uses `CellRuntimeStats::placement_active_cells()` for
its own authenticated session. Signed lease identity and all other resource
eligibility gates remain mandatory; remote observations stay signed snapshots.
Runtime admission and owner CAS still authorize actual acquisition. This is a
local count refresh, not proactive fleet rebalancing.

The existing numeric Query/Scan regression now waits for Finish before checking
that a below-threshold child performs no split work. Its original assertions and
timeout remain; a published epoch alone no longer implies completed opening.
Net production growth for this increment is about 130 lines, including SQL and
local placement wiring; moving existing handlers adds no second code path.


Verification on 2026-09-27:

- Before intent retention: the injected publication/open crash stranded an
  `Activated` child after owner restoration and four capacity steps (2.54s).
- Final 11 SDK residency cases: PASS, 23.20s, including both interruption points,
  pending-member lookup, nested-split exclusion, table deletion, preserved newer
  writes, completed replay at full capacity, GSI tombstones, and remote recovery.
- Data/transaction/split owner recovery: PASS, 3.17s.
- Numeric Query/Scan and successive capacity splits: PASS, 3.10s.
- 1,025-range directory publication: PASS, 7.42s.
- LSI mutations/transactions/splits: PASS, 1.32s.
- Strict all-target Clippy: PASS, 9.38s; server build: PASS, 12.79s.
- Format, diff, Cell/LTX layout, and policy entry-point checks: PASS.

Earlier concurrent selections reported a five-second blocked-source timeout,
placement denials, a remote-placement assertion, and a transient GSI read. The
blocked-source case passed alone in 2.19s. The final selection above ran after
completed-replay and fresh local-count fixes; no test timeout or assertion was
relaxed. CI and fleet-scale qualification remain required. The older recorded
idle-owner regression was not included in this selection and remains unresolved.

### Resume ownership claims during configured and background recovery

Request routing already resumed a published root left in `Recovering` after
this boot session claimed ownership. Configured admission and background
discovery did not: `admit_initialized` rejected that root, and
`recover_discovered_owner` treated its own ownership as completed activation.
Both behaviors also exist in the inspected `origin/main` at `311105eb864`.
The initial regression run failed both cases: configured recovery returned
“Cell has another owner or is still activating”; discovered recovery returned
success with authority still `Recovering` and no serving actor.

Configured admission now restores both Idle published roots and published roots
claimed by this session through `activate_published`, shared with request
routing. Discovery checks the local handle before declaring recovery complete;
missing actors use canonical admission. Existing local actors avoid the admission
mutex. Live remote owners and expired-owner takeover keep their existing paths.
No coordinator decision, split state, account schema, or dependency changes.

| Evidence boundary | Source and behavior |
| --- | --- |
| Entry points | `src/bin/beyonddb.rs` configured startup, registered route/coordinator recovery, and the transaction/GSI background workers. |
| Product owner | `src/provision.rs::recover_discovered_owner` verifies an actor; `admit_initialized` distinguishes initial bootstrap from published restoration. |
| Shared mechanism | `src/provision/residency.rs::activate_published` restores while the caller holds admission; it tracks coordinator residency only after activation succeeds. |
| Runtime contract | `crab-cell-runtime/src/cell/actor/acquire.rs`: `local_handle` checks actual dispatcher residency; `acquire_idle_restored` reserves capacity before claiming; `activate_restored` validates local ownership and restores the authoritative LTX root. Lease, capacity, and authority checks remain in the runtime. |
| Siblings | Account, credential, base range, GSI range, and coordinator use this admission path. Peer request restoration shares the same helper. The ordinary peer dispatcher still refuses an inactive owner. |
| Regression | `tests/peer_network/residency/recovery.rs` commits SDK data, drains owners, publishes only their takeover claims, and invokes configured/discovered recovery. It verifies Serving authority and a local actor before SDK reads, including a restored GSI image. Coordinator discovery uses a local-only client so request routing cannot conceal a skipped activation. |

This increment closes an interrupted-activation recovery gap. It does not prove
fleet recovery throughput, rebalancing, a larger metadata budget, hot-key
partitioning, or the 10,000-active-Cell/multi-TB target. Ordinary restoration under
a full local pool still depends on coordinator reclamation or spare capacity;
retired-range reclamation remains on explicit provisioning/splitting paths.

Focused verification for this increment: 13 peer-network residency cases passed
in 23.20 seconds, including signed SDK reads, cross-Cell transactional writes
and reads, base/GSI split recovery, reclamation, and remote placement. The two
transaction admission-failure cases passed in 10.55 seconds; settled-coordinator
restart/checkpoint recovery passed in 1.28 seconds. Strict all-target BeyondDB
Clippy passed. The new regression module adds coverage for the interrupted claim
without production fault injection or weaker assertions/timeouts. Production
code grows by 38 net lines to share restoration and verify discovery readiness.
These are focused local results; the broader process qualification remains in CI.

### Bounded base-range usage measurement

`PartitionUsage` previously counted every item row and summed its stored BLOB
lengths on every capacity check. The same aggregate exists on inspected
`origin/main` at `311105eb864`. SQLite can obtain BLOB lengths without loading
the full payload, but the aggregate still traverses all rows. This made the
per-range capacity sweep grow with range item count.

A singleton `ddb_partition_usage` row now maintains the existing report's item
count and stored JSON/key bytes. AFTER INSERT, UPDATE, and DELETE triggers on
`ddb_partition_items` update that row within the item command. The UPDATE trigger
covers only counted columns, so TTL generation/backfill edits leave totals alone.
The usage query reads that singleton and the existing occupied-page PRAGMAs.
GSI capacity measurement already uses occupied-page metadata without an item scan;
account-local tables are outside the data-range capacity controller.

| Evidence boundary | Source and behavior |
| --- | --- |
| Entry point | Account capacity loop → `provision/capacity.rs::split_if_over_database_bytes` → `PartitionUsage`; report shape and physical split threshold remain unchanged. |
| Mutation owner | `partition.rs::write_item`/`delete_item` cover CRUD, partition transaction resolution, TTL deletes, and imported split images. No second adapter-specific accounting path. |
| Storage boundary | `partition_schema.sql` installs and seeds the singleton plus triggers. `item_storage.rs::StoredValue::write` allocates the final BLOB size before bounded content writes. |
| Runtime contract | `registry/handlers.rs::write_sql_blob` and `primitives/sql.rs::write_blob` require fixed-size BLOB allocation; `cell/executor.rs` rolls rejected application work back to its savepoint. Counter changes publish and roll back with those item changes. |
| Capacity reservation | The singleton occupies one preallocated page and has one bounded row. Its page is part of occupied storage before PREPARE; updates cannot grow its B-tree. Existing transaction headroom tests exercise both small/large COMMIT after other writers fill the Cell, refused PREPARE, and SQLite FULL upload rollback. |
| Regression | `tests/elastic_cells/usage.rs` compares maintained totals to an independent full aggregate through inserts, UPSERT, large BLOB allocation and chunk writes, savepoint rollback, TTL metadata changes, and repeated deletion. `tests/peer_network/residency/usage.rs` verifies SDK mutation, failed condition, transaction replay, split imports, and owner-restored totals. |

SQLite's [trigger semantics](https://www.sqlite.org/lang_createtrigger.html),
[fixed-size BLOB writes](https://www.sqlite.org/c3ref/blob_write.html), and
[BLOB length behavior](https://www.sqlite.org/lang_corefunc.html#length) match
this boundary. The singleton preserves the useful distinction between item
bytes and occupied database pages without reading all item rows. It adds one
metadata page and bounded accounting work to each changed row; write-throughput
impact at fleet scale remains unmeasured.

A local SQL microprobe using Python SQLite 3.53.4, the actual schema, and the
SELECT extracted from `PartitionUsage` counted VM instructions. At 100 rows,
the prior aggregate used 1,413 steps and the singleton used 10. At 10,000 rows,
they used 140,013 and 10 steps respectively, with identical totals. These are
SQL-shape measurements, not 10,000 Cells or a production throughput benchmark.
The Rust/runtime tests use the workspace's pinned rusqlite dependency.

Focused verification: raw SQL invariant test PASS (0.01s); signed SDK usage,
replay, split, and restart test PASS (1.29s); four transaction-capacity tests PASS
(46.11s); TTL/transaction-lock test PASS (1.11s); LSI mutation/transaction/split
test PASS (1.54s); numeric Query and automatic split test PASS (3.40s).
Strict all-target Clippy and the standalone server build passed. This adds 32
net production lines, retaining the existing report and removing its scan.

This changes the unreleased data-Cell schema. Existing development roots require
reprovisioning; no compatibility reader or dependency patch is introduced.
DescribeTable's DynamoDB size statistics remain a separate unfinished path;
these counters retain the internal stored-JSON/key-byte definition. Metadata
sharding, hot-key subdivision, fleet rebalancing, and 10,000-Cell/multi-TB
qualification remain required.
