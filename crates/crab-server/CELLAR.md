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
per catalog UUID, `DIRECTORY` to a shared native `Directory` Cell, and
`ORGANIZATIONS` to independent native `Organization` Cells. Directory stores
private qualified account keys and chosen public actor profiles. Organization
stores explicit membership, invitation and standalone team authority; repository grants remain
in the catalog.
`cells/client.rs` sends bounded typed requests through native ingress and checks
the operation's class and read receipt's target before routing.
`cells/application.rs` shares transaction, replay and output handling; each
native class owns its schema and operation dispatch. The bounded envelope
is buffered before owner dispatch so Cellar can safely retry stale routes; it is
not forwarded as a non-replayable upload stream. Successful HTTP output
uses the SDK output gate, including its ownership and durability checks.

Repository assignment format 2 and discussion index format 12 require the explicit
preparation described below. Issue lifecycle metadata also requires explicit
`cells prepare-issue-state` on existing repositories. Pull lifecycle metadata
requires `cells prepare-pull-state`. Other canonical records
retain their formats.
Transport requests and replies use bounded JSON. Mutation receipts store
the payload digest and outcome in the same transaction as application writes.
Rejections roll application writes back in a nested SDK transaction while keeping
their receipt. Identical retries recover the original decision; conflicting
payloads fail. Receipts expire after the request expiry plus a 24-hour retention
window, with bounded pruning on subsequent mutations.

Native target addresses distinguish Repository and Organization UUIDs from Directory. This
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
before exposing profiles or author-enriched issue/PR responses. Repeating initialization
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
author-enriched discussion responses. Global readiness still verifies repositories; it does
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

## Organization authority

Organization schema 1 lives in `cells/organization/schema.sql`. Prepare Directory,
then explicitly bootstrap through the private mTLS listener:

```sh
crab-server --config server.toml cells bootstrap-organization \
  --organization 01900000-0000-7000-8000-000000000001 \
  --input-file organization.json
```

The JSON supplies `owner: {issuer, subject}`, `name` and optional `description`.
Use a new organization UUID and the owner's exact provider-qualified account.
Metadata is validated before account registration or schema creation. Bootstrap
registers that stable account without enrolling a public profile; retries with
identical intent preserve the organization. Different bootstrap intent conflicts.
Neither a catalog owner string nor a public handle creates authority. Upgrade
all peers before bootstrapping this new class; existing Repository/Directory
schemas and identities do not change, and no rebuild is required.

The returned `/-/organizations/{uuid}` page has People, Teams, Invitations and profile
settings. Public APIs accept account sessions or local operator identity, never
repository Git tokens. HTTP resolves the qualified account to its stable UUID;
native content reads and authority writes check current membership in their own
transaction before receipt replay. Team commands separately permit recovery of
actor-bound terminal decisions after revocation; they return only an
already-known target UUID or rejection code. Privileged content and
grants cannot pass that exception.
Member and invitation pages seek indexed UUIDs with limits 1–50 and
organization/revision-bound cursors. Public profile enrichment is bounded and
followed by a current organization permission check.

Owners edit/remove active memberships; the final owner cannot be removed or
demoted. Removed memberships retain their versions. Subsequent membership is
created only by accepting a versioned invitation. The authenticated recipient
alone can accept/decline; cancellation or offer edits invalidate old decisions.
Pending offers expire at seven days, may be edited without extending their
deadline, and may be explicitly retried after expiry. They are organization-owned
offers until cancellation/expiry, independent of the inviter's later role.
Acceptance commits its offer state, membership version and audit event atomically.
Reads do not mutate expired offers. Closed offers cannot regrant access after
removal or disclose current organization metadata to a former member. Their
recipient can still inspect the historical invitation status. Immutable activity entries share the command's SQL/KV transaction;
no-ops and rejected operations append nothing.

Invitations currently target registered accounts, with public-username search and
a shareable acceptance link. There is no email delivery, organization owner-name
registry/discovery, nested teams, organization deletion, personal-owner namespace, or
repository/project grant inheritance yet. The native authority is groundwork for
those features, not a claim of complete organization or Projects parity. There
is no distributed transaction between Organization and Repository Cells; native
repository authority fencing/cutover remains separate work.

### Standalone teams

`cells/organization/teams/` owns team metadata, visibility, direct member/maintainer
edges, organization membership incarnations and permanent submission outcomes.
Fresh private organization bootstrap installs this feature atomically. For an
existing organization, update all peers, then explicitly prepare through mTLS:

```sh
crab-server --config server.toml cells prepare-organization-teams \
  --organization ORGANIZATION_UUID --actor CURRENT_OWNER_ACTOR_UUID
```

Preparation installs the feature relations and indexes membership incarnations
in frames of eight. Progress is versioned, restartable and transactionally rolled
back on failure. Existing membership versions, organization metadata and old
receipts are preserved. Serving never installs upgrade DDL. Fresh organization
member changes and invitation acceptance fail closed until preparation completes;
ordinary organization reads and invitation declines remain available.

All current organization members see visible (`closed`) teams. Secret teams are
visible only to their current direct members and organization owners. Owners and
current team maintainers edit metadata and direct memberships or delete a
standalone team. Members may create teams by default; owners can restrict creation
to owners. Creation makes its actor a maintainer. Leaving/rejoining the organization
advances a membership incarnation; old team edges cannot become grants again.
Role changes retain the same incarnation. Team deletion preserves tombstones and
submissions; name reuse creates a new UUID without inherited membership.

Public `/api/organizations/{id}/teams` reads searchable, bounded native pages.
`/{team}` reads a team, `/{team}/members` its current roster,
`/{team}/candidates` the paginated current organization roster for managers, and
`/{team}/members/{actor}` a manager's versioned membership read. `/policy` is
owner-only. Cursors bind organization, revision, query/collection and team;
team-list cursors contain a UUID rather than a potentially private team name.
Directory enrichment rechecks current team access and the same native revision
before exposing associations. Candidate pages currently have pagination, without
username autocomplete.

POST `/teams/commands` accepts `{request_id, command}` with a tagged
`create`, `edit`, `delete`, `member` or `policy` command. POST `/teams/cancel`
accepts the identical body. The native transaction serializes execution and
cancellation against one permanent actor/digest ledger. Cancellation recovers an
applied write or durably fences an unapplied one; a late original cannot execute.
Terminal domain rejections are permanent too: freeing a name or enrolling a
candidate cannot make a delayed rejected request execute. Their guards run before
any domain write; the terminal decision commits without changing team data or
audit. Only matching minimal terminal decisions recover after revocation. A current or
historical organization member may fence their own fresh submission; outsiders
cannot allocate cancellation records. Changed payloads/accounts conflict.

Browser submission ownership stays above route and organization loading, bound
to the original account, organization, UUID and payload. Unproven denials and lost
responses retain that intent; only a validated acknowledgement or canonical
rejection releases it. Reload/closing warns while an intent is uncertain. A
conflicted settings draft survives refresh until saved metadata is reviewed.
The UI uses shadcn components and existing Geist/Vercel tokens.

These teams do not yet own repository/project grants, nested membership, review
requests or mention notifications. Catalog repository grants are unchanged, and
there is still no atomic cross-Cell authority cutover.

## Repository collaboration

Discussion author enrichment uses the bounded native `LookupActors` read (Directory
operation 6). It resolves qualified private keys into public UUID/handle/name
summaries in one snapshot, with no repository record or schema change. Directory
identity validation counts Unicode scalar values, matching SQLite length bounds
and valid stored repository identities. Handlers recheck durable membership after
the separate Directory read; profiles never act as grants. Counts/cursors keep
their repository snapshot. Prepare Directory before enabling this DTO contract
and update peers together; unsupported operations have no fallback reader.

Reviewer selection also uses Directory account UUIDs. The HTTP member roster
binds them back to current qualified repository members and emits only public
candidates. Native request records remain qualified and retain their existing
keys, versions and replay semantics. HTTP and native mutations now both require
triage/write authority; authorship alone is insufficient. Deploy serving peers
and the bundled UI together. These public DTO and permission changes need no
schema, assignment or discussion-index rebuild. Read access still permits review
submission. Public profile attribution does not grant management authority.
Review HTTP writes now require the inspected `commit_oid`; fresh writes verify
the live head, while retries use the existing native immutable submission
ledger. This adds no native field or schema change. Deploy the API and bundled
UI together; old HTTP payloads have no fallback decoder.

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
require `cells rebuild-search`; fresh repositories initialize current format 12.

Native list output adds `review_requested_from_me`. Author, reviewer and commenter
names now share one ordered `people` binding collection, one Directory snapshot,
digest encoding and validation. Public HTTP cannot supply bindings.
Update peers together; there is no old list-shape reader or source-scan fallback.
Team requests, mention relationships and native grant authority remain separate
delivery work. Review-status predicates are described below.

## Reviewer status preparation

Discussion index format 9 adds a latest-review number and optional canonical
request key to each qualified reviewer row. Fresh review creation and request
changes update these references in their native transaction, after replay
checks. The latest decisive review remains in the canonical decision table;
comments do not overwrite it. Private read 252 returns bounded participant pages,
including uninvited commenters, with point joins to their canonical sources.
The retired request-only read 251 is removed. Deploy matching native peers and UI.

Run `cells rebuild-search` on existing repositories. The existing reviewer clear
phase removes eight rows per frame, then replaces the empty derived table with
the new constrained shape. This avoids the full-table constraint validation of
ALTER ADD COLUMN. Review and request phases rebuild eight source records at a
time, preserving canonical records, source IDs, versions, and receipts. Reads
and new writes fail closed until preparation completes. Interrupted frames use
the existing generation/version cursor. No serving-time rebuild or old reader
is supplied.

## Conversation preparation

Discussion index format 11 adds a per-PR outstanding-review-request counter.
Reviewer projection insert/update/delete triggers maintain it in the source
transaction. Lists combine its indexed point read with bounded canonical review
decisions and current-head status/check catalogs; they never scan the participant
history. Existing Cells require explicit `cells rebuild-search`. Bounded clearing
removes each pull's aggregate before its reviewer rows and replays canonical
sources; PR records and original reviews/requests remain unchanged. Native tests
cover format-10 preparation, repeated rebuild and request fulfillment/cancellation.
The API captures selected refs once, verifies PR versions in the native summary
read and reports a missing head explicitly. Merged PRs retain their recorded
source head after deletion. These counts do not establish merge eligibility.

Discussion index format 10 adds an ordered reference index over canonical issue
comments, PR comments, submitted reviews, and compact activity. Contributions
are one editable card; their immutable activity snapshots and the opening event
do not duplicate cards or the root description. Inline thread/reply events remain
compact links. Relationship events retain the same identity in both subjects.
New contributions and lifecycle events project in their native transaction.
Canonical bodies and timestamps remain unchanged; order uses creation time,
source kind and number, including historical clock rollback.

Run `cells rebuild-search` on existing repositories before indexed traffic.
The `conversation` phase clears and scans at most eight references per frame,
with durable source/composite-key cursors. It includes historical SQL comments
that predate activity collection. Restart resumes preparation; missing/building
indexes fail closed. No implicit schema installation or alternate reader runs
while serving. Canonical records, versions, public submissions and receipts need
no conversion. Drain and upgrade peers together for the new native read and
public activity actor DTO; cursors bind subject and index generation. A rebuild
invalidates earlier conversation cursors.

## Commenter search preparation

Discussion index format 7 adds one qualified-identity projection per conversation
comment, submitted review, inline thread or reply. Empty-body approvals count as
submitted participation; opening a discussion or requesting a review alone does
not. Canonical commands project only after submission replay, in their native
transaction. Source keys distinguish equal child numbers across issues, PRs,
threads and contribution types. Canonical deletion triggers remove only that
source, including thread/reply and discussion cascades. Projection rows survive
ordinary discussion document replacement.

Run `cells rebuild-search` before indexed traffic on existing repositories.
Clearing and source scanning each process at most eight records per transaction.
A persisted source/composite-key cursor resumes across all five canonical tables;
the final phase completes only after every source. No canonical records,
versions, assignment formats or public submission records change. Upgrade and
drain all peers together for the unified private people-binding input; there is
no old-input decoder or serving-time scan. `commenter:username` and `commenter:@me`
use current qualified identities and public username completion. Mentions,
`involves` still needs its mention relationship model.

## Discussion text search preparation

Discussion index format 8 adds indexed text for conversation comments, submitted
reviews, inline threads and replies. Text rows reference their contribution
identity through a composite foreign key, so canonical source deletion cascades
through both projections. INSERT/UPDATE/DELETE triggers enqueue text IDs; native
flush deletes old FTS tokens and inserts the current body in the same domain
transaction. No virtual-table writes run from untrusted SQLite triggers. Fresh
writes process one contribution, rebuilding processes eight, and dirty-ID fanout
is capped at 128 per normal mutation. Overflow rolls the whole mutation back.

Default text searches cover title, body and comments. Recorded author names are
no longer indexed as free text; use `author:` for identity search. The `in:`
qualifier selects title, body, comments, or comma-separated combinations. Repeated
selectors in one conjunction combine; an explicitly scoped inner group overrides
its enclosing text fields. Field scopes survive facet and state navigation.
Negate text terms to exclude words; negated field directives are rejected.

Run `cells rebuild-search` before serving existing indexes. Header clearing uses
its existing eight-document frames; a separate preparation frame recreates the
empty header/FTS tables without the retired author-name column. Contribution
clearing and rebuilding use the same bounded source cursor as format 7 and flush
FTS in each frame. All canonical data and public submissions retain their formats.
Both FTS indexes pass integrity checks before readiness. Upgrade and drain peers
together; indexed traffic remains unavailable until preparation finishes.

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

## Issue lifecycle preparation

`cells/repository/issues/lifecycle/` owns one canonical SQL row per issue for
nullable close reason and time. Fresh Repository initialization creates it. For
each existing repository, run:

```sh
"$SERVER" --config /secure/server.toml cells prepare-issue-state --owner OWNER --name REPO
```

Private mTLS GET/POST `issue-state` status/preparation operations use versioned
frames of eight issue/activity steps. Preparation preserves the exact issue
record and immutable events, recovers close time only from matching historical
activity, and leaves missing reasons unknown. SQL rows and the persisted issue/
activity cursor commit together; replay and restart resume the same frame.
Mixed-version serving is unsupported. Prepare before reopening traffic. Missing/
partial metadata returns public 503 `schema_upgrade_required`; serving never
installs DDL or guesses a missing row. PR-only queries without reason predicates
remain independent. No discussion-index format change or rebuild is needed.

State/reason changes, timestamps, milestone progress, typed activity, search
revision and command receipts share the native SDK transaction/output gate.
Existing issue command input digests with no reason and original record/outcome
bytes are retained for durable receipt replay; HTTP renders a causal detail read
after the commit. Existing generic closure events remain immutable history and
continue to represent current PR closure. New issue closures carry their reason.
Duplicate relationships remain unfinished. Combined comment/state actions are
implemented by the native collaboration state operation below.

## Atomic discussion state actions

`cells/repository/collaboration/state/` composes canonical comment writers and
issue/PR state commands in one native transaction. SQL/KV/search/activity/progress
commit together, and rejection rolls all application work back before its native
rejection receipt commits. `collaboration/v1/state-submissions/{subject}/{uuid}`
retains the original payload digest and optional canonical comment number. A
committed intent recovers before later archive/version/merge checks, without
repeating the state write. Current HTTP repository membership still gates access.

No new SQL schema, migration or index rebuild is required. Original standalone
comment inputs, submission digests and record/outcome formats stay unchanged;
only their canonical contribution writers are shared. HTTP marks immutable-payload
validation and confirmed native domain rejection with
`X-Crab-Write-Outcome: rejected`. Context errors and submission-identity conflicts
remain uncertain, including errors before a retry reaches native execution; an
earlier attempt may have committed. Retry the unchanged
original intent after resolving availability/access. Public output exposes only
existing comment DTOs, with final membership checks after enrichment.


## Bounded bulk discussion edits

`cells/repository/collaboration/bulk/` composes existing issue/PR edit and milestone
assignment commands inside one native transaction. HTTP preparation and edits bind
the stable repository UUID; reused owner/name routes cannot retarget a selection.
Selection reads bind record and independent milestone assignment versions. Up to 100 explicit subjects share one
success/rejection; no unbounded query mutation or cross-Cell transaction is implied.
Labels are merged against one active catalog snapshot so retired source IDs cannot
block a later delta. Plain per-item writers and their receipt formats are unchanged.

Permanent `collaboration/v1/bulk-submissions/{uuid}` documents bind the exact targets,
versions, action and qualified actor. They store a count and digest, not a second
copy of discussion records. Recovery precedes fresh role/archive/version checks;
current HTTP read access still gates every response. Current-roster assignee/name
snapshots are excluded from intent identity, and fresh additions require a complete
eligible match. The proven-rejection marker is shared in `api/write.rs`; context,
repository-identity and submission-identity errors preserve the original intent. No new DDL, preparation, index
format, dependency, compatibility reader or automatic authority cutover is involved.

### Prepare pull lifecycle metadata

Before reopening traffic on an existing repository, run:

```sh
"$SERVER" --config /secure/server.toml cells prepare-pull-state --owner OWNER --name REPO
```

Private mTLS GET/POST `pull-state` exposes the same independently versioned
status/frame contract as `issue-state`. The shared lifecycle preparation owner
advances at most eight subject/activity steps per transaction. Fresh repositories
initialize both metadata relations. Serving never installs missing schemas;
pull detail/list reads and state/merge writers fail with 503
`schema_upgrade_required` until preparation finishes.

`repository_pull_lifecycle` stores nullable close and merge-completion times.
Canonical pull records, original command payloads/outcomes and immutable events
retain their formats. Preparation uses the latest direct state transition only
when it agrees with canonical state. Missing or conflicting history stays null;
reservation/last-edit times cannot prove completion. SQL rows and the preparation
cursor commit together, and native receipts replay the original outcome.

Close/reopen and merge completion update lifecycle metadata in the same native
transaction as the canonical pull, milestone progress and activity. Ordinary
edits and exact merge retries preserve it. No search-index format change or
rebuild is required. Update all peers and embedded UI together; mixed-version
serving is unsupported.

### Prepare review dismissal storage

```sh
"$SERVER" --config /secure/server.toml cells prepare-review-state --owner OWNER --name REPO
```

Fresh repository initialization creates `repository_pull_review_dismissals` and
its format marker. Existing repositories use the private mTLS GET/POST
`review-state` route and version-zero preparation above; repeating it is a no-op.
It installs an empty canonical relation, not a derived index or historical reader.
Review details, conversation review entries and reviewer summaries fail with
503 `schema_upgrade_required` until preparation completes. Other canonical
review records, existing receipt bytes and the discussion index stay intact.
Deploy all serving peers and embedded UI together before reopening traffic.

Dismissal, exact-decision removal, review/PR version changes, immutable activity
and permanent recovery ledger share one native transaction and SDK output gate.
SQL stores the complete reason under the existing body limit. Detail/conversation
pages include it in their byte budget before admitting an entry, so large reasons
remain paged and complete. Current catalog grants still supply capabilities;
this does not implement native cross-Cell grant fencing or new team restrictions.

Before protections are saved, HTTP composition supplies the configured initial
branch classification. The command prefers stored rules from its own transaction;
an empty saved set overrides the initial policy. Review snapshot details preserve
the distinction between absent and empty rules and include the exact base ref for
public capability composition. Update serving peers together for this private
native input/read contract; no persisted schema or receipt rewrite is needed.


### Prepare review status search

Discussion index format 12 adds `discussion_index_pull_sources`, a derived
relation containing base/head refs and the completed merge's recorded source
head. Run the existing private `cells rebuild-search --owner OWNER --name REPO`
for each repository before reopening list traffic. Missing preparation returns
503 `discussion_index_unavailable`; request handlers never install its schema.
The bounded rebuild decodes completed merges through their native binary codec
and preserves canonical records and permanent submission ledgers. Native
transport receipts continue their existing bounded append/pruning lifecycle.

Required/approved filters validate a captured source token against the ready
immutable Git ref epoch. Counts and page rows share one native read. Filtered
rollups reuse that epoch and reject source-version changes; cursors also bind
source identity and effective branch rules. HTTP schedules existing bounded
reconciliation and returns `review_index_pending` when refs are not ready.
No-review and changes-requested predicates use canonical active decisions and
need no ref projection. New commits invalidate approval freshness but do not
resolve another reviewer's requested changes; list summaries and protected merge
checks share this rule.

Deploy serving peers and embedded UI together: private search inputs carry
optional captured source context and private rollup inputs carry a source token.
There is no old-input decoder, canonical review migration, new environment
option, dependency override or implicit preparation.
