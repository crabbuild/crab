# BeyondDB architecture and implementation status

BeyondDB is an in-progress DynamoDB-compatible service composed from ExtendDB's protocol,
validation, expression, authentication, and operation layers and Crab's Cell
application/runtime/host. ExtendDB is pinned to commit
`bdb7b3df4ace3b80a6e928f144036d056aec0327` in `Cargo.toml`.

## Request path

```text
AWS SDK / DynamoDB JSON client
  -> ExtendDB HTTP handler, SigV4 verification, IAM, validation, expressions
  -> ExtendDB StorageEngine and CatalogStore backed by BeyondDB
  -> Crab ApplicationHandle (one command/query per logical operation)
  -> Cell actor, SQLite transaction, LTX publication, object store
```

## Running the current server

The server writes warnings and errors to stderr.
Peer requests reserve memory while awaiting storage or Cell dispatch. CPU slots
cover envelope decoding, signature verification and reply encoding; asynchronous
work does not retain a codec slot. The signed request deadline bounds enrollment,
dispatch and reply encoding.
Serving nodes publish a 15-second lease and renew every three seconds.
`shutdown_serving_node` drains the runtime and joins heartbeat maintenance before
conditionally retiring the boot-session advertisement. A replacement can reuse
the physical node ID immediately after successful retirement. If a canceled
heartbeat commits after the final load, retirement reconciles only a newer
advertisement from the same signed boot identity. Unsealed logs and recovery
claims still reject withdrawal. Retirement reads and writes share one lease
lifetime as their deadline; errors retain scratch state and failed drain never
starts retirement. Unclean exit still requires authoritative expiry before
takeover. See [measured lease qualification](SCALING.md#graceful-session-retirement-and-immediate-restart).

Each renewal measures RAM and scratch-filesystem availability on a blocking
worker, caps them by runtime reservations, and signs Cell/job counts and backlog
pressure for the placement planner. Failed measurements and shutdown advertise
no placement capacity. A probe cannot extend the lease or publish after fencing.
Linux placement measurement requires a readable, complete cgroup-v2 memory
hierarchy; it checks ancestor limits and usage, including usage above a limit.
Cgroup-v1, hidden ancestors in a container namespace, and unsupported platforms
are ineligible instead of advertising host-wide RAM. Native macOS uses host RAM.
Configured owned Cells can still serve when placement measurement is unavailable.
Requests for idle, previously published data/GSI Cells now select a destination
from signed capacity and activate it over pinned mTLS. Missing eligible capacity
rejects placement. Account, credential, and coordinator residency keep their
existing policies. Placement evaluates heartbeat freshness after fleet discovery,
so storage latency does not make a renewed sample appear to come from the future.
Initial data, GSI, and split-child provisioning use the same
signed placement measurements with a separate authenticated provisioning
capability. A serving-node loop now uses the runtime's transfer planner to move
settled data/GSI owners to available capacity. It samples every 15 seconds,
retains the planner's residence and settlement gates, and releases at most two
ranges per pass. Account, credential and coordinator Cells retain their existing
ownership policies. Fleet load and failure qualification remain open; see
[range movement](SCALING.md#automatic-movement-of-settled-ranges).
When the local Cell pool is full, new range admission can release obsolete
base/GSI sources after current-owner metadata proves retirement. Restoration of
published owners and metadata admission can also release settled data/GSI owners,
preferring sealed sources, then live directory owners if no range can yield.
Restoration does not require the account to be resident. Runtime generation and
settled-work checks gate release; durable roots retain items, intents, directory
membership and unfinished transfers for later restoration.

`cargo run -p beyonddb --bin beyonddb -- config.json --bootstrap` starts one
leased Cell node, a private mTLS peer listener, and ExtendDB's public DynamoDB
listener. `--bootstrap` reads one access-key secret from stdin, stores it
encrypted in a credential Cell, and commits the configured inline user policy.
Run without `--bootstrap` after the first successful start. The encryption key
file contains exactly 32 raw bytes and must be retained across restarts.

```json
{
  "storage_url": "s3://my-bucket/beyonddb",
  "node_id": "01994f26-5966-7b20-8b58-2fddf198a321",
  "data_dir": "/srv/beyonddb/scratch",
  "disk_budget_bytes": 107374182400,
  "encryption_key_file": "/etc/beyonddb/encryption.key",
  "region": "us-east-1",
  "peer_bind": "0.0.0.0:9001",
  "peer_endpoint": "https://node.example.com:9001",
  "peer_certificate": "/etc/beyonddb/peer.crt",
  "peer_private_key": "/etc/beyonddb/peer.key",
  "peer_ca": "/etc/beyonddb/peer-ca.crt",
  "peer_server_name": "node.example.com",
  "public_bind": "0.0.0.0:8000",
  "public_endpoint": "https://ddb.example.com:8000",
  "public_certificate": "/etc/beyonddb/public.crt",
  "public_private_key": "/etc/beyonddb/public.key",
  "owned_accounts": ["123456789012"],
  "owned_access_keys": ["AKIAIOSFODNN7EXAMPLE"],
  "initial_partitions": 4,
  "split_threshold_bytes": 268435456,
  "bootstrap": {
    "account_id": "123456789012",
    "access_key_id": "AKIAIOSFODNN7EXAMPLE",
    "principal_name": "operator",
    "policy_name": "tables",
    "policy_file": "/etc/beyonddb/operator-policy.json"
  }
}
```

The peer certificate must be an Ed25519 leaf trusted by `peer_ca`, valid for
both client and server authentication, and cover `peer_server_name`. All nodes
in one fleet use the same CA, storage root, compiled release, and encryption
key. The public listener requires TLS unless bound to loopback. The object
store must support strict create and conditional updates; local `file://`
storage does not provide the required Cell authority semantics. A new node
session uses a fresh scratch directory; graceful shutdown removes it after
Cell drain. Crashed sessions may leave scratch directories for operator cleanup.

The process smoke test starts RustFS, bootstraps a key, sends AWS SDK table and
item requests, kills the server without draining it, then restarts it at a new
peer address and reads the committed item after lease expiry and fenced Cell
takeover. It also verifies
BatchWriteItem, BatchGetItem, paginated parallel Scan, and TTL expiry across four
initial data Cells before the crash. It checks batch reads and TTL configuration
after recovery, then disables TTL and verifies that state through another
restart. Run it in a
dedicated environment with Docker, `aws`, and `openssl` available. The fixture
starts a digest-pinned RustFS 1.0 GA container with an isolated Docker volume
and a random loopback port; no native RustFS installation is used. Colima users
can select their daemon with `DOCKER_CONTEXT=colima`.

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-beyonddb \
  cargo test -p beyonddb --test server_binary -- --ignored --test-threads=1
```

The `settled_history_beyond_residency` process test isolates coordinator recovery:
70 distinct coordinator shards update two small items in separate data Cells,
then the server is killed and restarted at another peer address. It uses the
same 45-second readiness gate as the full smoke test. On a readiness failure,
it retains the fixture and prints its path for startup-only replay. A failed
process test stops but retains its RustFS container and volume, printing the
container name; successful tests remove both. This test
does not cover large payloads or index restoration; both remain in the full
scenario. See [scaling qualification](SCALING.md) for actual results and open gates.

The dedicated [SDK qualification workflow](../../.github/workflows/beyonddb-qualification.yml)
selects the native elastic-Cell, peer-network SDK and process suites on
relevant pull requests and main changes. It uses Ubuntu 24.04 and the same
digest-pinned RustFS GA container, runs tests serially, and retains the test log on failure.
Ordinary `cargo test` does not run the ignored process tests.

This server uses an explicit list of locally owned account and credential
Cells. On startup it recovers configured account and credential Cells, then
pages base/index directory leaves and the account coordinator registry. Idle or expired owners can
be recovered at a new peer endpoint; live remote owners remain in place. Every
takeover still requires node-session fencing and a Cell authority CAS. Recovery
also resumes a published root already claimed by this boot session if activation
was interrupted. Discovery verifies a local actor exists before reporting the
Cell recovered; configured admission and request routing share root restoration.
This requires the account to be configured on the replacement and enough local
capacity for its recovered ranges. While serving, it also discovers expired
coordinator owners through configured accounts and restores their original
participants. Requests also select capacity and recover expired data, index and
directory owners from their published roots, without waiting for transaction or
index maintenance to discover them. Unreachable owners with live leases remain
fenced against takeover. Background recovery of unaccessed data-only nodes still
needs a recovery scheduler.
Distributed recovery scheduling, fleet qualification,
management APIs, and the remaining DynamoDB operations are still required
before this is a complete service. A public node with no locally owned account
or credential Cells can forward signed requests to live owners through mTLS.

The account Cell, independently owned data-range Cells, and a partial ExtendDB
`StorageEngine` adapter are implemented today. Table and item operations have
Cell paths; most remaining traits return explicit unsupported errors. TTL settings
are committed in the account Cell. The serving binary sweeps enabled tables,
configures a fixed expiry index in each routed data Cell, backfills old items in
bounded commands, and conditionally deletes expired items. Each tick processes
at most one 64-Cell route page per table and 16 tables per account. An owner
restart restores the settings, sweep and backfill cursors, and index state. The global TTL listing
trait remains unsupported; the worker lists tables by locally owned account.
UpdateTimeToLive primes at most one route page and returns while the worker
reconciles the rest; disabling stops expiry sweeps without dropping the fixed
data Cell index.
TTL candidate selection skips shared and exclusive transaction locks. If a
prepare races selection, the conditional delete defers that item while the
sweep continues; later passes revisit it after transaction resolution.
The account capacity worker resumes incomplete table creation from its durable
catalog row, preserving any published GSI directories before publishing the base
route. It uses the same provisioning path as CreateTable and can continue after
account-owner restoration without a client retry. One creation attempt can install
all remaining initial ranges; this is not a one-Cell-per-tick operation. The global
table-transition hook has no account inventory and remains a no-op.
Table resource tags now have Cell-backed CreateTable, TagResource, UntagResource,
and ListTagsOfResource paths; DeleteTable fences the generation and removes their
rows in bounded cleanup batches. Large deletions remain DELETING until the
account capacity worker finishes; progress survives account-owner replacement.
The name remains reserved until catalog cleanup completes. This does not reclaim
the retired data/index Cells' object-store history. The RustFS
process test verifies these requests through the AWS SDK across a server
restart and verifies that a recreated table starts without the old tags.
TransactWriteItems accepts Put, Delete, Update, and ConditionCheck across
account and data Cells. Every public write transaction uses one durable
coordinator for admission, token lookup, prepare, decision, and resolution.
Matching tokens find the original participant set before consulting current
routes. Successful tokens replay for ten minutes after all participants
resolve; canceled tokens are released only after every abort resolves.
The signed SDK process test writes two primary keys in distinct data Cells,
checks replay, mismatch and rollback, then verifies replay and both values
after a hard kill and restart. Cross-Cell TransactGetItems uses shared key
locks and durable captured images; the same process test checks projected
results and missing items before and after restart. Get, Query, and Scan can
finish a blocking transaction's durable COMMIT or ABORT and repeat their read.
Once a terminal decision is known, resolution attempts every participant even
if an earlier participant or receipt fails, so reachable Cells can release their
locks. It keeps up to four attempts in flight per call and replenishes that
window as attempts finish, allowing progress while one owner remains slow.
The overall request stays incomplete until all receipts are durable; errors
remain retryable. Cancellation leaves durable progress for the next resolver.
Each Cell query helps at most one transaction; BEGIN and unavailable decisions
remain retryable conflicts. Transactional reads retain conflict cancellation.
Once a table's initial route is published, keyed CRUD and Scan use its data
Cells. Placement is committed with the table generation: account-local tables
use the account Cell, while routed tables remain CREATING until publication,
including for clients without provisioning capability. The host-backed
provisioner installs 1–256 initial data Cells per table during CreateTable and
resumes using the persisted count after an interrupted setup. Configuration
changes affect new generations only. UpdateTable rejects incomplete routed
creation atomically, preserving the specification already installed in owners. A host-backed controller can resume a recorded split. A cancellable
account capacity loop can trigger a split, and the serving binary starts that
loop for locally owned accounts. It visits base ranges and then GSI ranges in
order, advancing past transient failures and capacity refusals. GSI plans remain
discoverable through their source and children until both replacements open;
adding a node can resume a pending split onto remote owners. Prepared
transactions and pending index projections defer sealing without stopping the
serving task. Inspection and split replay use the routed client, so published sources and children can stay on remote owners. New table ranges, GSI ranges, and split
children use signed fleet placement in the serving binary. There is no merge controller or complete
`StorageEngine`/`CatalogStore` behavior, or account-management service yet.
`build_http_state` now assembles ExtendDB's signed request path from a ready,
leased Cell node. ExtendDB requires a `CatalogStore` even for DynamoDB request
authorization. BeyondDB supplies a Cell-backed catalog for authorization reads;
management, admin login, settings, and metrics methods return explicit errors
until their Cell implementations exist.
Long-lived SigV4 credentials now live in 256 key-derived Cells. A server-held
AES-256 key encrypts secrets before Cell commands; temporary sessions are
explicitly unsupported until their token and expiry contract is implemented.
Revocation is a durable Cell command; an inactive key is rejected by ExtendDB
authentication and stays inactive after owner recovery.
The provisioner can inspect a table's active data Cells, resume a pending split,
and split one range whose occupied SQLite pages cross a caller-supplied
threshold. That measurement includes indexes and runtime tables, but excludes
WAL and LTX files. Base-range item counts and stored JSON/key byte totals are
maintained atomically with item changes, so usage queries read one metadata row
instead of traversing all items. The account loop checks one table range per tick and can
trigger one split per tick. The provisioner can install it in the node's task
group, where failure closes readiness and shutdown cancels it before Cell drain.
The serving binary installs it for every locally admitted account. The loop
still needs disk and capture-pressure inputs before a Cell reaches its
admission limit.
One request-path gate now passes: a signed HTTP request reaches a durable Cell
write and survives owner restart in a loopback integration test using ExtendDB's
HTTP server and an AWS DynamoDB SDK client. The credential is encrypted and
committed in its own Cell before the request; the test verifies its ciphertext
does not contain the secret and reopens it after owner restart. The test uses
the Cell catalog to satisfy ExtendDB's server wiring. Developer authorization
is disabled: the signed request is evaluated
against an inline user policy stored in the account Cell. The test verifies
denial without a policy, denial for an unlisted table, immediate denial after
policy removal, and policy recovery after owner restart. Group, role, boundary,
and tag policy storage, credential provisioning, Cell-backed management
operations remain unfinished. Until policy-cache
invalidation is wired, serving composition must use ExtendDB's pass-through
authorization cache so removal takes effect immediately.
The same SDK client creates another table and writes through its newly admitted
data Cell without rebuilding the server's routed client. The HTTP state builder
accepts a caller-supplied `CellClient`. The signed SDK test supplies
`CellClient::runtime_with_peer` from a separate runtime; signed requests reach
account, credential, and data Cells through an authenticated loopback peer
round trip. `BeyonddbPeers::router` authenticates incoming requests against live node
advertisements, restricts targets to BeyondDB namespaces, and dispatches ordinary
operations only to the current local owner. A separate Describe-only capability
can activate published idle data/GSI Cells. A distinct provisioning capability
bootstraps cataloged data/GSI ranges; it cannot create catalog entries or acquire
account/credential/coordinator Cells. `BeyonddbPeers::client` binds the owner-resolving HTTP
transport and a fleet-scoped principal to account, credential, and data Cells.
It takes the matching node provisioner to restore cataloged ownerless Cells on
demand. An interrupted ownership claim by the same boot session resumes from
its published root; existing remote owners retain authority. SDK regressions
release data, account, and credential Cells and read the persisted item again,
including the claimed-but-not-yet-restored state. Data/GSI restoration selects
a destination from signed capacity; account, credential, and coordinator
restoration retains its existing policy. The range rebalancer uses the same
authenticated activation path after generation-fenced source release.
`tests/peer_network.rs` uses separate mTLS identities on two leased nodes,
denies a wrong peer principal, and sends signed AWS SDK CreateTable, PutItem,
and GetItem requests through ExtendDB's public listener and the private peer
listeners. The public node provisions and owns the data Cell; the other node
owns the account and credential Cells. After that owner's lease expires, a
replacement fences its session and discovers expired ranges at a new endpoint.
Startup coordinator discovery also finishes a COMMIT left after one participant
apply and before its receipt. Both Cells restore from object storage.
Both public endpoints then read the committed item, including a read that
forwards to the data owner. The replacement refuses data takeover while that
owner is live, then fences its expired node session after lease renewal stops,
restores the data Cell from object storage, and reads the item again. General
unattended takeover and fleet qualification remain unfinished.

## Cell ownership

The ExtendDB adapter maps an account ID to one deterministic SQL Cell. That
Cell owns table key definitions and, until route activation, the table's items.
Route activation rejects a table with account-local items, and the old account
item commands are fenced after activation. A Cell command is the atomic
boundary. A rejected command rolls back its writes; a successful response is
returned only after LTX publication.

The data Cell module separately persists one table partition-key hash range per
Cell. Sort-key siblings have the same owner.
Its install contract binds the table ID, partition ID, range and Cell epoch;
keyed reads and writes reject a wrong range or stale epoch. The account Cell
can publish a durable initial route after the provisioner installs its data
Cells. The directory version can advance while unaffected data Cells retain
their own epochs. A durable split plan must replace one range with two fresh,
contiguous child ranges while leaving every other range unchanged. Plans are
owned by source range: independent splits can remain pending and publish
concurrently. Each publication advances its leaf version without changing
unrelated source or planned child epochs. Route validation requires complete,
nonoverlapping hash coverage and the table's immutable key schema. Route
publication currently trusts the provisioner to have installed and published
the data Cells; it does not verify
their receipts. The source data Cell can persist an idempotent split seal that
fences ordinary reads and writes, then serves bounded export pages after owner
restart. Import-only child Cells accept idempotent item copies and verify an
expected count and digest before activation. Activation closes imports while
keeping ordinary requests fenced. The host-backed split controller admits
children, seals the source, copies bounded export pages, checks both child
fingerprints against the sealed source, and atomically publishes the exact
durable plan against its exact predecessor source range. It then opens the
children for ordinary requests, verifies their durable open states, then
finishes the plan. Source and child reservations remain discoverable through
publication, so a restarted capacity sweep can resume from either published
child. An opened child cannot start a new split until its parent plan finishes.
The host provisioner can choose a range
midpoint, record the plan, and repeat the split on an already opened child. Repeated calls
resume this sequence after interruption.
The directory command cannot inspect other Cells; serving code must use
the controller rather than calling route publication directly.
`DescribeTable` reports periodically sampled item counts and logical item bytes
for the table and its indexes. Each mutation maintains local counters; the server
samples one range per tick and publishes a completed snapshot only if its table
and route generations still match. Published totals survive account-owner restart.
The values can lag writes and index projection, and do not measure billed storage
or SQLite/object-store usage. See [statistics semantics and proof](SCALING.md#table-and-index-statistics).

Routed keyed CRUD and Scan use independently owned base and GSI directory trees.
The account retains a publication anchor for each generation; bounded leaves own
range membership and full split plans. Point requests follow one tree path.
Scan reads at most 64 ranges per page, validates the previous leaf's version,
and adopts the neighbouring leaf's version when crossing its immutable boundary.
Query reads the HASH key's owner through its local ordered RANGE-key index,
including numeric sort keys and continuation.

Public transactional writes use durable coordinator decisions and idempotent
participant resolution. Transactional reads within one Cell use one snapshot;
cross-Cell reads capture images under shared locks. An admitted transaction
retains its original coordinator and participants across directory changes.
Base and GSI splits advance only the source's epoch and the owning leaf's
membership version. Unfinished plans prevent metadata movement until both
children open.

The base directory cutover is under verification. Focused signed SDK split,
restore, pagination and replay tests pass. Native fixtures compile and the
data-range owner-restart check passes; broader runtime and CI gates remain
incomplete. The account catalog and publication anchors still share
a 512 MiB Cell and one writer. Hot partition-key groups, coordinator/history
bounds and 10,000-Cell/multi-TB fleet qualification remain open. See
[metadata ownership](METADATA_SHARDING.md) and [scaling requirements](SCALING.md).
Transaction request/intent payloads use bounded SQL chunks while remaining in
one local command. Item and saved-read images also use bounded BLOB transfers,
including JSON images larger than 1 MiB after escaping. The signed SDK fixtures
cover multi-MiB transactions and replay across owner/process restart. Aggregate
Update accounting needs cloud-reference qualification: DynamoDB Local accepts
small Updates over more than 4 MiB of stored images. The protocol document and
`scripts/probe-transaction-size.py` record this distinction. Transactions
use bounded binary uploads for BEGIN and prepare inputs larger than one Cell
RPC, plus bounded recovery queries. Temporary uploads are capped and expire;
HTTP body limits, WAL/disk/memory headroom, and history collection remain separate
gaps. Prepare now reserves SQLite page capacity for resolution; unrelated
commands and runtime receipts cannot spend that claim. The conservative bound
reduces admitted transaction concurrency and still needs scale qualification.

The [cross-Cell transaction protocol](CROSS_CELL_TRANSACTIONS.md) specifies
the decision, lock, visibility, and failure-recovery contract.
Start with its [foundation assessment](CROSS_CELL_TRANSACTIONS.md#foundation-assessment-for-multiple-primary-keys)
for a three-key transfer, reader isolation, the driver trust boundary, and
the remaining 10,000-Cell qualification gates.
Account and data Cells persist prepared write images, immutable read images,
and exclusive write/shared read locks using one participant state machine.
Ordinary reads respect write locks; writes respect both lock modes. Data Cells refuse split sealing; account Cells refuse
table deletion and route activation while prepared intents remain. Sharded coordinator Cells can durably
record a participant set, prepare receipts, one commit or abort decision, and
resolution progress. The public write driver resumes published BEGIN records from
stored account/data participant payloads, handles prepare/decision ambiguity, and returns
only after participant resolution. A proven capacity refusal during participant
upload or prepare proposes ABORT and returns ordered `ThrottlingError` reasons
only after cleanup. This includes direct SQLite FULL errors with verified rollback;
unknown outcomes remain retryable and a competing COMMIT wins.
Recording a terminal participant resolution releases its coordinator operation
images for writes and aborted reads. Durable decisions, request digests, targets,
receipts and replay markers remain. Committed read operations stay available for
response assembly. Once all results are assembled, the reader durably acknowledges
consumption. The existing recovery loop deletes saved images in each original
participant Cell, then compacts its coordinator mapping after a durable receipt.
A driver whose chunk fetch races compaction re-reads and finishes the durable
decision. Unacknowledged read images, decisions, tombstones, and object-store
history still require safe retention and collection.
Coordinator token lookup preserves original
participants across route changes and starts the ten-minute replay window only
after all participants resolve. The old account claims and Cell-local token
receipts have been removed. Fenced startup
recovery discovers registered coordinators and their immutable participant owners, then resolves unfinished work before
public traffic starts. Keyed reads, Query, Scan, and same-Cell transactional
reads now reject unresolved intents; range checks include pending creates.
A mixed-participant host test restores account, data, and coordinator owners
and finishes a pending transaction with concurrent drivers. The Cell barriers
fail closed; the adapter helps resolve a blocking terminal decision before
retrying Get, Query, or Scan. An undecided or unavailable coordinator remains
a retryable error.
A supervised serving worker rotates through locally admitted coordinators and
discovers one registered shard per tick across configured accounts. It fences
expired coordinator/participant owners, resumes abandoned BEGIN records, and
finishes terminal decisions. It processes at most one pending transaction per
tick, advances past failures, and revisits them on a bounded pass. Discovery
uses an account-registry hint to skip a settled Idle root only when its root
digest, incarnation, ownership epoch, code, and schema still match. The hint
survives restart and is shared with startup recovery; an active owner, recovery
overlay, missing hint, or changed root takes the ordinary recovery path. Cell admission reclaims settled
coordinators at the active-Cell limit; idle shards restore before token lookup.
When published owners outnumber resident slots, restoration can release one
settled base or GSI owner and later restore its exact durable root. Metadata
admission, including a transaction's first coordinator, has the same allowance.
New data owners still require free capacity;
placement can choose another node instead of evicting live ranges to bootstrap.
Recovery reactivates released coordinators; startup resolves shards one at a
time. A participant admission failure no longer prevents a published decision
from resolving healthy Cells. Startup continues through the shard's pending
records but retains errors and fails readiness until recovery completes. Serving
recovery keeps undecided transactions behind successful admission. Distributed
recovery scheduling, bounded transaction/read-image retention, and
fleet qualification remain incomplete. Production admits 64
active Cells per node; busy coordinators apply retryable backpressure. See
SCALING.md for the unqualified 10,000-Cell, multi-TB target.

HTTP Cell work uses bounded FIFO mailbox admission per Cell, with independent
queues for different Cells. The client shares a 128-call/32-MiB encoded-input
budget and permits 50 seconds of admission waiting per transport stage. Unknown
write outcomes still require resolution. See [concurrent admission evidence and
remaining overload limits](SCALING.md#concurrent-request-admission).

The signed SDK host test uses `CellNodeBuilder::build`, a published node
advertisement, a renewing lease guard, and a task group. The lease task keeps
the authoritative advertisement fresh and fences the node on terminal failure;
normal task cancellation leaves the guard live while the node drains. Other offline host
tests use `build_unleased_for_maintenance`. The serving binary renews its
node lease through this task, supervises its tasks, and passes the host readiness gate before
accepting requests. It must route to the current owner or perform a fenced takeover;
bootstrapping another writer for an existing Cell is not valid.
The provisioner also admits account and credential Cells, including acquisition
from an idle published root after restart. The signed SDK test exercises both
initial admission and owner recovery through those paths.

## Streams dependency contract

The closed-shard completion proposal and full Streams implementation boundaries
are in [STREAMS_CONTRACT.md](STREAMS_CONTRACT.md). Its accompanying patch is
unapplied and awaits explicit dependency-change approval. Public Streams
operations remain unsupported.

## API coverage boundary

BeyondDB is not a complete DynamoDB replacement. In addition to the index,
Streams, backup/PITR, IAM, and scale gaps described here, the pinned ExtendDB
engine does not dispatch PartiQL or Global Tables operations. Its import/export
handlers use local filesystem extensions rather than the DynamoDB S3 workflow
([ImportTable](https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_ImportTable.html),
[ExportTableToPointInTime](https://docs.aws.amazon.com/amazondynamodb/latest/APIReference/API_ExportTableToPointInTime.html)).
BeyondDB supplies empty import/export path lists in `src/server.rs`, so those
handlers reject requests before file access. Enabling filesystem paths would
not establish DynamoDB import/export compatibility. These operations need
upstream protocol support and Cell-backed orchestration, followed by signed SDK
and restart qualification, before they can be listed as supported.

## ExtendDB contract to implement

Use ExtendDB's existing `extenddb-server` and engine. The backend must supply
all six `StorageEngine` traits (`TableEngine`, `DataEngine`, `MetadataEngine`,
`StreamEngine`, `BackupEngine`, `WorkerStore`), plus `CatalogStore` and its
`CredentialStore`. The server must use the same Cell-backed state for HTTP,
workers, backup/restore, and management. There must be no separate SQLite or
Postgres write path for those operations.

The backend needs these invariants:

- Treat ExtendDB's `TableKeyInfo` and validated expression AST as the request
  contract; evaluate conditions and updates inside the Cell command against
  the item version that is being changed.
- Keep table metadata, base items, secondary-index entries, TTL state, and
  stream records in the same atomic Cell command where those features require
  it. Generate stream sequence numbers from durable Cell state.
- Keep account and resource authorization scoped to authenticated identity.
  Credential lookup requires a global access-key index; secrets must never be
  logged or stored in plaintext.
- Implement pagination against a stable ordering and use opaque, validated
  continuation keys. Check item, request, transaction, and Cell wire limits
  before dispatch.
- Derive backup points from published Cell snapshots. A backup spanning data
  Cells needs a coordinated cut; independent snapshots are not a consistent
  table backup.
- Make retries deterministic. ExtendDB's transaction client token is scoped
  by account and its fingerprint must distinguish a replay from a different
  request using the same token.

## Current verified slice

`src/lib.rs` registers an account Cell with table create/describe/list/update/
delete, keyed put/get/update/delete, transactional put/delete, and transactional
get operations and initial table route publication. It also registers
independently addressable data Cells with partition install, keyed CRUD and
bounded Scan. The `CellStorage` adapter implements the matching base-table
`TableEngine` and `DataEngine` methods. Conditional single-item writes and
update expressions execute inside the Cell transaction. Base-table Scan uses
bounded pages with stable continuation keys. Parallel Scan assigns contiguous
hash intervals to segments and skips data Cell ranges outside each interval;
cells crossing an interval boundary still scan and filter their items. Query
supports hash-only tables
and sort-key tables once their initial data route is published.
Low-level account and partition commands support local atomic transactions.
The public adapter routes all transactional writes through the coordinator,
including account participants before route activation. All transactional reads
use durable shared locks and captured participant images, retrieved individually
to avoid an aggregate Cell response limit. Same-Cell reads now pay the same
coordinator protocol cost. Account-local
base-table sort-key Query, online global-index changes, non-ALL local index projections,
and streamed writes remain unsupported. Local secondary indexes with ALL
projection support account/routed Query and Scan, strong reads, numeric sort
ordering with base-sort tie-breakers, and base-plus-index continuation keys.
Sparse entries are absent until their index key exists. Ordinary writes,
transactions, TTL deletion, and split import maintain index entries in the
same Cell command as base items. Indexed Query fences the HASH group's prepared
write intents, including absent creates and sort-key moves; account-local Query
conservatively fences the table. See [the remaining LSI read contract](LSI_CONTRACT.md)
for KEYS_ONLY/INCLUDE, base-fetch capacity accounting, and scale limits.
Global indexes created with a table have independently owned ranges, durable
asynchronous maintenance, and ALL/KEYS_ONLY/INCLUDE projected Query and Scan.
See [global-index implementation and limits](GLOBAL_INDEXES.md) for journal
replay, transaction boundaries, restart evidence, and unfinished index splitting
and collection.

Data Cells also provide an internal foundation for online index backfill:
revisioned write policies, bounded durable scan cursors, a historical-journal
delivery barrier, and inheritance of unfinished work through splits. Policy
changes wait for prepared transactions to resolve, preserving their capacity
reservations. Native Cell tests cover restart, invalid historical keys, delivery
tracking, and split inheritance. Account lifecycle orchestration and SDK
`UpdateTable` create/delete remain unimplemented; these commands do not make
online index changes a supported API. The foundation extends the unreleased
initial SQL schema; upgrading roots written by earlier binaries is unqualified.

`tests/account_cell.rs` exercises them through a real
`CellNodeBuilder` and in-memory object store, including request replay,
receipt-based reads, conditional writes, update expressions, scan pagination, rollback of a
multi-table write, and restoration from the published object-store root on a
new host. `tests/elastic_cells.rs` exercises two distinct data Cells through a
real host, range and epoch rejection, route validation and publication,
partitioned adapter CRUD, Scan, same-Cell and cross-Cell writes, rollback and
cross-Cell snapshots, shared-read/write conflicts, account write fencing, and restoration of
both a data Cell and the route's account Cell from object storage. These tests
do not prove fleet-scale repartitioning, complete IAM, every index projection,
Streams, or backups. The
elastic test also verifies
sealed-source export, two import-only children, activation by source-derived
fingerprint, a delayed-copy fence, host-backed split resumption, atomic route
switch, adapter reads through the replacement children, and child recovery
after owner restart. Data Cells
expose their durable partition contract and lifecycle state so a trusted
controller can check split readiness across Cells before publication. The
provisioning test retries after installing two data Cells but before route
activation, writes more than 64 MiB of item payload across both ranges, and
reacquires both Cells after restart with the same measured payload bytes.
The 65-range host test checks TTL expiry in its first and last data Cells over
two bounded sweep ticks, deferred index setup during enable, disabled expiry,
and account-level cursor advance across 17 TTL tables.
The numeric Query host test checks ordered pages, reverse order, a sort-key
predicate, equivalent numeric key spellings, and two successive splits, the
first triggered by the account capacity loop, while preserving Query results
through the Cell adapter. It also sends
ExtendDB engine wire-format PutItem and GetItem requests through the Cell
backend, then restores the item owner from object storage and reads the exact
written image. The same test also sends signed SDK table creation,
DescribeTable, PutItem, GetItem, conditional UpdateItem/DeleteItem, and paginated
Query/Scan requests through ExtendDB's HTTP server, rejects an incorrectly
signed request, and verifies SDK-written items from both an existing table and
a newly created table after owner restart. It exercises Cell-backed inline user
authorization but not a production management catalog. The SDK key is read
from a credential Cell; revocation denies a signed request immediately and
persists through recovery, and a wrong decryption key fails closed.

The local-index integration fixture covers five ALL indexes on account and data
Cells, tied numeric keys, forward/reverse and scan pagination, sparse removal,
invalid/oversized images, mixed-participant commit/replay, prepared create/delete/
sort-key-move read barriers, COMMIT/ABORT read helping, and split import. The
server-process fixture additionally exercises signed SDK index metadata, Query
projection/ranges, parallel Scan, transactional changes, and replay after an
unclean restart. These cases do not qualify the full upstream protocol suite.

## Acceptance proof for a server claim

Run ExtendDB's protocol suite against the BeyondDB endpoint, then exercise
an AWS SDK against the same endpoint. Include table/item CRUD, query/scan,
secondary indexes, batch and transactional operations, expressions, streams,
TTL, backups, auth failures, pagination, and error responses. For durability,
restart the owner from object storage and verify committed items and table
metadata; replay requests and inspect idempotency outcomes. For isolation,
repeat with two accounts using the same table names. For partitioned cells,
inject owner loss between prepare, decision, and apply and verify atomic
resolution.


### Development storage layout

Cell catalog heads include the tenant as well as the application and shard.
This isolates account and credential catalogs sharing a BeyondDB store. Existing
unreleased roots with application-only catalog heads require reprovisioning;
there is no fallback reader. This does not enable multi-tenant backup or garbage
collection. See [the recovery finding](CROSS_CELL_TRANSACTIONS.md#tenant-catalog-collision-found-during-recovery-qualification).

Global-index journals and metadata, local-index schemas, required table/participant metadata, and settled-root
observations in the coordinator registry also change the unreleased Cell format.
Reprovision development roots created before these changes; there is no in-place
upgrade reader.

## Serving global-index recovery

The supervised projection worker discovers base and global-index ranges for
configured accounts. It restores Idle owners and uses fenced takeover after
remote leases expire, including indexes with no pending journal. A failed
index does not prevent healthy indexes from applying the same source change;
the journal remains until all have applied. See [global index recovery and
qualification](GLOBAL_INDEXES.md#splits-and-recovery). This does not establish
fleet placement, bounded recovery time, or 10,000-Cell capacity.

## Independent client qualification

`scripts/qualify-upstream.py` starts a fresh local RustFS store and the compiled
BeyondDB binary, then runs ExtendDB's Python client tests unchanged. Requires
`uv`, `openssl`, and `rustfs`. Supply a read-only ExtendDB checkout at the pinned
dependency revision and an existing artifact directory on the workspace volume:

```sh
uv run crates/beyonddb/scripts/qualify-upstream.py \
  --binary "$HOME/Workspace/crabbuild-target/crab-your-worktree/debug/beyonddb" \
  --tests-root /path/to/extenddb/tests/python \
  --artifacts "$HOME/Workspace/crabbuild-target/crab-your-worktree" \
  test_transactions.py test_items.py
```

Build the binary first using this checkout's separate `CARGO_TARGET_DIR`. The
runner records the binary digest, upstream revision, client versions, JUnit
results, and service logs in a fresh artifact directory. It uses only local
fixture credentials and stops both services on exit. It disables bytecode and
pytest cache writes in the upstream checkout. Omit test selectors to collect the
whole Python suite; it stops at the first failure. Passing selected files does
not establish full DynamoDB compatibility.

The persisted creation-placement field changes the unreleased table-record
format, including embedded base/index specifications. Existing development
roots require reprovisioning before running this revision.
