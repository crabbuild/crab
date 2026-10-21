# Native Cellar runtime

`crab-server` embeds the published `cellar-main = 0.1.0` SDK, imported as
`cellar`. Product HTTP authorization, Git publication,
repository policy, and the catalog remain Crab responsibilities. Cellar 0.1.0
links `rusqlite` 0.31, so the workspace SQLite dependency follows that version;
the cache progress-handler call uses its matching API.

The linked libsqlite3-sys 0.28 bundles SQLite 3.45.0. It predates the upstream
[WAL-reset fix](https://www.sqlite.org/wal.html#walreset) for concurrent
write/checkpoint connections. A matched dependency upgrade carrying that fix
is required before release; a smoke pass cannot qualify away the dependency bug.

## Execution and persistence

The compiled application binds `REPOSITORIES` to one native `Repository` Cell
per catalog UUID and `DIRECTORY` to a shared native `Directory` Cell. The latter
stores private qualified account keys and chosen public actor profiles.
`cells/client.rs` sends bounded typed requests through native ingress and checks
the operation's class and read receipt's target before routing.
`cells/application.rs` shares transaction, replay and output handling; each
native class owns its schema and operation dispatch. The bounded envelope
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

Native target addresses now distinguish Repository UUIDs from Directory. This
changes the private peer application envelope: upgrade the fleet together;
mixed-version operation is not supported or qualified. The compiled script name
and existing Repository class/namespace are unchanged because Cellar uses these
names in durable identities. Repository SQL/KV records and stored replay replies
retain their formats.

`cells/directory/schema.sql` is a separate initial schema. Run
`cells initialize-directory` through the running server's private mTLS listener
before exposing profiles. Repeating initialization verifies the existing schema;
reads and writes never install tables. Missing preparation returns a visible
503 `directory_unavailable` from profile APIs. Global readiness still verifies
repositories; it does not certify the optional profile directory. This directory
adds no organization, team, project or repository grant authority.

`cells/schema.sql` owns canonical repository domain tables;
`cells/repository/search/index/schema.sql` owns the derived discussion index. Initialization
creates it directly, including replay receipts; no historical DDL upgrade chain
or lazy table creation runs during serving. Explicit repository
initialization creates both schemas for an empty cell. Existing native cells
prepare the derived index through the private management command described below. Serving a
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
`status --owner OWNER --name NAME`, `runtime`, `initialize-directory`, and
`rebuild-search --owner OWNER --name NAME`. Repository create/adopt and
storage probing remain available. Reads use the authoritative Cell owner. `local_disk_limit_bytes` bounds the
Crab staging envelope; Cellar 0.1.0 has no injected shared disk budget. Place its
cache on a filesystem with an appropriate quota. Physical free-space checks
still guard staging admission. Staging reserves 20% of available disk
(at least 1 GiB) for filesystem and native cache headroom and requires space for
one initial Git receive reservation. Transfer reservations enforce the remaining
capacity; small disks do not imply support for arbitrarily large imports.

`cells rebuild-search` contacts the running server's private mTLS management
listener. It explicitly installs/rebuilds the derived discussion schema from
existing native records; it never recreates domain data. Eight records per
command bound the work. Persisted generation, phase, cursor and version allow
restart/resume and prevent duplicate steps from advancing twice. Indexed lists
and fresh mutations return unavailable until preparation finishes; identical
recorded command replays and detail reads remain available. Readiness stays
unavailable until every repository's index is ready. Run preparation before
admitting public traffic after upgrading an existing native deployment.

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
then verifies profiles, issues, replay, Git refs, management status, and native objects:

```sh
python3 crates/crab-server/tests/qualify_cellar_process.py \
  --binary "$CARGO_TARGET_DIR/debug/crab-server" \
  --artifacts "$CARGO_TARGET_DIR/cellar-process-proof"
```

Directory/profile native tests cover explicit preparation, authority/receipt
isolation, concurrent handle claims, qualified identity separation, stable IDs,
versions, replay and restart. The full isolated server suite passed 325 tests
(five existing cases ignored), including Directory remote reads and receipt
replay after mTLS handoff in object and fleet durability modes. A real RustFS
browser run enrolled and renamed an actor, visited its stable public URL and
verified unchanged profile data after process restart and repository-index
rebuild. HTTP tests cover session/CSRF, private-claim exclusion and Git-token
rejection. All 72 UI browser tests and 107 unit tests passed; UI/server builds,
all-target Clippy and architecture checks passed.

Local process runs passed against RustFS. The script retains its JSON receipt
and logs, and removes its own RustFS container on exit. The latest build and
all-target server Clippy check passed. The full architecture checker and its
13 regression tests pass after rebasing onto the current `crab-v2` dependency
policy.

These checks do not qualify mixed-version upgrades, protected cloud fault
matrices, or production scale. Release publication remains disabled by a direct workflow gate until those
native contracts are implemented and verified. The obsolete qualification
framework, fleet fixtures, and runtime-specific deployment recipes are removed.
