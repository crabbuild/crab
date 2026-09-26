# BeyondDB elastic Cell topology

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
BEGIN and prepare now upload bounded 256-KiB binary pieces before the phase
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
loopback transport test. `build_peer_client` now binds the shared HTTP owner
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
The account directory can consume the exact durable plan and switch the route
with a predecessor compare-and-swap; owner restart retains the published route.
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
