# crab-server

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-server/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns the repository HTTP application, native Git endpoints, authentication, and embedded React assets. It composes remote-git/read/write/storage APIs; shared Git and storage mechanics remain in their owner crates.

## Read first

Read `crates/crab-server/CELLAR.md` for native Cellar storage, lifecycle,
and migration limitations.

1. `crates/crab-server/src/main.rs` — thin binary entry; `src/cli/mod.rs` owns command parsing and dispatch, with ECS discovery and CLI tests beside it.
   `src/cli/setup.rs` owns the private single-container configuration and stable
   peer identity; `deploy/entrypoint.sh` supervises its server/proxy lifecycle.
2. `crates/crab-server/src/server/mod.rs` — `serve`: process composition and ordered shutdown; `src/server/http/mod.rs` composes routes and `src/repository/mod.rs` owns pinned reads and maintenance.
3. `crates/crab-server/src/app.rs` — `admit / repository`: request repository/principal boundary and error mapping.
4. `crates/crab-server/src/auth/mod.rs` — `Authentication`: identity and configuration; `auth/principal/` owns credentials and UUID-bound current grants; `auth/state.rs` owns durable sessions, `auth/http/mod.rs` owns login/session endpoints, and `auth/transport.rs` bounds provider requests.
   `auth/deploy_tokens/` owns administrator issue and revoke plus Git-only
   authentication of read-only repository credentials. The repository Cell
   stores versioned digests and expiry. Authentication verifies each transport
   request before binding the principal to that repository UUID.
   `auth/automation.rs` binds Git-only credentials to an Organization Cell,
   repository UUID, and current ownership epoch. Its independent grant stays
   out of human membership and ruleset bypass. The Organization Cell owns
   issuance, token ceilings, revocation, and audit.
   `auth/accounts.rs` owns durable suspension revisions, with mutations exposed
   through private `server/management/accounts.rs` and the `accounts` CLI.
   `auth/scim/` owns the bearer-only user directory and its durable pending
   change. Reconciliation applies managed state through the same account
   revision authority while preserving independent operator holds.
   `auth/scim/groups/` owns the durable group roster and link intent.
   `cells/organization/teams/sync.rs` versions and projects membership into the
   ordinary roster, checking the current owner on link changes and fencing
   delayed refreshes by link generation and group revision.
5. `crates/crab-server/src/git/receive/mod.rs` — `receive / publish_objects`: bounded native receive and publication orchestration.

Trace one path: `crates/crab-server/src/main.rs` → `src/cli/mod.rs` → `serve` in
`crates/crab-server/src/server/mod.rs` → router/handler composition.
Receive continues through `crates/crab-server/src/git/receive/mod.rs` to the
shared publication owner `crates/crab-write/src/journal.rs`.

The Cell repository facade in `src/cells/repository/mod.rs` exposes typed
operations. `models.rs` owns common records/inputs; `issues/` owns issue and
comment operations, `reads.rs` owns snapshot presentation reads and `lifecycle/`
owns canonical close reasons/times and explicit bounded preparation.
`api/issues/templates/` owns the default-branch template chooser, bounded Git
file reads and signed defaults for issue submission. Inheritance uses a
readable `.github` repository in the same namespace and binds the proof to
that source. The ordinary native issue creation transaction owns the receipt.
`issues/pins/` owns the independently versioned, three-slot
repository pin collection and permanent submissions; `api/issues/pins/` enriches
its bounded current issue summaries and composes current Write capability. Tests stay
with that owner. Private preparation is `cells prepare-issue-state`. Feature commands and their persistence helpers live under
`checks/`, `labels/`, `statuses/`, `settings/`, `releases/`, and `pulls/`. Pull
readiness belongs to `pulls/readiness/`, reviewer invitations and fulfillment
belong to `pulls/review_requests/`, bounded reviewer summaries to
`pulls/reviewers/`. `pulls/rollups/` owns bounded current-head check/review summaries;
`api/pulls/rollups/` captures selected source refs once for both list kinds and
checks source versions before composition. Outstanding request counts live in
the transactional discussion projection. `pulls/reviews/` owns submitted review commands; `reviews/dismissals/` owns
canonical dismissal SQL, effective detail reads, and permanent recovery. Public
review presentation uses these native records directly in `api/pulls/reviews/`.
`cells/repository/pulls/head.rs` commits and records a PR head switch.
`api/pulls/branch_update/replacement.rs` recovers that switch from the Git
publication receipt and holds both comparison tips and the new ref through
the Cell mutation.
`cells/repository/pulls/auto_merge.rs` owns durable PR auto-merge requests.
`cells/repository/collaboration/links/closure.rs` captures opt-in issue closures
under the Git publication policy lease and applies them at native merge
completion. `links/keywords.rs` interprets closing references in Markdown prose.
`links/commits.rs` reserves issue closures discovered by the exact default-ref
visibility walk and applies them once the publication receipt proves the ref
edit. `repository/commit_closures.rs` reconciles live and interrupted writes.
The discussion index projects pending references by base ref and completed
references from the publication snapshot used for receipt recovery.
`api/pulls/auto_merge/worker.rs` sweeps waiting requests and uses the standard
merge admission and publication under the per-PR merge lease. FIFO queue
entries share this worker. The Cell assigns order per target branch, admits
tests within the configured build window, and guards merge reservation for
the first entry. `auto_merge/queue.rs` reconciles candidate checks and retries.
An administrator's priority change advances the branch queue's order
revision in the same Cell transaction as the move. Candidate sources, CI
admission, and begin-merge compare that revision so a stale worker cannot
publish or remove an entry for a superseded order. A pending publication
prevents reordering until it settles.
Classic branch protection and active branch rulesets can require queues.
Classic transitions share the branch ref lease with publication. Ruleset
settings and final branch publication share a repository or organization
policy lease after any ref leases. Default-branch changes hold the same lease
because they affect ruleset matching.
`api/pulls/candidate.rs` prepares and describes the exact commit used for
prospective CI. The build prefix must connect current candidate refs to the
live base. Settings revisions bind queue entries to their effective method,
including after removal of an exact branch override.
Linear candidates carry source comparison metadata on their tip. Git receive
owns the reserved ref and leases the target, prospective parent, and PR head.
`api/integrations/` binds signed workflow reports to the current candidate.
`collaboration/mentions.rs` queues mentions, review requests, and assignments
in the native transaction that writes the source activity. `server/mentions.rs` expands current eligible
recipients and delivers to the Directory inbox before acknowledging the event.
`api/notifications/` rechecks access before exposing source links or titles.
`cells/directory/notifications/email.rs` owns the verified address binding,
versioned opt-in, and transactional email queue. `server/email.rs` rechecks
current delivery authority and sends generic inbox links with durable
provider idempotency keys. `server/email/transfers.rs` discovers committed
personal transfer offers from the catalog. Directory receipts fence replay,
and the sender checks the exact pending offer again before delivery.
`cells/directory/organizations.rs` stores account-to-organization candidates
before bootstrap or invitation commits. Live Organization Cell reads own
disclosure and email eligibility. Discovery starts with fresh organization
state. `cells/organization/repository_policy.rs` owns repository creation and
baseline access policy. Git publication and policy writes share the existing
organization ruleset lease.
`cells/repository/integrations.rs` owns GitHub Actions connections, delivery
receipts, bounded accepted-delivery history, and workflow attempt ordering. It
reads active providers and their exact-commit results from one snapshot for merge
admission. `api/integrations/`
owns signed GitHub deliveries, operator-bound workflow dispatch, and scoped Git
checkout. The operator App webhook authenticates before resolving an exact
installation/repository/workflow binding. The Repository Cell keeps standalone
and App results separate so a signing authority change requires a new result. Dispatch reservations and run binding live in
`cells/repository/integrations/dispatches.rs`.
`cells/repository/webhooks.rs` owns destination-bound subscriptions, leased
deliveries, and bounded completed history. `server/webhooks.rs` rechecks each
approved destination before signing and sending. Public Git ref writes from HTTP Git and the browser
reserve before publication and recover through the immutable plan receipt.
`webhooks/sync.rs` expands committed branch updates into signed PR deliveries
using bounded open-pull pages. A private fork reserves this work in its upstream
Cell. Page advancement and delivery records commit together. PR mutations queue
lifecycle events in their Cell transaction.
Private `cells prepare-review-state` installs the empty feature relation on
existing repositories, without rewriting reviews or receipts. `reviews/pending.rs`
owns private drafts and atomic review publication through the existing review
and thread writers. Review threads belong to `pulls/review_threads/`.
`api/pulls/review_threads/suggestions.rs` builds a single expected-head Git
commit from versioned review suggestions. The actor and exact batch bind the
Capsule plan. Retry reads its durable branch receipt before checking live tips.
`recent_pushes.rs` keeps bounded actor-scoped native push intents.
`git/receive/recent_pushes.rs` records candidates before Git publication.
`api/pulls/recent_pushes.rs` checks committed receipts and live ref transactions
before offering comparison links. Intent recording is advisory for Git receive.
`pulls/branch.rs` owns open-comparison lookup by repository-qualified head and
base. PR creation checks it inside the Cell transaction and records duplicate
rejections through the existing submission ledger.
`pulls/viewed.rs` owns actor-scoped review progress,
bound to stable comparison file identities. Browse projection reads and
writes belong to `projection/`. `dispatch.rs` alone decodes operation IDs. `search/` owns the shared issue/PR
query grammar; `search/results/` owns tagged issue/PR results and the mixed
list operation; `search/counts/` owns exact integer comment/reaction/interaction
comparisons and ranges over the transactional index; `search/navigation/` owns query edits and state destinations;
`search/index/` owns transactional projections, full counts,
sorting/query-bound cursors, and explicit bounded preparation; `search/completion/` owns repository-scoped
suggestions and browser UTF-16 replacement ranges. `search/milestones/` owns
milestone projection/catalog reads, grammar-preserving facet destinations and
the native choice read. `search/people/` owns qualified identity digests, public name validation and
shared resolved bindings. `search/authors/` owns global author-facet destinations
and personal-view selection. `search/contributions/` owns per-contribution qualified
identities, text/FTS maintenance, canonical deletion triggers and bounded five-source rebuilding;
`search/text/` owns field scopes, text predicates and relevance SQL;
`search/reviews/` owns reviewer predicates, transactional
review/request projections and bounded rebuild source cursors. Team predicates
project pending PR requests under the same index generation and use current,
viewer-scoped organization bindings from `api/discussion_search/review_teams.rs`.
Its `status/`
owner binds source epochs and effective branch rules for repository-wide review
status predicates and grammar-preserving menu choices; tests stay beside it.
`search/assignees/` owns stable-ID predicates, resolved binding validation,
historical snapshot choices and personal selection; `api/discussion_search/assignees/`
resolves handles/current accounts and composes authorized filter choices.
`search/labels/` owns bounded active-label choices and
include/exclude/no-label transitions; it reuses the canonical label decoder.
`search/navigation/` shares global-conjunction extraction, bounded destinations
and choice records between facets and state controls. Canonical milestone
commands update that projection in their transaction; assignment changes dirty
the discussion projection. The HTTP
choice endpoint stays with other search reads in `api/discussion_search/`.

`pulls/lifecycle/` owns canonical close/merge-completion times and a consistent
pull-detail read. `collaboration/lifecycle/` shares bounded explicit preparation
with issue lifecycle metadata; each owner recovers only matching transition events.
Private `cells prepare-pull-state` prepares existing native repositories.

Collaboration documents live in `cells/repository/collaboration/`. `creation/`
owns initial metadata validation, stable selection digest terms, terminal metadata
rejections and canonical initial milestone assignment. Both create writers commit
labels/assignee snapshots at version one; `api/discussion_creation/` resolves the
current roster and triage capability. Its tests stay with those owners. `state/`
owns atomic close/reopen with optional comments; `bulk/` owns bounded mixed-kind
selection and atomic metadata/state edits. `submissions/` owns their shared
applied/rejected decision storage with issue pins. Its `preparation/` alone reads
historical successful documents in bounded frames. Existing issue/pull state
preparation commands also prepare these ledgers; ordinary mutations require their
prepared format. Nested SDK scopes discard domain work before a terminal
rejection commits. State format-2 records remain unchanged by this extraction.
`api/discussion_bulk/` binds repository UUIDs, resolves current roster eligibility
and owns the bulk HTTP contract. It holds the organization policy lease when
applying issue types; the repository command checks the captured type assignment
versions and reuses the canonical issue-type projection.
`collaboration/locks.rs` owns versioned issue and PR conversation locks, their
deadlines and activity. `search/index/locks.rs` projects the persisted records
for indexed filtering. Ready format-14 indexes copy the lock records through
bounded format-15 preparation frames. Posting, edit, review batch, state-with-comment and reaction commands
read the parent lock before mutating. Published submission receipts remain
recoverable. `api/discussion_lock/` resolves current role authority for locks.
`collaboration/minimization.rs` owns per-contribution visibility and activity.
The API checks the same discussion moderation roles, and contribution responses
read visibility in the batched reaction summary query. A hidden review stays part
of the merge decision and a hidden thread stays part of conversation resolution.
`collaboration/edit_history.rs` records prior text and attributed tombstones.
The native body and inline suggestion writers own history capture. The HTTP
owner enriches editors and checks current membership before returning text.
Retained text is available only through the addressed revision's authorization.
`auth/oidc/` owns verified signed group claims and their bounds; group-backed
sessions have a fifteen-minute ceiling. `auth/principal/` resolves individual/group
role union against current repository ACLs and intersects Git token scope.
Mention autocomplete probes durable sessions for qualified profile candidates
and applies the same group grant rule against the latest repository ACL.
`catalog/` owns atomic complete ACL edits and the audit outbox; `api/members/`
and `packages/ui/src/features/settings/groups.tsx` compose access administration. SSO groups do
not grant native Organization or team authority. Tests stay with these owners.
`api/organizations/repository_access.rs` composes an owner-only member
grant inspection from a catalog revision and the Organization Cell's current
team memberships. Its native read shares the same team-epoch predicate as
repository authorization. The public view omits provider identity and claims.
`api/organizations/audit/` pages and exports owner-only audit history from
immutable Organization events and catalog membership events. `catalog/audit.rs`
records event-time scope and chain links in the catalog CAS outbox. Never
infer a historical organization from a repository's current placement.
`api/catalog.rs` composes personal and organization repository creation using
the durable catalog reservation and shared Git and Cell initializer.
`cells/organization/repositories.rs` records owner-authorized submissions.
The catalog's explicit organization UUID owns the repository relationship.
`api/repository_transfer/` checks the current source and destination authorities.
Its incoming view reads pending offers from the current catalog and applies
recipient and organization-owner checks independently of source access.
`catalog/transfer.rs` commits consent, the complete address and grant change,
and the membership audit together. `catalog/rename.rs` owns historical addresses.
`repository/access.rs` resolves current owner grants, and `app::membership`
applies them alongside repository members and groups across browser and Git.
`git_import/jobs.rs` owns actor-scoped durable import status and worker heartbeats.
`api/write.rs` owns the shared proven-rejection response marker.
`api/discussion_state/` owns current membership and state HTTP composition.
`server/http/boundary.rs` binds browser mutation headers to the stable repository
UUID before domain effects; the public catalog exposes that UUID. The admitted
principal retains its repository constraint across route/grant refreshes, including
local trust and case-insensitive membership resolution.
Plain and combined comments share their canonical issue/PR writers. Its `target.rs`
owns shared discussion addresses, `activity/` owns append-only events and subject
indexes, and `reactions/` owns actor membership/counts. Activity append calls stay
inside domain commands so their SQL/KV changes and events commit together.
`conversation/` owns the chronological contribution/event reference index,
bounded before/after/focus reads, and explicit restartable preparation under the
shared discussion-index lifecycle. `api/conversation/` enriches these native
windows with public authors, reactions and selected-head review freshness.
`links/` owns versioned issue/PR relationships and both subject indexes; its
single activity event is indexed into both timelines.
`dependencies.rs` owns the versioned issue graph, its active direction indexes,
and bounded cycle checking. It shares one dependency event between both issues
and computes blocked status for issue and Project reads in the source snapshot.
`duplicates.rs` owns typed duplicate marks and event-bound undo; the canonical
issue and PR comment writers call it before their native receipts commit.
`collaboration/mentions.rs` records Markdown mentions with the source body.
`collaboration/mention_settings.rs` owns versioned aliases. `server/mentions.rs`
delivers the durable outbox to `cells/directory/notifications.rs`.
`collaboration/watches.rs` owns private repository and discussion preferences
and their active watcher indexes. `collaboration/watches/discussions.rs`
reads current follows by paging the canonical discussion keys.
`collaboration/watches/chronology.rs` owns the private origin-time index.
`api/notifications/discussions/timed.rs` merges authorized Repository pages.
`collaboration/watches/publications.rs`
owns release watch activation and the publication outbox. Release completion
queues the notice in its publication transaction. `api/releases/notes.rs`
composes canonical completed pull merges with a pinned Git ancestry range
for the editable release preview. Its `notes/configuration.rs` reads categories
and exclusions from that target commit and resolves current label and author
identities. Activity writes enroll participants under
their qualified identity before scheduling delivery. Watch outbox events carry
the activity number and advance by bounded pages after Directory delivery.
The mention outbox pins enrollment sources to committed activity and gates
later watch pages until the inbox delivery and follow commit. The Directory
groups inbox entries by typed discussion. Issue transfer receipts capture
mentions from the source description. Destination publication enqueues them
in the same transaction that installs the issue. Transfer inbox entries
carry their source repository and require current grants to both repositories
for delivery, viewing, and triage. An opted-in email uses the same notice ID.
The sender rechecks both grants and the destination issue before sending an
authenticated inbox link. `api/issues/transferred.rs` follows committed issue
moves to the published location and rechecks every traversed repository grant
before disclosure. `notifications/triage.rs` binds all
selected rows to observed activity in one transaction. `notifications/filing.rs`
retains saved and completed conversations with private activity-order indexes.
`notifications/filters.rs` owns account-private, revisioned inbox filters.
Applying a filter uses the ordinary inbox query and its current repository
grants. The filter stores repository and organization IDs without labels.
Author filters bind the viewer or public actor ID to a qualified identity.
The Repository Cell matches the original discussion or release author.
`notifications/retention.rs` sweeps expired completions and fences replay.
`collaboration/watches/inbox.rs` commits inbox dismissal with the Repository
watch preference. `api/notifications/` checks repository access and mute state before listing
entries or changing their state.

The native runtime lives in `src/cells/runtime.rs`; `src/cells/client.rs` owns
the authority-checked `CellClient` and target-bound read receipts.
`src/cells/application.rs` owns shared `Transaction`, replay dispatch and the
output gate. `src/cells/address.rs` identifies Repository, Directory and Organization targets;
commands and reads declare their native class. Repository initialization lives
in `src/cells/repository/schema.rs`, using the canonical `src/cells/schema.sql`.
`cells/repository/assignments/` owns stable actor references and name snapshots;
its `migration/` alone reads retired subject bytes and performs bounded explicit
conversion. `api/members/roster.rs` binds current member UUIDs to qualified private
accounts and public candidate profiles for assignments and reviewer selection.
`api/pulls/reviewers/` composes current-head status and public participant pages;
`repository::ref_oid` uses the selected-ref data plane without opening packs.
`api/assignees/resolution.rs` owns assignment selection limits and historical
assignee presentation. Both refresh grants after Directory reads. Private
assignment inventory/migration live in `server/management/assignments.rs` and
`cli/assignments.rs`. `src/cells/directory/` owns qualified private account identities and optional
public profiles. `accounts/` owns stable UUID registration; `profiles/` owns
chosen names/handles and immutable submissions. `migration/` alone reads format 1,
with bounded, restartable format-2 conversion and colocated historical-input tests.
Private status/migration/registration are in `server/management/directory.rs` and
the `cells` CLI. Registration does not grant permissions.
`cells/directory/lookup/` owns bounded qualified account-ID reads and public
profile reads by qualified key or actor UUID. `cells/directory/search/` owns bounded indexed handle resolution
and completion. Handle completion returns bounded public matches plus an exact
truncation flag. `api/discussion_search/people/` resolves the shared author/reviewer/commenter name union
before native reads and composes public choices with final membership checks.
`api/discussion_search/authors/` and `reviews/` own their filter endpoints. `api/profiles/attribution/` deduplicates discussion authors into one
Directory read for lists, details and conversation responses; handlers recheck
durable membership afterward. `api/issues/view.rs` owns issue/comment JSON
presentation. Issue content edit admission separates author or Moderate
authority from Triage state changes. The native transaction attributes edits
to the current editor while preserving the original author.
`cells/directory/saved_replies.rs` owns account-private versioned snippets.
Its HTTP handlers derive the account from the browser session.
`cells/repository/collaboration/attachments.rs` owns draft reservations,
claims in the discussion transaction, and sealed cleanup records.
`api/discussion_attachments/` owns current grants, bounded transfers,
and expiry collection. It shares staging and hashing with release assets
through `api/asset_transfer.rs`.
`api/discussion_search/composer.rs` resolves cross-repository references
against the target grant and opens UUID-bound typed links.
`cells/organization/` owns native organization metadata, membership tombstones,
versioned invitation decisions, indexed roster pages and transactional audit.
`cells/organization/teams/` owns standalone teams, current-incarnation direct
memberships, visibility/creation policy, bounded explicit preparation and permanent
applied/cancelled submissions. `api/organizations/teams/` owns its browser contract
and revision-checked profile enrichment. Existing organizations require private
`cells prepare-organization-teams`; ordinary serving never installs its feature DDL.
`api/organizations/` owns account-to-actor resolution, late permission checks after
Directory enrichment and browser routes. Explicit first-owner bootstrap belongs
to `server/management/organizations.rs` and `cells bootstrap-organization`.
Membership checks run inside the shared native transaction before receipt replay;
repository grants stay catalog-owned. Public sessions cannot bootstrap authority.
`catalog/` binds repository and organization UUIDs and audits grant replacements.
`auth/principal/` caps effective roles by credential scope.
`app.rs` composes current native team and catalog grants for every request.
`api/members/team_access.rs` authorizes team grant management for current
organization owners. The native team membership and grant-name read stays in
`teams/`. The owner editor lives in `packages/ui/src/features/settings/team-access.tsx`.
`src/api/profiles/` exposes session-authenticated profile reads/edits without
exposing authentication claims. `src/server/management/directory.rs` owns explicit
private initialization; serving never creates its schema implicitly. HTTP authorization and
Cell error mapping stay in `src/app.rs`. No previous-runtime readers or catalog
and session migration paths are supported.

## Module layout

- Use `<component>/mod.rs` for every component with child modules; keep its
  implementation, submodules, and tests in that directory. Avoid custom `#[path]`
  mounts and sibling `<component>.rs` entry points.
- `api/` owns browser HTTP feature handlers and route assembly; pull merge-tree
  mechanics live in `api/pulls/merge/`, comments in `api/pulls/comments.rs`,
  replies in `api/pulls/review_threads/replies.rs`, and thread persistence in
  `api/pulls/review_threads/`. `git/` owns native Git, object,
  receive, and LFS endpoints and their route assembly. `auth/` assembles login
  and session routes. `server/http/mod.rs` applies the common security boundary
  after merging these routers. Shared request policy remains in `app.rs`.
- `catalog/rename.rs` owns the versioned name and redirect transaction.
  `api/repository_name/` binds the administrative action to the durable
  repository UUID. Renames and name-changing transfers check the current
  name's active rules under the source policy lease before catalog publication.
  `server/repository_route.rs` rechecks current authority
  before serving historical addresses from the shared catalog.
- `api/repository_metadata/` authorizes description and topic edits under
  the catalog CAS. The original description stays bound to creation replay.
  Maintain can edit repository metadata and ordinary PR merge options. Queue
  and automatic head deletion settings stay with the administrator boundary in
  `api/pulls/merge_methods.rs`. `api/pulls/head_branch/` shares the publication
  boundary for manual and automatic deletion. The merge transaction promotes
  cleanup to pending work after Git publication. The worker rechecks the source
  setting generation and visible ref transaction under Git publication leases.
  `head_branch/stacked/` reuses the base change transaction for stacked pulls.
  Stock Git and the branch list resolve a base only from same-repository merges
  of the branch and exact commit being deleted. The branch list uses the
  existing pull request deletion action with its confirmed impact. Git pushes
  lease the combined ref set and commit all dependent comparisons together.
- `api/catalog_browse/` ranks and pages catalog matches. Reuse `api/catalog.rs`
  for current direct, group, organization, and fork grants and topic metadata. Browse reads
  lifecycle settings only for the result page and rechecks access before
  returning results and continuations.
- `api/code_search/` searches authorized Git snapshots under bulk read
  admission. Repository searches bind the UUID, commit, and query filters.
  `api/code_search/qualifiers.rs` owns the bounded Boolean grammar, term
  predicates, and regex compilation. `code_search/evaluate.rs` evaluates the
  same query at repository, path, index, and verified Git boundaries. Keep
  unknown evidence under negation and apply trigram pruning only to byte runs
  required by every match after parsing flags and Unicode case folding.
  The indexed reader bounds path rows for Boolean and regex queries.
  Workspace search reuses the catalog's current grants and the same bounded
  Git scans. Its cursor also binds the actor and readable repository scope.
  `repository/code_index/` publishes the bounded, checksummed default-tree
  artifact through the background lifecycle. It stores paths and separate text
  and definition filters. Search verifies candidates against the pinned Git tree
  and reparses definition locations using `code_index/symbols.rs`. Reuse filters
  by blob and parsing language. Change the artifact version if parser upgrades
  change which definitions the filters can rule out.
  Missing or stale derived data leaves the authorized Git scan available.
- `api/contracts.rs` exports the public identity/access/organization/team wire
  types to `packages/ui/src/lib/contracts.ts` during tests. Update the actual
  Serde owner, regenerate with `npm run contracts:generate` in `packages/ui`,
  and verify with `contracts:check`; never hand-edit generated declarations.
- `repository/` owns pinned reads, catalog materialization, maintenance, and
  browse projections. Its `settings.rs` supplies policy to both browser and
  native Git paths. `catalog/` owns durable catalog persistence and membership.
- `peer/` owns private transport with TLS in `peer/tls/`; `server/` composes these
  owners and manages ordered startup/shutdown.
- Keep unit tests with their module. Put larger suites in `<component>/tests/`;
  share only reusable setup through `test_support/`, gated by `cfg(test)`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| HTTP receive | `crates/crab-server/src/git/receive/mod.rs` | `crates/crab-write/src/journal.rs` |
| Content reads | `crates/crab-server/src/api/contents.rs` | `crates/crab-remote-git/src/snapshot/mod.rs` |
| Auth or routing | `crates/crab-server/src/server/http/mod.rs` | `crates/crab-server/src/server/http/boundary.rs`, `src/auth/mod.rs` |
| Read readiness | `crates/crab-server/src/repository/mod.rs` | `src/server/background.rs`, `src/repository/maintenance.rs`, `crates/crab-write/src/generation.rs` |
| Management and capacity | `crates/crab-server/src/server/management/mod.rs` | `src/server/capacity.rs`, `src/metrics/mod.rs` |
| Git imports and private organization forks | `crates/crab-server/src/git_import/mod.rs` | `src/git_import/source.rs` (validation), `src/git_import/git.rs` (process isolation), `src/git/receive/fork_lfs.rs` (fork LFS copy), `crates/crab-remote/src/prepare/capsule.rs` (shared Xet verification), `src/catalog/mod.rs` (fork lineage and access) |
| Fork branch synchronization | `crates/crab-server/src/api/fork_sync.rs` | `src/git/receive/fork_sync.rs` (upstream pack and branch lease), `src/git/receive/publish.rs` (policy and receipts) |
| Fork pull request merges | `crates/crab-server/src/api/pulls/merge/mod.rs` | `src/git/receive/fork_merge.rs` (exact fork dependencies), `src/git/receive/publish.rs` (upstream policy and both ref leases) |
| Commit review suggestions | `crates/crab-server/src/api/pulls/review_threads/suggestions.rs` | `src/git/receive/publish.rs` (target lease and fresh review checks), `packages/ui/src/features/pull-requests/reviews/suggestion.tsx` (destination and commit dialog) |
| Browse APIs | `crates/crab-server/src/api/mod.rs` | `src/api/query.rs` (Git queries), `src/api/representation.rs` (encoding/ranges) |

## Invariants

- Keep transport identity/session checks and repository authorization before storage or Git effects; inspect auth and app boundary together.
- The `provider-cdn` qualification feature is operator-owned and bound to an
  exact native catalog placement. Attach it to verified browse and upload-pack
  readers. Catalog records, Cellar, receive, and maintenance use origin layouts.
  Source: `crates/crab-server/src/app.rs`.
- Git receive validation, publication outcome, and protocol acknowledgement are distinct steps; trace validate/publish and fault tests before changing error mapping.
  Source: `crates/crab-server/src/git/receive/mod.rs`.
- Repository operations and maintenance share long-lived runtime ownership; preserve explicit shutdown/draining when changing listener lifecycle.
  Source: `crates/crab-server/src/server/mod.rs`.

## Features and platform

No declared Cargo features. Both library and auto-discovered main binary exist. `crates/crab-server/build.rs` requires packages/ui/dist/index.html and rejects symlink assets. Build the frontend before any Cargo check/test/build for this crate; Node requirements are in its README.

## Verification

Tests live under their owner: `api/tests/`, `auth/tests/`,
`api/<feature>/tests.rs` (larger suites such as membership use `tests/mod.rs`), `auth/<feature>/tests.rs`, `catalog/tests.rs`,
`cells/repository/<feature>/tests.rs`, `cells/directory/tests.rs`,
`cells/directory/profiles/tests.rs`, `git/receive/tests/`, `git/lfs/tests.rs`,
`cells/directory/lookup/tests.rs`, `cells/directory/search/tests.rs`, `cells/organization/tests/`,
`api/organizations/tests/`,
`cells/repository/search/authors/tests.rs`, `api/discussion_search/authors/tests.rs`,
`api/profiles/attribution/tests.rs`,
`api/pulls/tests/`, and `repository/tests/`. `server/peer_e2e_tests.rs`
exercises process composition and native peer handoff.
Shared setup lives in test-only `src/test_support/`; feature suites never import
fixtures from another suite. HTTP fixtures and their deterministic identity
provider belong in `test_support/http/`. Git commands/history belong in `test_support/git.rs`,
server/Cell lifecycle fixtures in `test_support/server.rs`, peer identities
in `test_support/tls.rs`, and archive-race recovery checks in
`test_support/archive.rs`.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
npm ci --prefix packages/ui
npm run build --prefix packages/ui
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-server --locked --lib auth::http::tests
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-server --locked --lib git::receive::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-server/README.md` — quick start and reading routes.
- `crates/crab-server/REFERENCE.md` — detailed contracts and qualification history.
- `crates/crab-server/Cargo.toml` — dependency and feature authority.

Read `crates/crab-server/build.rs` and `packages/ui/package.json` for embedding changes. Dedicated image/service proof: `.github/workflows/http-server-container.yml`; native fetch proof: `.github/workflows/git-protocol-v2-partial-clone.yml`. Browser UI changes require their own frontend checks. The explicitly ignored
`api::organizations::tests::browser` qualification requires `agent-browser` and
built embedded UI assets; it drives two real OIDC sessions through invitation,
consent and removal against isolated native Cells.
`api::members::tests::team_access_browser` uses the same browser fixture to
verify repository access before a grant, after saving it, and after revocation.

`tests/qualify_cellar_kubernetes.py` runs the Helm chart, container SIGKILL,
large-file transfer and complete-root restore on an explicitly selected
three-node Kind cluster. Local setup and proof limits live in `deploy/operations.md`.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.

`cells/repository/projects/` owns repository project definitions, typed fields,
saved views, item membership and draft conversion. Its `hierarchy.rs` owns
versioned issue parents, cycle/depth checks and canonical child progress;
`preparation.rs` alone installs the new feature for existing repositories.
`api/projects/` owns its repository-authorized HTTP routes and read enrichment
boundary. Native and HTTP tests stay with those owners. Private preparation
reuses `server/management/lifecycle.rs` and `cli/lifecycle.rs` through
`cells prepare-projects`. The browser owner is `packages/ui/src/features/projects/`;
its account/repository-bound command provider stays above route loading.

`cells/organization/projects/` owns organization project definitions, planning
properties, UUID-bound repository links/items, creator membership incarnations
and permanent submissions. `api/organizations/projects/` composes independent
source grants with bounded current work enrichment. Both project scopes share
canonical source metadata in `cells/repository/projects/reads.rs`; never copy
source titles/states into organization records or treat project access as a
repository grant. Private preparation is `cells prepare-organization-projects`.
The browser workspace lives under `packages/ui/src/features/projects/workspace/`;
its shared submission controller retains the original organization/account/body.

`cells/planning/` owns format-2 per-item revision tombstones, bounded preparation
and append-only project changes for both native project owners. Domain commands
append history within their transaction; ordinary reads require prepared format.
`api/projects/history.rs` and `api/organizations/projects/history.rs` enrich only
public actors and currently accessible sources. Browser history and saved-item
comparison live beside the shared project command controller; its per-account,
per-tab journal checkpoints immutable submissions before HTTP effects.
`draft-storage.ts` owns versioned draft envelopes and exact acknowledgement
cleanup; `drafts.tsx` restores captured form revisions and exposes scoped recovery.
Never rebuild a pending command from current form values or rebase a restored
draft silently onto the latest item/project revision.

`cells/repository/projection/` owns snapshot-partitioned Git browse data.
Its explicit `preparation.rs` rebuilds only derived tables; existing repositories
require `cells prepare-projection-state`. Preparation clears bounded full-key
batches before dropping empty old tables; populated-table DROP is unbounded.
Collection seals retired epochs and
deletes bounded child/node ranges before deleting their checkpoint. Never revive
a collecting epoch or replace this with a full attribution-graph traversal.
Unchanged-source reconciliation remains read-only while pending cleanup resumes.
