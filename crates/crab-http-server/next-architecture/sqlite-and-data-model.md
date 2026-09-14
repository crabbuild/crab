# SQLite runtime and application data model

[Design index](README.md) · Proposed architecture; not implemented.

The SQL transaction and WAL boundaries here feed the
[publication coordinator](storage-protocol.md#commit-publication-and-response-gating).
The [crab-ltx implementation](crab-ltx.md#implemented-state) supplies local capture,
snapshot and exact restore, plus optional remote transport, immutable views and
writable sparse SQL with checksum-seeded continuation. Full restoration remains
the initial server activation policy; sparse support is a library capability,
not yet a wired AppCell workflow. The domain schema, executor and HTTP publication wiring
in this document are still proposed.
Restore and takeover follow [recovery rules](recovery-and-retention.md);
the [offline importer](hard-cutover.md) must preserve domain identities and retry
semantics when constructing these tables.

## SQLite runtime and WAL capture

### Connection and executor model

Each loaded cell owns one application writer connection and the replication
connections required to protect WAL capture. A bounded set of dedicated blocking
executor threads owns synchronous SQLite work; cells are assigned to executors.
Do not create a permanent Tokio blocking task per catalog record or perform SQL
and WAL parsing on the async I/O worker pool.

The logical actor accepts typed domain commands through a bounded mailbox.
Async object-store operations run outside SQL transactions. While a publication
is pending, the actor can handle control messages and renewal, but it does not
start another application mutation in the first version. Other cells continue.

`ManagedDb` now owns the application writer and both replication connections.
Domain commands use its transaction callback; no independent writer factory
may bypass it. Restore uses explicit local files before managed activation.
No untracked database opener may change checkpoint behavior.

### Initial database settings

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA foreign_keys = ON;
PRAGMA wal_autocheckpoint = 0;
PRAGMA busy_timeout = 1000;
```

These are the implemented managed-connection defaults. The writer also receives
a `max_page_count` derived from library admission. Domain schema initialization
and future query-only read connections must enforce the same factory policy. Pin and
test the compiled SQLite version, enabled features, page size and restore codec.

SQLite WAL permits concurrent readers but only one writer. WAL shared-memory
coordination assumes a local machine; do not place a shared writable database
on NFS or mount the same database into several Pods.
[SQLite WAL documentation](https://www.sqlite.org/wal.html)

`FULL` protects the local commit against relevant local failures; it does not
make a remote copy durable. Object-store publication remains necessary.

### WAL capture protocol

Disabling auto-checkpoint alone is insufficient. The replication subsystem
must own WAL-generation tracking, read locks, capture through complete commit
boundaries, checksum validation, checkpoint barriers and safe WAL restart.

The inspected Celld implementation explicitly holds a read-lock connection and
coordinates checkpoint takeover. It includes handling for WAL salt changes,
restart and passive-checkpoint races. Port that responsibility as a tested
unit; do not replace it with periodic copies of the `-wal` file.
[Pinned capture implementation](https://github.com/denoland/celld/blob/10cb1303dac710dcb3b557e318e08c855261f68b/crates/ltx/src/db.rs)

The application transaction returns a local revision and commit boundary. The
capture layer emits complete LTX coverage and a post-apply checksum covering that
revision. Partial WAL tails and uncommitted transactions are excluded. WAL salt
changes require an explicit validated transition, not concatenation of offsets
from different WAL generations.

The pinned Celld L0 writer does not supply this rolling checksum unchanged: it
uses the no-checksum flag. The [capture checksum adaptation](crab-ltx.md#capture-results-and-checksum-contract)
is a required integration gate. File CRC or an uploaded TXID with checksum zero
cannot stand in for the verified database position described here.

If a WAL hook is used, it only signals work and records lightweight state. It
must not await object storage or treat a hook error as a rollback: SQLite calls
the hook after commit, and an error returned by the hook can surface to the
statement even though the transaction committed. There is only one hook per
connection, and auto-checkpoint registration can replace it.
[SQLite WAL hook contract](https://www.sqlite.org/c3ref/wal_hook.html)

### Checkpoint and snapshot discipline

Checkpoint only through the managed replication layer after required WAL bytes
have been safely captured. Captured local segments remain retained until their
remote publication outcome is resolved. Bound both retained segments and WAL
growth; stop new commands before exhausting disk.

A snapshot must describe an exact database revision. Use a supported consistent
SQLite backup/checkpoint mechanism with application mutation admission paused
where needed. Never copy only `app.sqlite` while committed pages remain in WAL.
The SQLite online backup API provides a database snapshot mechanism, but the
replication coordinator must still bind that snapshot to its exact published
position. [SQLite backup documentation](https://www.sqlite.org/backup.html)

Restore to a fresh temporary directory, fsync required files and directory
metadata, validate, and atomically install the local working copy. Leftover
scratch from another session is evidence or cache input, not authority.

## Read consistency and pagination

### Owner reads

Application reads go through the owner. The simplest first implementation
serializes them between mutation publication cycles, so the connection's visible
state equals a published revision.

To provide a linearizable repository read despite a paused stale owner:

1. Start an owner read only when no unpublished local mutation is visible.
2. Materialize the bounded result at local published position `P` while keeping
   subsequent application writes out of this actor's execution interval.
3. Perform an uncached control read after materialization.
4. Return the result only if the owner session/epoch still matches and the
   authoritative application position matches `P`.
5. Otherwise discard the result and reroute or return a retryable error.

For this comparison, logical position means generation, application revision and
verified database state. Compaction may change the manifest digest without
changing that logical position; a new activation always changes the epoch and
requires rerouting.

A takeover after step 3 overlaps the read and can be ordered after it. A takeover
before step 3 is detected. This makes the extra object-store read an intentional
cost. A future lease-based read optimization needs a separate proof and fault
tests before removing it.

No follower database reads or undocumented stale-read fallback are enabled.
Pure Git reads continue to follow their existing repository snapshot contracts.

### Mixed views

A PR page combines published SQL discussion state with a Git ref snapshot.
Those are separate observation times. Return or internally retain enough
position information to bind approvals and check evaluation to the exact head
OID. A page view does not authorize a later merge without publication-time
revalidation.

Membership, archive state and branch protection remain separate control-plane
state. Their consistency is not upgraded to a cross-store transaction by this
design. Preserve current authorization refresh rules and explicitly qualify
revocation and policy races across long operations.

### Pagination

Use indexed keyset pagination with deterministic tie-breakers, preserving
existing externally visible ordering, filters, page limits and response shapes.
Do not change issue and PR numbering namespaces to imitate GitHub during a
storage migration.

An opaque cursor binds repository UUID, resource kind, sort/filter definition,
cursor version and last key. Sign it with the existing cursor-key mechanism.
Do not promise a snapshot spanning multiple requests unless implementing a
bounded snapshot token; concurrent edits may change list membership.

At the hard cutover, reject incompatible old cursors with a clear reload
response and deploy the matching embedded UI with the new server. No legacy
cursor decoder is required for this transition.
Index-backed pagination need not reproduce the old implementation's empty pages
caused by sparse object scans, but clients must still follow `next` correctly.

Repository-wide search can use indexed SQL and a later explicitly enabled FTS
schema. Cross-repository issue search would require a separate derived index or
bounded fan-out design. Catalog listing must not awaken every AppCell.

## Relational application model

### Schema conventions

Each database belongs to exactly one repository UUID. A singleton identity table
records that UUID and the database schema generation. Domain tables therefore
do not need a redundant repository column on every row.

Use stable issuer/subject pairs for persisted authors. Display names are
presentation snapshots, not identity keys. Membership remains in the catalog;
an old author record does not grant current access. Validate case-insensitive
label uniqueness using the same normalization contract as the application,
rather than silently substituting SQLite's ASCII-oriented collation behavior.

Keep JavaScript-visible IDs and versions in the existing supported integer
range. All mutations use parameterized SQL and explicit transactions. Persist
UTC timestamps using the current millisecond convention; use monotonic clocks
for local deadlines. Timestamps do not establish transaction ordering.

### Implemented schema v1

The exact migration is
[`0001_repository_identity.sql`](../src/cells/migrations/0001_repository_identity.sql);
the registry hashes those bytes into the repository module digest. Do not copy a
second executable schema into this design. Schema v1 currently contains:

| Table | Key | Purpose |
| --- | --- | --- |
| `repository_identity` | singleton `1` | 16-byte catalog repository UUID and checked application revision |
| `repository_sequences` | kind | issue-number allocator, initially `('issue', 0)` |
| `repository_issues` | number | author snapshot, title/body, state, version and timestamps |
| `repository_comment_sequences` | issue number | independent checked comment allocator per issue |
| `repository_issue_comments` | issue number, comment number | author snapshot, body, version and timestamps |

All tables are `STRICT`. JavaScript-visible counters are checked against
9,007,199,254,740,991. The foreign keys from comment state to issues use cascade
deletion, although command handlers also verify parent existence explicitly so
their business rejection does not depend on connection pragma state.

Runtime-owned `sys_requests` is the sole command deduplication ledger. Do not add
a second repository `requests` table: `CellClient` binds the request ID to the
module/command/codec/input digest and the actor stores the typed success or
business rejection in the same transaction as domain state. Planned outbox rows
reference their stable effect/operation identity and domain object, not a
duplicated HTTP response cache.

### Remaining domain tables

| Domain | Required relational content | Important invariants |
| --- | --- | --- |
| PRs | `pulls`, `pull_comments`, `pull_reviews`, review comments if supported | Base/head refs, recorded OIDs, method, state, version, immutable merge intent |
| Assignments | `issue_assignees`, `pull_assignees` | Distinct stable subjects; resolve against current membership |
| Labels | `pull_labels`, allocation history and reservation records | Preserve existing lifetime allocation and tombstone rules |
| Statuses | `commit_statuses` | Immutable status events, exact commit OID and context, deterministic latest selection |
| Checks | `check_runs`, `check_outputs`, supported annotation rows | Existing state transitions, revision checks, bounded output and request replay |
| Releases | `releases`, `release_assets`, tag/name claims and upload reservations | Tag identity, asset integrity, metadata tombstones, uniqueness rules |
| Retry state | Offline mapping from imported reservations/claims into `sys_requests` outcomes | Preserve actor/content conflicts and allocated IDs even for incomplete operations |
| Replication metadata | Managed capture control tables | Reserved names; never mistaken for user/domain tables |

Separate issue and PR comment tables keep foreign keys concrete. Do not add
polymorphic foreign keys that SQLite cannot enforce just to reduce table count.
JSON is appropriate for bounded structured output or immutable intent payloads;
it should not become a generic `documents(path, json)` replacement for the
relational domain model.

### Issue creation transaction

Within `BEGIN IMMEDIATE`:

1. The actor looks up request ID plus canonical command digest in `sys_requests`.
   A matching outcome returns without invoking the handler; a different digest
   is a request conflict and does not allocate.
2. Open the application savepoint and validate actor/title/body again inside the
   compiled handler.
3. Increment `repository_sequences.last` for kind `issue` with a checked upper
   bound and read the resulting number in the same outer transaction.
4. Insert the issue and version 1, then increment
   `repository_identity.app_revision` exactly once.
5. Encode the typed result within the registered 80 KiB bound; release the
   application savepoint and insert the `sys_requests` outcome and runtime
   sequence.
6. Commit locally, capture, upload and publish through the barrier. Return the
   typed output and receipt only after control names that exact root.

The canonical digest covers Cell/incarnation identity, module, command ID, codec
version, and the exact bounded input bytes. Because author identity is part of
the input, another actor reusing the same request ID conflicts. Handler-generated
timestamps are not input bytes and are never regenerated for an exact replay.

### Versioned edits and durable retries

Optimistic edits use a predicate such as:

```sql
UPDATE repository_issues
SET title = :title, body = :body, version = version + 1,
    updated_at_ms = :updated_at_ms
WHERE number = :number AND version = :expected_version;
```

Zero affected rows require a not-found/permission/version decision consistent
with the current API. A durable retry lookup happens before rejecting a stale
version, so replay of a successful edit returns its existing result.

Not every existing mutation accepts `request_id`. For those routes, first
preserve the current expected-version and refetch behavior. Adding a request ID
to provide durable edit replay is an explicit API/UI change, with consumer tests;
an internally generated ID cannot deduplicate a later user retry that never saw
that ID.

Durable replay establishes the same logical outcome, not necessarily identical
HTTP bytes. Existing issue creation replay can resolve the current version of
the originally created issue. Preserve that behavior by storing a stable outcome
descriptor and reconstructing the response from published state where the
endpoint requires it. `response_json` may hold this versioned descriptor. Do not
return an old serialized permission flag or user display value as current truth.

Do not silently expire old creation request IDs and permit them to allocate
again. Keep compact durable deduplication records for the supported lifetime.
If response payload retention is later bounded, retain identity, content hash,
resource result and a documented replay policy. Imported reservations may need
their original validation fields to preserve exact conflict behavior.
