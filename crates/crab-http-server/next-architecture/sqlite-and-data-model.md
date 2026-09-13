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

### Core schema example

This executable SQL illustrates the core transaction model. It is not a complete
production migration: the domain inventory below defines additional tables and
the implementation must supply their constraints and fixtures.

```sql
CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    checksum TEXT NOT NULL,
    applied_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE repository_identity (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    repository_uuid TEXT NOT NULL UNIQUE,
    app_revision INTEGER NOT NULL DEFAULT 0
        CHECK (app_revision BETWEEN 0 AND 9007199254740991)
) STRICT;

CREATE TABLE sequences (
    scope TEXT PRIMARY KEY,
    last_value INTEGER NOT NULL
        CHECK (last_value BETWEEN 0 AND 9007199254740991)
) STRICT;

CREATE TABLE requests (
    scope TEXT NOT NULL,
    request_id TEXT NOT NULL,
    actor_issuer TEXT NOT NULL,
    actor_subject TEXT NOT NULL,
    request_hash BLOB NOT NULL CHECK (length(request_hash) = 32),
    state TEXT NOT NULL CHECK (state IN ('pending', 'complete', 'conflict')),
    response_status INTEGER,
    response_json TEXT,
    app_revision INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (scope, request_id),
    CHECK (
        (state = 'pending' AND response_status IS NULL AND response_json IS NULL)
        OR
        (state IN ('complete', 'conflict')
         AND response_status IS NOT NULL AND response_json IS NOT NULL)
    )
) STRICT;

CREATE TABLE issues (
    number INTEGER PRIMARY KEY
        CHECK (number BETWEEN 1 AND 9007199254740990),
    request_id TEXT NOT NULL UNIQUE,
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('open', 'closed')),
    version INTEGER NOT NULL CHECK (version > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE issue_comments (
    issue_number INTEGER NOT NULL REFERENCES issues(number),
    number INTEGER NOT NULL CHECK (number > 0),
    request_id TEXT NOT NULL,
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_name TEXT NOT NULL,
    body TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (issue_number, number),
    UNIQUE (issue_number, request_id)
) STRICT;

CREATE TABLE labels (
    id INTEGER PRIMARY KEY CHECK (id > 0),
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    color TEXT NOT NULL,
    description TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version > 0),
    deleted_at_ms INTEGER
) STRICT;

CREATE UNIQUE INDEX labels_live_name
    ON labels(normalized_name) WHERE deleted_at_ms IS NULL;

CREATE TABLE issue_labels (
    issue_number INTEGER NOT NULL REFERENCES issues(number),
    label_id INTEGER NOT NULL REFERENCES labels(id),
    PRIMARY KEY (issue_number, label_id)
) STRICT;

CREATE TABLE publication_outbox (
    operation_id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('pull_merge', 'release_tag')),
    request_scope TEXT NOT NULL,
    request_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (
        state IN ('prepared', 'publishing', 'reconciling', 'complete', 'conflict')
    ),
    ref_name TEXT NOT NULL,
    expected_old_oid TEXT,
    intended_new_oid TEXT NOT NULL,
    intent_json TEXT NOT NULL,
    receipt_json TEXT,
    version INTEGER NOT NULL CHECK (version > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (request_scope, request_id),
    FOREIGN KEY (request_scope, request_id)
        REFERENCES requests(scope, request_id)
) STRICT;

CREATE INDEX issues_state_number ON issues(state, number);
CREATE INDEX issue_labels_label ON issue_labels(label_id, issue_number);
CREATE INDEX outbox_work ON publication_outbox(state, created_at_ms, operation_id);
```

### Remaining domain tables

| Domain | Required relational content | Important invariants |
| --- | --- | --- |
| PRs | `pulls`, `pull_comments`, `pull_reviews`, review comments if supported | Base/head refs, recorded OIDs, method, state, version, immutable merge intent |
| Assignments | `issue_assignees`, `pull_assignees` | Distinct stable subjects; resolve against current membership |
| Labels | `pull_labels`, allocation history and reservation records | Preserve existing lifetime allocation and tombstone rules |
| Statuses | `commit_statuses` | Immutable status events, exact commit OID and context, deterministic latest selection |
| Checks | `check_runs`, `check_outputs`, supported annotation rows | Existing state transitions, revision checks, bounded output and request replay |
| Releases | `releases`, `release_assets`, tag/name claims and upload reservations | Tag identity, asset integrity, metadata tombstones, uniqueness rules |
| Retry state | Imported reservation/claim representation plus `requests` | Preserve actor/content conflicts and allocated IDs even for incomplete operations |
| Replication metadata | Managed capture control tables | Reserved names; never mistaken for user/domain tables |

Separate issue and PR comment tables keep foreign keys concrete. Do not add
polymorphic foreign keys that SQLite cannot enforce just to reduce table count.
JSON is appropriate for bounded structured output or immutable intent payloads;
it should not become a generic `documents(path, json)` replacement for the
relational domain model.

### Issue creation transaction

Within `BEGIN IMMEDIATE`:

1. Look up `(scope = 'issues.create', request_id)`.
2. If found, compare the actor and canonical request hash. Return the established
   result or conflict; do not allocate again.
3. Increment the issue sequence with a checked upper bound.
4. Insert the issue, labels/assignments if accepted by that endpoint, and version.
5. Increment `repository_identity.app_revision` once for the logical mutation.
6. Insert the stable request result at that revision.
7. Commit locally, capture, upload and publish through the barrier.

The canonical hash covers command kind, normalized validated payload, expected
version where applicable and relevant domain identifiers. The row separately
stores actor identity so another author reusing the same scope/ID gets a conflict.
Do not include transient timestamps generated during a retry.

### Versioned edits and durable retries

Optimistic edits use a predicate such as:

```sql
UPDATE issues
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
