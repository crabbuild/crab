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

Repository assignment format 2 and discussion index format 6 require the explicit
preparation described below. Other canonical records retain their formats.
Transport requests and replies use bounded JSON. Mutation receipts store
the payload digest and outcome in the same transaction as application writes.
Rejections roll application writes back in a nested SDK transaction while keeping
their receipt. Identical retries recover the original decision; conflicting
payloads fail. Receipts expire after the request expiry plus a 24-hour retention
window, with bounded pruning on subsequent mutations.

Native target addresses now distinguish Repository UUIDs from Directory. This
changes the private peer application envelope: upgrade the fleet together;
mixed-version operation is not supported or qualified. The compiled script name
and existing Repository class/namespace are unchanged because Cellar uses these
names in durable identities. Update and drain all application peers together for the changed repository DTOs;
old private RPC envelopes are not supported after cutover. Public submission
reservations continue to read the current canonical records. Native transport
receipts retain their original bytes and normal expiry; they are not rewritten
or interpreted through a compatibility decoder.

Directory format 2 separates `accounts` from optional `profiles`. Canonical
bootstrap DDL lives in `cells/directory/identity.sql`, `schema.sql`,
`accounts/schema.sql` and `profiles/schema.sql`. Run
`cells initialize-directory` through the running server's private mTLS listener
before exposing profiles or nonempty issue/PR lists. Repeating initialization
verifies the current schema; reads and writes never install tables. Format 1
requires `cells migrate-directory`: bounded native transactions preserve actor
UUIDs, profile versions/data, enrollment reservations and receipts. Progress is
durable across restart, and invalid source records roll back the whole current
batch, including schema preparation. Only the explicit migration reads retired
tables; other operations fail closed until format 2 is ready. Private native
423 signals preparation required and becomes public 503 `schema_upgrade_required`.
The typed `DirectoryState` is shared with the CLI; `cells directory-status` reports
outdated/migrating/ready state. Peers must update together. Missing
preparation returns a visible 503 `directory_unavailable` from profile APIs and
author-enriched lists. Global readiness still verifies repositories; it does
not certify Directory readiness. This directory
adds no organization, team, project or repository grant authority.

Current repository grants still belong to the catalog. HTTP admission resolves
those grants before repository access, checks the configured issuer and stable
repository UUID, and never derives authority from the routing snapshot. Git
publication and LFS upload refresh grants at their late authorization checkpoints.
This is not a native membership migration or atomic authority fencing. Git-token
format 2 adds the stable UUID; previous tokens require revocation/reissue and
coordinated replica deployment. Sessions and canonical Cell schemas are unchanged.

`cells register-accounts --identities-file` creates stable IDs for explicit
qualified private accounts without requiring or inventing a public profile.
Enrollment reuses those IDs. Account registration grants no repository or other
permissions. This is an identity foundation for assignment/participant/owner
models; existing assignments convert through the explicit repository preparation
described below. It is not a native grant cutover.

List author enrichment uses the bounded native `LookupActors` read (Directory
operation 6). It resolves qualified private keys into public UUID/handle/name
summaries in one snapshot, with no repository record or schema change. Directory
identity validation counts Unicode scalar values, matching SQLite length bounds
and valid stored repository identities. Lists recheck durable membership after
the separate Directory read; profiles never act as grants. Counts/cursors keep
their repository snapshot. Prepare Directory before enabling this DTO contract
and update peers together; unsupported operations have no fallback reader.

The Author list menu composes Directory handle completion with native query
navigation. Directory read 8 returns a bounded `items`/`truncated` result, using
21 indexed matches to distinguish exactly 20 from an incomplete result. Update
native peers together for this output change; there is no old-array reader.
Public menu records contain only actor summaries and query destinations. Catalog
grants are rechecked after the Directory read. No canonical schema or derived
index change is required.

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

Label choices read the existing canonical SQL catalog and validate its records;
they do not introduce a second label store or a new index format. Native query
destinations share the parser and global-conjunction rules with milestone and
state navigation. Definitions, assignments, counts and cursors continue to use
the existing native transaction and revision contracts. Prepared format-2
repositories need no additional rebuild for the label menu.

## Reviewer search preparation

Discussion index format 6 adds qualified reviewer projections. Each PR/identity
row records whether a review was submitted and whether a direct request remains
pending. The canonical review/request commands update that row in their native
transaction, after permanent submission replay checks. A commented review counts
as review participation and fulfills a pending request. Cancelling a request
preserves submitted participation, and retrying an old review cannot fulfill a
newer request. Reviewer rows reference canonical PRs rather than disposable index
documents, so ordinary text/label projection refreshes retain relationships.

Explicit rebuild adds bounded reviewer clearing, SQL review-history paging, and
KV request-source paging. SQL paging [seeks by the composite primary key](https://www.sqlite.org/rowvalue.html#scrolling_window_queries).
A separate typed source cursor persists the composite
review key or exclusive KV bound. Both phases process at most eight source records
per transaction; unrelated pull metadata also advances the request cursor. Reads
remain unavailable until all sources complete. Domain records, versions, request
history, receipts and public submission mappings are unchanged. Older indexes
require `cells rebuild-search`; fresh repositories initialize format 6.

Native list input adds exact ordered reviewer identity bindings; output adds
`review_requested_from_me`. Author and reviewer bindings share the same Directory
snapshot, digest encoding and validation. Public HTTP cannot supply bindings.
Update peers together; there is no old list-shape reader or source-scan fallback.
Team requests, review-status/merge-policy predicates, general participants and
native grant authority remain separate delivery work.

## Assignee search preparation

Discussion index format 5 adds `name` and folded `name_key` to its assignment
projection. Explicit rebuild begins with constant-default column additions;
[SQLite stores those defaults without rewriting existing rows](https://sqlite.org/lang_altertable.html#alter_table_add_column). Existing bounded
clear/reprojection phases fill snapshots before indexed serving resumes. A
format-4 index is rejected until preparation, with no serving fallback or
canonical record conversion. Interrupted rebuilds resume through the same CLI.

Native list inputs add ordered public-name/account-ID bindings; list output adds
`assigned_to_me`. Repository read 263 returns up to 20 unique historical actor
snapshots plus truncation. Private peer applications must deploy together.
HTTP resolves public handles and the current qualified account through Directory
reads 7/12, then native SQL applies IDs. Literal `actor/UUID` and wildcard terms
need no Directory binding. Counts/cursors include resolved IDs; names grant no
authority. Menu presentation merges current members and optional public profiles,
then rechecks durable repository membership. Canonical assignment format 2 and
Directory format 2 are unchanged.

## Assignment preparation

Repository assignment format 2 stores stable Directory account IDs with explicit
display-name snapshots. Public responses expose `id`, `name`, and an optional
public `profile`; HTTP edits accept `assignee_ids`. Candidate resolution registers
missing qualified accounts without inventing profiles or granting access.
Historical assignees retain their snapshots after membership removal, and chosen
public profiles supply current names and stable profile links. Empty selections
need no Directory read. Nonempty selections and assignment choices require the
explicitly prepared Directory.

Before exposing an existing repository after upgrade, run:

```sh
crab-server --config server.toml cells assignment-inventory --owner OWNER --name REPO
crab-server --config server.toml cells migrate-assignments --owner OWNER --name REPO --mappings-file assignments.jsonl
crab-server --config server.toml cells rebuild-search --owner OWNER --name REPO
```

Inventory streams bounded pages from the private mTLS listener. For each nonempty
source assignment record, the operator supplies one JSON Lines mapping, ordered
by issue number then pull number. Bare stored subjects do not prove a historical
issuer; do not derive it from a current provider, name, repository owner or author.
Each line has this shape, with an explicit mapping for every original subject:

```json
{"target":{"kind":"issue","number":1},"version":3,"mappings":[{"subject":"old-subject","identity":{"issuer":"https://identity.example","subject":"account-subject"},"name":"Recorded name"}]}
```

Empty records need no mapping; omit `--mappings-file` if all are empty. Mapping
lines are bounded to 1 MiB and consumed incrementally. After interruption, rerun
the same command/file: the CLI skips already converted records and resumes at the
native cursor. A stale frame/record version is a recorded conflict, so retrying its native
request preserves the decision even after migration completes. Missing/extra
mappings, duplicate actors or invalid identities fail the current frame. Correct the file and resume; committed
previous frames remain. Qualified account registration is idempotent and grants
nothing. A failed repository frame can leave unused private account IDs because
Directory and Repository transactions are independent.

Eight discussions per native transaction bound conversion. Progress and canonical
SQL commit together. Normal repository operations fail closed while format 1 is
present; private inventory/migration and startup control reads remain available.
The first frame renames the assignment column and converts its values in place;
old application writes fail, and peers must still drain and update together.
As documented by [SQLite](https://www.sqlite.org/lang_altertable.html), renames
only edit schema text; conversion never drops a column or rewrites
whole discussion tables. The empty canonical count encoding also preserves the
existing SQL default. Completion preserves record versions/content/submissions
and requires an explicit search rebuild. Format 4 renames its actor
lookup column and clears/repopulates entries through its existing bounded phases. Serving never decodes the retired subject format or rebuilds the
index implicitly. Readiness requires both assignment preparation and search
preparation. No dependency versions or storage grants change in this upgrade.

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

Discussion index format 2 added a bounded milestone catalog and one assignment
projection per discussion. Titles/states and assignments update inside the same
native transaction as their canonical milestone commands and replay receipt.
Renames invalidate query cursors without rewriting every assigned discussion.
Existing older indexes report `outdated` with their generation and format;
indexed reads and fresh mutations return unavailable. Run the same
`cells rebuild-search` command to begin a new generation. Preparation clears
old projections, rebuilds milestone titles in batches of eight from their
canonical KV catalog, then rebuilds issues and PRs with their assignments.
Interrupted steps resume their persisted phase/cursor/version. Serving never
installs these tables or falls back to old projections. Unknown formats fail.
Repository domain records and stored command replies retain their formats.

Discussion index format 3 adds one 32-byte qualified-author digest per
indexed discussion and an indexed lookup. Canonical issuer/subject pairs are
encoded as a JSON tuple before hashing; their codec belongs to this index
format. The canonical discussion row and projection commit together. Rebuild
format-1/2 indexes explicitly with the same bounded management command;
canonical data and replay receipts stay unchanged. Author handles resolve in
Directory reads 7/8, with private bindings carried in native list inputs and
included in cursor fingerprints. There is no distributed snapshot across the
Directory and repository Cell; each query uses the identities it resolved and
rechecks the durable repository grant after awaited reads. Update fleet peers
together for the changed native list input/output and Directory operations.

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

Discussion index format 2 passed the full isolated server suite (329 passed,
five existing ignored). Real RustFS browser selection, milestone queries and
choices survived process restart and interrupted rebuild. Two persistent local
format-1 indexes reported `outdated` and were explicitly rebuilt to format 2;
canonical repository records, milestones and assignments compared unchanged.
The final mobile dropdown margin passed focused keyboard/accessibility/layout
checks after the 75-test full browser run. This proves index preparation and the
implemented list filter; it does not complete organization/project authority.

The label-choice slice passed 333 isolated server tests (five existing ignored),
107 UI unit tests and all 80 browser tests. The embedded UI exercised native
multi-label, exclusion and no-label queries against real RustFS; exact records,
choice destinations and counts survived process restart. The qualification
script retains label-choice and filter restart receipts. Both persistent
preview repositories compared unchanged after the server-only update. Label
choices reuse canonical SQL records and format-2 indexes, requiring no rebuild.
UI/server builds, all-target Clippy, formatting and architecture checks passed.

The list-attribution slice passed 336 isolated server tests (five existing
ignored), including the new Directory lookup after native mTLS owner handoff
in object and fleet durability modes. It also passed 107 UI unit tests, 86
browser tests, final focused maximum-handle/mobile-containment checks, builds,
all-target Clippy, formatting and architecture checks. These reads add no
repository/index format or stored-reply migration; the new private Directory
operation and list DTO require matching application versions and explicit
Directory preparation.
The embedded UI followed real RustFS-backed mixed-list author links to a stable
public profile, including a maximum-length handle in dark/mobile layout.
Attribution survived process restart and index rebuild; the process script
retains the new attribution-restart receipt alongside existing Git/merge proof.
The persistent preview's two repositories compared unchanged after its update,
and its operator profile remains unenrolled.

These checks do not qualify mixed-version upgrades, protected cloud fault
matrices, or production scale. Release publication remains disabled by a direct workflow gate until those
native contracts are implemented and verified. The obsolete qualification
framework, fleet fixtures, and runtime-specific deployment recipes are removed.
