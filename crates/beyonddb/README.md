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
item requests, kills the server without draining it, then restarts it and reads
the committed item after lease expiry and fenced Cell takeover. It also verifies
BatchWriteItem, BatchGetItem, paginated parallel Scan, and TTL expiry across four
initial data Cells before the crash. It checks batch reads and TTL configuration
after recovery, then disables TTL and verifies that state through another
restart. Run it in a
dedicated environment with `rustfs`, `aws`, and `openssl` available:

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-beyonddb \
  cargo test -p beyonddb --test server_binary -- --ignored
```

This server uses an explicit list of locally owned account and credential
Cells. On startup it recovers configured account and credential Cells and routed
data Cells whose previous owner used this node's peer endpoint. This requires
the account to be configured on the restarting node; data-only nodes and
replacements with a different endpoint still need a recovery scheduler.
Automatic placement, fleet-wide unattended takeover, multi-node capacity loops,
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
The synchronous table-transition worker is also implemented. Table resource tags now have Cell-backed CreateTable, TagResource, UntagResource,
and ListTagsOfResource paths; DeleteTable removes their rows. The RustFS
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
Each Cell query helps at most one transaction; BEGIN and unavailable decisions
remain retryable conflicts. Transactional reads retain conflict cancellation.
Once a table's initial route is published, keyed CRUD and Scan use its data
Cells; unactivated tables
still use the account Cell unless a provisioner is configured. The host-backed
provisioner installs 1–256 initial data Cells per table during CreateTable and
resumes after an interrupted setup. The initial count must stay fixed across
retries. A host-backed controller can resume a recorded split. A cancellable
account capacity loop can trigger a split, and the serving binary starts that
loop for locally owned accounts. There is no merge controller or complete
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
and split one range whose SQLite database image crosses a caller-supplied
threshold. That measurement includes indexes and runtime tables, but excludes
WAL and LTX files. The account loop checks one table range per tick and can
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
round trip. `peer_router` authenticates incoming requests against live node
advertisements, restricts targets to BeyondDB namespaces, and dispatches only
to the current local owner. `build_peer_client` binds the owner-resolving HTTP
transport and a fleet-scoped principal to account, credential, and data Cells.
`tests/peer_network.rs` uses separate mTLS identities on two leased nodes,
denies a wrong peer principal, and sends signed AWS SDK CreateTable, PutItem,
and GetItem requests through ExtendDB's public listener and the private peer
listeners. The public node provisions and owns the data Cell; the other node
owns the account and credential Cells. After that owner's lease expires, a
replacement fences its session and restores both Cells from object storage.
Both public endpoints then read the committed item, including a read that
forwards to the data owner. The replacement refuses data takeover while that
owner is live, then fences its expired node session after lease renewal stops,
restores the data Cell from object storage, and reads the item again. Automatic
placement and fleet-wide unattended takeover remain unfinished.

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
contiguous child ranges while leaving every other range unchanged. Route
validation requires complete, nonoverlapping hash coverage and the table's
immutable key schema. Route publication currently trusts the
provisioner to have installed and published the data Cells; it does not verify
their receipts. The source data Cell can persist an idempotent split seal that
fences ordinary reads and writes, then serves bounded export pages after owner
restart. Import-only child Cells accept idempotent item copies and verify an
expected count and digest before activation. Activation closes imports while
keeping ordinary requests fenced. The host-backed split controller admits
children, seals the source, copies bounded export pages, checks both child
fingerprints against the sealed source, and atomically publishes the exact
durable plan against its predecessor route. It then opens the children for
ordinary requests. The host provisioner can choose a range midpoint, record
the plan, and repeat the split on an already opened child. Repeated calls
resume this sequence after interruption.
The account command itself cannot inspect other Cells; serving code must use
the controller rather than calling route publication directly.
Routed keyed CRUD and Scan use the published directory. The account Cell
stores the route epoch and table snapshot alongside indexed range rows, so
keyed requests read one owner row instead of transferring the complete route.
Table status checks use a bounded route page. Scan reads at most 64 owner ranges
per directory page and pins the route epoch while advancing through pages in
one request. Query reads the HASH key's owner Cell through a local ordered
RANGE-key index, including numeric sort keys and page continuation.
Public transactional writes share the coordinator protocol for single-Cell and
cross-Cell requests. Transactional reads confined to one Cell use one snapshot;
cross-Cell transactional reads capture a consistent set of images under
shared locks through the same coordinator. Split
plans persist only the source range, two children, and expected epoch; route
publication does not rewrite a route-sized blob. Host split selection,
publication checks, and results use indexed rows and compact plans. The account Cell's
declared 512 MiB database budget and single writer cannot represent a
production DynamoDB account. The host's LTX limits are separate admission
settings. Production placement needs online route transitions and elastic
data Cell provisioning. A transaction crossing Cells needs durable prepare
intents, one authoritative decision record, idempotent resolution, and
recovery after owner loss. Routing writes to several Cells without that
protocol cannot implement `TransactWriteItems`. See [the elastic topology
design](SCALING.md) for split, routing, recovery, and validation requirements.
Transaction request/intent payloads use bounded SQL chunks while remaining in
one local command. Item and saved-read images also use bounded BLOB transfers,
including JSON images larger than 1 MiB after escaping. The signed SDK fixtures
cover multi-MiB transactions and replay across owner/process restart. Aggregate
Update accounting needs cloud-reference qualification: DynamoDB Local accepts
small Updates over more than 4 MiB of stored images. The protocol document and
`scripts/probe-transaction-size.py` record this distinction. Transactions
use bounded binary uploads for BEGIN and prepare inputs larger than one Cell
RPC, plus bounded recovery queries. Temporary uploads are capped and expire;
HTTP body limits, apply headroom, and history collection remain separate gaps.

The [cross-Cell transaction protocol](CROSS_CELL_TRANSACTIONS.md) specifies
the decision, lock, visibility, and failure-recovery contract.
Account and data Cells persist prepared write images, immutable read images,
and exclusive write/shared read locks using one participant state machine.
Ordinary reads respect write locks; writes respect both lock modes. Data Cells refuse split sealing; account Cells refuse
table deletion and route activation while prepared intents remain. Sharded coordinator Cells can durably
record a participant set, prepare receipts, one commit or abort decision, and
resolution progress. The public write driver resumes published BEGIN records from
stored account/data participant payloads, handles prepare/decision ambiguity, and returns
only after participant resolution. Coordinator token lookup preserves original
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
A supervised serving worker now rotates through locally admitted coordinator
shards, resumes abandoned BEGIN records, and finishes terminal decisions. It
processes at most one pending transaction per tick, advances past failures,
and revisits them on a bounded pass. Cell admission reclaims settled
coordinators at the active-Cell limit; idle shards restore before token lookup.
Recovery reactivates released coordinators; startup resolves shards one at a
time. General Cell placement/activation, bounded transaction/read-image
retention, and fleet qualification remain incomplete. Production admits 64
active Cells per node; busy coordinators apply retryable backpressure. See
SCALING.md for the unqualified 10,000-Cell, multi-TB target.

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
sort-key Query, index operations, and streamed writes remain unsupported.
`tests/account_cell.rs` exercises them through a real
`CellNodeBuilder` and in-memory object store, including request replay,
receipt-based reads, conditional writes, update expressions, scan pagination, rollback of a
multi-table write, and restoration from the published object-store root on a
new host. `tests/elastic_cells.rs` exercises two distinct data Cells through a
real host, range and epoch rejection, route validation and publication,
partitioned adapter CRUD, Scan, same-Cell and cross-Cell writes, rollback and
cross-Cell snapshots, shared-read/write conflicts, account write fencing, and restoration of
both a data Cell and the route's account Cell from object storage. These tests
do not prove live repartitioning, complete IAM, secondary indexes, Streams, or backups. The
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
