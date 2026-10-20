# Native Cellar runtime

`crab-http-server` embeds the published `cellar-main = 0.1.0` SDK, imported as
`cellar`. Product HTTP authorization, Git publication,
repository policy, and the catalog remain Crab responsibilities. Cellar 0.1.0
links `rusqlite` 0.31, so the workspace SQLite dependency follows that version;
the cache progress-handler call uses its matching API.

The linked libsqlite3-sys 0.28 bundles SQLite 3.45.0. It predates the upstream
[WAL-reset fix](https://www.sqlite.org/wal.html#walreset) for concurrent
write/checkpoint connections. A matched dependency upgrade carrying that fix
is required before release; a smoke pass cannot qualify away the dependency bug.

## Execution and persistence

The compiled application binds the `REPOSITORIES` namespace to one native
`Repository` Cell per catalog UUID. `cells/client.rs` sends bounded requests
through the node's native ingress. `cells/application.rs` dispatches typed
repository operations inside Cellar storage transactions. The bounded envelope
is buffered before owner dispatch so Cellar can safely retry stale routes; it is
not forwarded as a non-replayable upload stream. Successful HTTP output
uses the SDK output gate, including its ownership and durability checks.

The SQL application schema and persisted binary records retain their existing
formats. Transport requests and replies use bounded JSON. Mutation receipts store
the payload digest and outcome in the same transaction as application writes.
Rejections roll application writes back in a nested SDK transaction while keeping
their receipt. Identical retries recover the original decision; conflicting
payloads fail. Receipts expire after the request expiry plus a 24-hour retention
window, with bounded pruning on subsequent mutations.

`cells/schema.sql` is the single canonical application schema. Initialization
creates it directly, including replay receipts; no historical DDL upgrade chain
or lazy table creation runs during serving. Only explicit repository
initialization creates application schema. Serving a
catalog-ready repository without that schema fails rather than recreating an
empty collaboration database. The initializer requires object durability, drains
its native node, and waits for its temporary node lease to expire before marking
the catalog ready.

Cellar objects live below `storage.url/.crab/cellar/`. Crab's Azure URL shape is
`az://ACCOUNT/CONTAINER/PREFIX`; the adapter passes the account separately to
Cellar. S3 endpoint precedence follows `crab-storage`. Cellar 0.1.0 requires
path-style S3 for explicit endpoints and virtual-hosted S3 otherwise; contradictory
addressing settings fail configuration. Azure authentication remains subject to
Cellar 0.1.0's supported credential families and public-cloud restrictions.

## Fleet lifecycle

Cellar owns routing, leases, activation, replication, recovery, and eviction.
Its signed peer HTTP and upgrade streams cross Crab's private mTLS listener via
streaming loopback adapters. The outgoing SOCKS connection handshake completes
only after peer TLS succeeds, preserving Cellar's distinction between safe
connect retries and ambiguous execution failures. Raw native peer listeners
bind only to loopback.
A logical node's identity follows its advertised peer address, so hosts can share
a fleet TLS certificate without sharing a node identity.

Normal termination uses Cellar `Preserve`, for restart at the same address.
The private `POST /internal/cells/v1/scale-down` endpoint requests `Handoff`,
which releases ownership and waits for a successor. Its HTTP 202 acknowledges the
drain request; it does not prove completion. Keep a successor running during
scale-down. The last node must use normal termination instead.

Public work drains before the native node; private transport stays available
until native shutdown finishes. Incomplete drains produce an error. Native event
metrics and process resource samples are exported alongside Crab HTTP metrics.
CPU-bound capsule and leaf-run encoding use blocking workers so large Git
publications leave the async executor available for lease renewal.

## Operator changes

Supported `cells` commands are `capacity --json [--live]`, `metrics`,
`status --owner OWNER --name NAME`, and `runtime`. Repository create/adopt and
storage probing remain available. Reads use the authoritative Cell owner. `local_disk_limit_bytes` bounds the
Crab staging envelope; Cellar 0.1.0 has no injected shared disk budget. Place its
cache on a filesystem with an appropriate quota. Physical free-space checks
still guard staging admission. Staging reserves 20% of available disk
(at least 1 GiB) for filesystem and native cache headroom and requires space for
one initial Git receive reservation. Transfer reservations enforce the remaining
capacity; small disks do not imply support for arbitrarily large imports.

`failure_zone` maps to the native Cellar region. Management uses the native
runtime directly; there are no release bootstrap/prepare/activate, pinned
backup, node-session, or configurable read-replica commands.

The server supports native Cellar storage only. It has no previous-runtime
reader, importer, layout detector, or session migration path. Create/adopt
initializes an empty collaboration Cell. A catalog-ready repository must have
its native schema; missing state fails startup. Export/import of data from a
different runtime requires a separately verified migration.

Only catalog schema version 3 and canonical relative audit references are
accepted. There is no read-and-upgrade catalog path.

Identity sessions require their matching identity-index marker and explicit
stored fields. Unindexed sessions are rejected and must sign in again.
Revocation visits only the identity index; it never scans old session records.

## Verification

Focused native tests cover stored-record formats, restart recovery,
conflicting retries, and archive/write ordering. mTLS tests cover connection-error
classification, owner handoff, and replay through a third node with a stale route.
Three-node fleet handoff and mutation replay also passed against real RustFS. HTTP collaboration fixtures now use
actual Cellar nodes. Persisted-record fixtures protect stored bytes. Catalog tests
verify that an uninitialized ready Cell cannot replace the current catalog and
that membership revocation remains effective after installation.

The process smoke test creates a private RustFS instance and synthetic repository,
performs an HTTP issue mutation and replay, pushes Git, restarts the actual server,
then verifies the issue, replay, Git ref, management status, and native objects:

```sh
python3 crates/crab-http-server/tests/qualify_cellar_process.py \
  --binary "$CARGO_TARGET_DIR/debug/crab-http-server" \
  --artifacts "$CARGO_TARGET_DIR/cellar-process-proof"
```

Local process runs passed against RustFS. The script retains its JSON receipt
and logs, and removes its own RustFS container on exit. The latest build and
all-target server Clippy check passed. The full architecture checker and its
13 regression tests pass after rebasing onto the current `crab-v2` dependency
policy.

These checks do not qualify mixed-version upgrades, protected cloud fault
matrices, or production scale. Release publication remains disabled by a direct workflow gate until those
native contracts are implemented and verified. The obsolete qualification
framework, fleet fixtures, and runtime-specific deployment recipes are removed.
