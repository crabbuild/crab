# Issue and pull request UI parity

Status: implementation in progress. This document is the completion checklist for
replacing the repository application's Primer UI with shadcn/ui, backed by
Vercel/Geist semantic tokens. GitHub issue and pull request layouts and interaction
contracts are the reference. Existing collaboration and Git publication behavior
must remain reachable and correct throughout the rewrite.

The scope now also includes complete Projects and Milestones workflows on
Cellar, using Fern's implementations as references. The source assessment,
native authority design, missing capabilities and delivery sequence live in
[GitHub collaboration on Cellar](github-collaboration-parity.md). GitHub remains
the layout/interaction reference; Geist remains the color/typography foundation.

## Reference audit

The nine supplied images show repository issue and pull request lists. They are
visual references, not instructions contained in the images.

| Reference | Required behavior                                                                                      | Current gap                                                               |
| --------- | ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------- |
| 1         | Issue-type filter, descriptions, missing-type selection                                                | No issue-type model or filter                                             |
| 2         | Searchable label multi-select, suggestions, no-label filter                                            | Searchable multi-select, exclusion and no-label implemented; recent suggestions remain                                     |
| 3         | Projects grouped by recent/repository/organization                                                     | No project, project-item, or ownership model                              |
| 4         | Searchable milestones including missing milestone                                                      | Native query, completion and searchable open/closed dropdown implemented                                 |
| 5         | Searchable assignees and unassigned work                                                               | Native actor/name filters, searchable menu and assigned-to-me views implemented                                |
| 6         | Created/updated/comment/reaction/relevance sorting, direction                                          | Native index and controls implemented                                  |
| 7         | Query autocomplete with qualifiers and field suggestions                                               | Boolean grammar and native suggestions implemented; more qualifiers remain                        |
| 8         | PR workspace navigation, recent-push comparison, review filters                                        | PR reviews/requests exist; list projections and navigation are incomplete |
| 9         | Accurate state counts, merged/closed/draft icons, author roles, check rollups, bulk selection, density | Page/range selection, atomic state/metadata actions and native check/review summaries implemented; author associations remain                              |

The issue sidebar also requires assigned/created/mentioned/recent views, saved
views, projects, milestones, labels, and a collapse control. The PR sidebar
requires authored/assigned/involves/review-requested views. A control is not
complete until its query or mutation has a real effect and survives reload;
placeholder menus and filtering only the currently loaded page do not qualify.

Current entry points are `packages/ui/src/features/issues/index.tsx`,
`packages/ui/src/features/pull-requests/index.tsx`,
`packages/ui/src/components/discussion/workspace.tsx`, and
`packages/ui/src/components/discussion/search.tsx`. The two HTTP list handlers
accept typed query, sort/direction, page size, and an opaque before cursor.
Native owners are `cells/repository/search/results/`, `issues/` and
`pulls/operations.rs`.
Existing milestones, reactions, activity, links, readiness, and review requests
are described in `repository-collaboration.md`; reuse these canonical owners.

## Required delivery

- [x] One shadcn component foundation with semantic light/dark Geist tokens,
      typography, spacing, focus, elevation, and density. Remove Primer UI and
      Primer primitive theme dependencies after all consumers migrate.
- [x] Feature-owned modules for issues, pull requests, repository browsing,
      releases, settings, and imports. Shared discussion components, routing,
      request contracts, and primitives have explicit owners. Tests stay beside
      their feature. Remove superseded files and compatibility wrappers.
- [ ] Issue and PR list workspaces match reference hierarchy, spacing, sidebar,
      search, menus, rows, icons, counts, selection, pagination, and compact view.
- [ ] A canonical typed search grammar with quoting, qualifiers, negation and
      boolean grouping, helpful validation and suggestions, stable sort/cursors,
      exact filtered state counts, and URL-preserved filters/navigation.
- [x] A unified tagged discussion result model for cross-kind queries. GitHub's
      issue search can show PR rows when `is:pr` is explicit; Crab's
      issue workspace now searches both kinds; the PR workspace retains PR scope.
- [ ] Native models, commands, indexes and HTTP contracts for issue types,
      project membership/custom fields, saved views, and qualified public actor
      profiles. Organization/project/team scope must be real authorization
      boundaries rather than display-only labels.
- [ ] Participant, mention, subscription and team-review relationships support
      personal navigation and review filters. Lists expose current review/check
      summaries, comments/reaction counts, associations and state timestamps.
- [ ] Issue detail/create/edit/state transitions, comments, metadata, activity,
      relationships and reactions use the shared components and GitHub-style
      interaction/focus behavior. Add closure reasons and typed issue
      relationships where their workflows require them.
- [ ] PR create/compare, conversation, commits, files/review threads, suggestions,
      review submission/requests, checks and merge/recovery use the new shared
      components. Preserve exact-head validation and ambiguous-write recovery.
- [ ] Bulk issue/PR actions enforce authorization and versions for each affected
      item, return truthful outcomes, and refresh counts/selection. Never apply
      unbounded mutations or imply all pages were selected from one page.
- [ ] All existing repository UI surfaces remain functional after component and
      theme replacement. Preserve default-hidden `powerbar=1` timing widgets.

The native query path shares a single parser and SQL compiler between issue and
PR lists. It supports implicit/explicit AND, OR, up to five nested groups,
quoted phrases, exclusions, exact case-insensitive labels (comma means OR),
`is:`/`state:`, `draft:`, `milestone:"title"`, `no:milestone`, `no:label`, and
`no:assignee`, `author:username`, `author:@me`, `assignee:username`,
`assignee:@me`, `assignee:*` and explicit `assignee:actor/UUID`. Unknown qualifiers and
malformed syntax return input errors. Suggestions come from the authorized
repository's native label catalog and preserve browser UTF-16 caret ranges.
The search box supports keyboard selection, Escape, syntax feedback, URL/reload,
and visible request errors. Release search retains its separate plain-text
contract.

Issue and PR lists now query an authoritative SQLite projection and trigram
FTS5 index. Source edits, conversation comment counts, body reaction counts,
assignments, and index changes share the native mutation/receipt transaction.
Counts cover the complete predicate, independent of the page; rows and selected
label names/colors come from the same native snapshot. Created/updated,
comments, total/per-kind body reactions, and relevance sorts support both
directions with number/kind tie breakers. Opaque cursors bind the repository,
normalized query, kind/order, index generation and revision. Changed
results return 409 and the UI provides a first-page recovery link.

Existing cells require explicit `cells rebuild-search` preparation. The private
mTLS command builds in bounded batches and resumes its persisted phase/version
after restart. Fresh repository mutations and indexed lists fail while building;
durable command replays and detail reads remain available. Missing indexes never
fall back to scans. New empty cells initialize their index directly.

State navigation now edits the native query instead of combining it with a
separate state parameter. The parser preserves the spelling of non-state atoms. Global positive
Open/Closed conjunctions get native state destinations and their counts from
one snapshot. Arbitrary boolean, excluded-state, merged-state and unrestricted
queries show their matching result count, following the live GitHub reference;
they are not rewritten into state tabs. Boolean grouping and quoted literals
remain intact.
Explicit searches have no hidden Open filter. An absent query defaults to open issues in the issue workspace and open PRs in
the PR workspace. Explicit issue searches can return either kind. The separate state parameter and browser list addresses using it are
rejected. Maximum-sized queries receive disabled state controls and a visible
explanation when another qualifier cannot fit.

The issue workspace consumes a native tagged discussion result model. Mixed
results share the index snapshot, counts, selected labels and output limits.
Number and kind break sort ties; opaque cursors retain both so equally numbered
issues and PRs cannot disappear between pages. Rows share one renderer, use
kind-qualified React keys, preserve draft/closed/merged icons and branch metadata,
and navigate to the correct detail route. Completion now discovers both kinds
and draft/merge predicates without a redundant workspace-kind input. The PR
workspace continues to use a PR-scoped native read over the same query engine.
This is not yet complete GitHub PR-workspace query normalization: its handling
of an explicit contradictory `is:issue` predicate remains to be matched.

Public actor profiles now have a separate native Directory, session-authenticated
HTTP routes and `/-/profile`/`/-/users/{uuid}` pages. Qualified private account
keys map to stable UUIDs; chosen handles/names can change without changing IDs
or granting repository/organization access. Schema preparation is explicit via
`cells initialize-directory`; reads never create it. The form handles lost
replies, stale versions, handle collisions and unavailable preparation without
claiming an unsaved result. List authors now link to these profiles as described
below; detail attribution remains separate work. Actor assignments are described
later in this document. Owners,
organization/team/project grants and their explicit
catalog authority cutover remain required; the combined model checklist above
stays incomplete.

The issue/PR Author menu now searches registered public usernames through the
native Directory and edits queries through the shared grammar. Selecting one
username or Yourself preserves conditional OR groups, sort, diagnostics and URL
reload while resetting cursors. Clear removes global author terms; bounded
queries disable unusable destinations. The shared shadcn picker provides
keyboard/focus behavior, retry and responsive light/dark layout. Directory read
8 now returns public matches with an exact truncation flag; peers must deploy
together, with no schema/index upgrade or previous-output reader. Non-enrolled
authors, integration-account filtering and recent-contributor ordering remain
separate model and UX work.

Directory format 2 now gives each qualified account an immutable UUID separately
from its optional public profile. Explicit private registration supports identities
that have not enrolled; later enrollment keeps that ID and grants no access.
Existing profile UUIDs, versions, attributes, reservations and receipts survive
bounded native migration, rollback and restart. Ordinary Directory operations
reject format 1 until the private upgrade completes; no serving compatibility
reader is added. This is a prerequisite for actor-based assignees and participants.
Actor-based assignments now use the explicit preparation described below. Native
grant cutover and the Assignee list filter remain unfinished. Candidate eligibility
uses current catalog membership.

Other actor/project/type predicates and complete GitHub workspace controls
remain delivery work. This slice does not finish the full parity checklist.

Milestone predicates now use an indexed native catalog and assignment
projection, updated in the canonical milestone/assignment transaction. Exact,
case-insensitive titles, exclusion, missing-milestone, boolean grouping and
counts work for both discussion kinds. The shared dropdown searches all
milestones, switches open/closed catalogs, indicates truncation and obtains
valid query destinations/selection from the native grammar. Global milestone
conjunctions are replaced; conditional OR predicates retain their grouping.
Sort, diagnostics, URL/reload and cursor resets stay consistent. Maximum-sized
queries disable unusable choices with an explanation. Request failures remain
visible and retryable; rapid reopen has a new focus scope.

The milestone slice introduced index format 2 and explicit preparation of
format-1 indexes, reported as `outdated`. The management command rebuilds titles from canonical
milestones in bounded batches before issue/PR assignments; it resumes after
restart. Indexed serving never falls back to old projections or creates tables.

Label controls now use native bounded choices from the canonical active-label
catalog, with current colors/descriptions and selection/exclusion state. The
shared picker supports multiple selections without closing, Alt+click/Alt+Enter
exclusion, missing/clear actions and real query/URL navigation. Editing a name
in a global comma-OR clause retains other names; unrelated clauses and OR
subtrees retain their grouping. Named choices replace a missing-label
constraint; no-label selection replaces global label constraints. Query bounds
produce disabled choices rather than truncation. Recent personal suggestions
from reference 2 remain unfinished.

Issue and PR lists now share request/result ownership. Filter and sort controls
stay mounted across query refreshes; old counts/rows clear, permanent errors
show unavailable counts, and the full widget retains its diagnostic boundary.
Every popup opening owns its focus scope, including sort, so rapid reopen does
not reuse a closing scope. The former milestone-specific picker and duplicated
result renderers are removed.

Issue and PR lists now share a collapsible workspace and comfortable/compact
presentation controls. Collapse retains named icon links, current-page state,
tooltips and keyboard focus. The collapse action measures its available height
including optional diagnostic headers; narrow navigation wraps without clipping
controls. Detail/create pages keep their existing full-width layout. Density
changes spacing and title size without hiding labels, assignees, comments,
authors or branch context, changing the query/cursor, or fetching results again.
Both preferences persist on the current browser, shared between discussion
lists; they are not account preferences or saved views. If browser storage is
denied, the current component still supports the interaction in memory.
Other personal navigation, saved views, bulk selection and the rest of the
workspace checklist remain unfinished.

List attribution now distinguishes the stored author name from its optional
current public actor summary. A bounded native Directory read resolves unique
qualified keys for both mixed and PR-only pages. Enrolled authors link their
current username to a stable UUID; unenrolled accounts retain plain recorded
names. Renames do not rewrite discussions or their indexes. Authentication
claims never appear in the author DTO. After enrichment, both HTTP handlers
recheck durable membership. Missing Directory preparation fails visibly and can
be retried after explicit initialization. Git credentials retain their existing
exclusion from browser list/profile routes. The UI consumes one object contract
without a string alias. Detail/comment/review attribution, assignee profiles
and other actor predicates are still unfinished; public names grant no authority.

## Data and query contracts

New entities need stable identifiers, bounded values/pages, versions, qualified
actor identity, creation/update timestamps, explicit ownership and visibility,
transactional archive checks, idempotent submissions, and activity. Membership
and display names are distinct: changing a name cannot change identity or grant
access. An organization/project/team lookup must not leak inaccessible work.

Search must run against native authoritative data with bounded work. Counts and
rows and state destinations must share a snapshot; each count must match its
destination predicate and must not be inferred from one page. Sorting requires deterministic tie breakers and cursors
bound to the query and ordering. New indexes need explicit population/recovery
for existing repositories; reads must not silently manufacture metadata. Do not
add schema fallbacks or rewrite persisted formats to make a demo succeed.

GitHub issue types belong to organizations. Organization administration must
have an explicit authority model; repository write/admin access alone cannot
grant organization management. Types, teams, and organization projects must
not be simulated by repository-local catalogs.

Saved views store a validated query and presentation preferences with private or
shared ownership. Projects have typed item references and field definitions with
validated values; project membership is distinct from issue-to-PR links. Review
request fulfillment and merge eligibility retain their existing native owners.

## Package organization

Target owners:

- `src/app/`: bootstrapping, theme/session providers, application shell.
- `src/components/ui/`: generated and reviewed shadcn primitives.
- `src/components/discussion/`: shared composer, metadata, timeline, filters.
- `src/features/issues/` and `src/features/pull-requests/`: feature models,
  requests, screens and colocated tests; split list/detail/review/merge owners.
- `src/features/repository/`, `releases/`, `settings/`, `imports/`: existing
  product surfaces with their own local helpers and tests.
- `src/lib/`: shared transport, routes, and class merging utilities.
- `src/styles/`: the canonical theme and necessary product layout styles.

This is a target ownership map, not permission to add unused folders or APIs.
Move components as they acquire their final owner; do not build Primer-shaped
adapters over shadcn or keep two permanent styling systems.

The first implementation slice replaces the Primer React components and theme
imports throughout the application. Shadcn primitives live in `components/ui/`;
shared Markdown, editing, search, metadata, mutation and conflict behavior live
in `components/discussion/`. Issue routing, listing, creation, detail, comments
and models have separate modules in `features/issues/`. Pull requests separate
list, create, detail, conversation, commits, timeline, merge, and review owners.
Repository browsing, refs, tree, files, commits, and diffs have distinct modules;
preview engines and workers stay together under files. Settings separates
general, membership, archive, and protection policy. Releases, imports, labels,
and milestones own their local forms, selection controls, and helpers. Shared
transport and routes live in `lib/`; app bootstrap and error handling live in
`app/`. The former flat files and forwarding paths are removed.

`styles/globals.css` maps Geist's light/dark sRGB palette to semantic component
roles. Geist Sans and Mono are bundled locally. Tinted status labels use the
stronger text step of each palette for contrast. Theme changes suppress color
transitions so intermediate colors cannot make controls unreadable. Primer CSS
variable names and component imports are removed rather than aliased. Octicons
remain repository-domain icons; Lucide supplies the shared shadcn controls.

Author predicates now bind current public usernames to qualified accounts via
native Directory reads. Unknown handles produce no match; display names never
select an account. `@me` uses the session identity before profile enrollment.
Boolean/excluded filters, counts, ordering and both list kinds share the native
predicate. Cursors include resolved bindings so handle reuse invalidates an old
cursor without requiring a repository write. Completion searches indexed handles
and preserves UTF-16 offsets and query bounds. Created by me/Authored by me links
use real queries; selection comes from a native global-conjunction flag rather
than browser string matching, and survives state/sort/reload.

This requires explicit index-format-3 rebuilds for existing repositories, plus
fleet-wide native input/output and Directory operation updates. Account handles
and repository data remain separate snapshots, not a distributed transaction or
authority. Author dropdowns, integration accounts, assigned/mentioned/review
views and owner/team/project authority remain delivery work.

Repository roles now separate Triage from code write access. Settings → Members
can grant/change it through the canonical revisioned catalog. Issue detail has
an independent `can_close` capability; state actions can remain available when
description editing is forbidden. Triage also manages existing labels,
assignments, milestones and PR review requests. Publication and merge/readiness
permissions remain separate; archive/version guards are preserved in native
commands. Durable role reads prevent delayed routing snapshots from deciding
these new capabilities. Native edit inputs change, requiring peers to deploy
together, with no new persistent schema or index preparation. Complete role
parity, qualified membership, owner/organization/team authority and atomic
cross-authority fencing remain incomplete.

## Verification and completion

Each requirement needs source and executable proof, not a checked box alone.
Native tests cover transactions, replay/stale versions, authorization, archive,
query semantics/cursors, counts, concurrency and restart. HTTP tests prove real
model wiring and malformed input behavior. Browser tests cover filtering,
keyboard/focus, URL/reload, error/conflict states, bulk actions, details and merge
recovery. Compare rendered issue/PR list and detail states in light/dark themes
and narrow/wide viewports against the reference structure. Exercise real writes
and restored data through the native server and isolated RustFS.

Keep existing assertions and release/security gates. Build the UI before server
embedding checks. Full suites and cross-platform/storage qualification belong
in CI or the dedicated qualification environment. Missing functionality and
verification remain explicitly incomplete until implemented and proved.

The current list/search slice passed 18 native query/index tests, five issue
HTTP tests, five PR HTTP tests (including real Git review/merge behavior),
107 UI unit tests, and all 68 browser tests. UI/server builds, all-target server Clippy,
formatting and architecture gates passed. An isolated RustFS process run created
an issue through the browser, verified result-count and state-tab presentations
and their destination queries, restarted during
preparation, resumed through the mTLS CLI, and compared unchanged source records.
The existing local preview's two repositories were explicitly prepared and their
records compared exactly before/after. These prove this slice, not the remaining
parity checklist or production qualification.

The mixed-result slice passed 20 native search/index tests, 63 API tests (one
pre-existing ignored case), 107 UI unit tests and all 69 browser tests. Browser
coverage includes colliding issue/PR numbers, kind-specific links/icons, query
completion, reload, dark/mobile accessibility and PR detail navigation. The full
browser run passed at the configured origin; an alternate-port run exposed two
existing fixed-origin expectations, without a product failure or relaxed checks.
UI/server builds, all-target Clippy, formatting and architecture checks passed.
An isolated RustFS process run exercised mixed search through the browser,
retained a mixed-result cursor across restart, and preserved Git/review/merge,
replay, milestone and index-rebuild recovery. The persistent preview's two
repositories were compared exactly across its server-only restart; domain
records were unchanged. This slice changes query/result contracts, not stored
index/domain formats, so existing prepared indexes need no rebuild.

The profile slice passed the full isolated server suite (325 passed, five
existing ignored), 107 UI unit tests and all 72 browser tests. Native tests prove
qualified identity separation, concurrent handle claims, class/receipt boundaries,
version/replay behavior, restart and mTLS owner handoff. HTTP proof covers
session/CSRF, Git-token rejection and private-claim exclusion. Browser proof
covers enrollment/lost-reply retry, stable public URLs, edits, stale drafts,
handle collisions, missing preparation and dark/mobile accessibility. A real
RustFS browser run enrolled/renamed an actor and verified it after server restart
and repository-index rebuild. UI/server builds, all-target Clippy and
architecture checks passed. The local preview preserves both repositories and
leaves profile enrollment to its operator. The private native envelope changes
require a coordinated fleet restart; mixed-version operation is unqualified.

The milestone slice passed the full isolated server suite (329 passed, five
existing ignored), 107 UI unit tests and all 75 browser tests. The final mobile
collision-margin change also passed its three focused browser cases. Native and
HTTP checks cover mixed kinds, assignment/removal, title changes, counts,
exclusion/missing predicates, cursor invalidation, bounded catalog search,
query destinations and revoked durable membership. UI/server builds, all-target
Clippy, formatting and architecture checks passed. The embedded UI completed a
real RustFS browser workflow in light/dark and mobile layouts, then retained
milestone search/choices across process restart and interrupted index rebuild.
The preview's two actual format-1 indexes reported `outdated`, upgraded through
the CLI and preserved exact canonical issues, PRs, comments, activity, links,
reviews, labels, milestones and assignments. No profile was enrolled in the
persistent preview. Organization/owner/team authority and the remaining parity
checklist stay incomplete.

The label slice passed the full isolated server suite (333 passed, five
existing ignored), 107 UI unit tests and all 80 browser tests. Native and HTTP
proof cover bounded catalog search, colors/descriptions, include/exclude and
no-label transitions, comma-OR and unrelated-query preservation, mixed results,
query bounds and revoked durable membership. Browser proof covers multi-select,
keyboard exclusion, URL/reload, metadata/result failures, dark/mobile
accessibility and a held-response regression for clicks during refresh. Shared
list controls stay mounted; loading/count rows and the optional diagnostic
header retain their height so completed requests cannot move a pressed choice.
Before a measurement exists, `powerbar=1` shows an explicit unmeasured placeholder.
UI/server builds, all-target Clippy, formatting and architecture checks passed.
The embedded UI completed a real RustFS label-selection/exclusion/no-label
workflow and retained exact issue records, choices and counts across restart.
The persistent preview's two repositories compared unchanged across its
server-only update. Labels reuse the canonical catalog and index format 2;
no rebuild or domain migration is required. Recent label suggestions and the
remaining parity checklist are still incomplete.

The workspace/density slice passed all 84 browser tests, 107 UI unit tests,
TypeScript/Vite and embedded server builds, and formatting checks. New browser
proof covers mixed kinds, preserved query/cursor and metadata, no extra result
fetch, keyboard selection and selected styling, reload/navigation, visible
collapse actions with diagnostics, dark/mobile layouts and denied browser
storage. The first broad run exposed a clipped mobile PR collapse action; the
source now wraps navigation and both kinds have expanded/collapsed accessibility
coverage. The embedded UI also exercised real RustFS-backed issue/PR lists,
saved preferences and reload in desktop and dark mobile layouts. The live mobile
audit found zero violations, with contrast checks incomplete for existing
clipped header links/footer decoration. This is not full accessibility proof.
Both preview repositories retained their domain records across server restart
and browser actions. Reaction observation receipts advanced with the repository
revision; `api/reactions/mod.rs` supplies that ordering watermark rather than a
per-target record version. Counts, membership and every other record field
compared unchanged. No native schema, query, dependency or configuration
contract changes in this slice; personal views and full parity remain open.

The list-attribution slice passed 336 isolated server tests (five existing
ignored), 107 UI unit tests and all 86 browser tests. Final focused cases cover
the maximum-length handle and toolbar/author containment with diagnostics on
mobile. Native/HTTP proof covers qualified issuer separation, lookup bounds,
Unicode identities, read-only receipts, current profile names, unchanged source
records, absent preparation, scoped-token rejection and revoked durable grants.
The new lookup also succeeds after native mTLS owner handoff in object and fleet
durability modes. Both list kinds share the same HTTP owner and row renderer;
detail/comment/review contracts retain their existing behavior and remain a
follow-up, not a second list-author path. UI/server builds, all-target Clippy,
formatting and architecture checks passed. The embedded UI completed a real
RustFS mixed-list-to-public-profile workflow, including a 39-character username
in dark mobile layout with diagnostics enabled. Toolbar buttons and author
links stayed inside the panel without hidden horizontal scrolling. Attribution
survived process restart and index rebuild alongside the existing Git, review,
merge, label and milestone qualification. Both persistent preview repositories
compared unchanged after their server update; no profile was enrolled there.
These qualify the implemented slice, not the remaining parity checklist.

The author-filter slice passed 341 isolated server tests (five existing ignored),
107 UI unit tests and all 88 browser tests. Focused native peer tests cover
handle resolution/completion and authored results after mTLS owner handoff in
object and fleet durability modes. HTTP/native cases cover qualified issuer
separation, unknown/renamed/reused handles, cursor invalidation without a
repository mutation, unenrolled self queries, UTF-16 completion bounds and
revoked durable grants. Browser proof covers both personal views through state,
sort, reload, keyboard completion and dark mobile accessibility. The shared axe
helper now awaits the actual Radix exit lifecycle instead of a retired Primer
selector; assertions and contrast checks remain enabled.

UI/server builds, all-target Clippy, formatting, architecture gates and relative
documentation links passed. A real RustFS process workflow exercised embedded
personal views, Git publication/merge and author search/completion across
restart and explicit index rebuild. Both existing preview repositories reported
format 2 as outdated, rejected indexed reads until explicit preparation, and
upgraded to format 3. All 18 captured canonical collaboration resources compared
unchanged; the operator remained unenrolled and self filtering still worked.
This does not qualify full GitHub parity or remove the Cellar 0.1.0 release
limitations documented in `crates/crab-server/CELLAR.md`.

The Triage slice passed 344 isolated server tests (five existing ignored),
107 UI unit tests and all 89 browser tests. Native/HTTP proof covers role/scope
ordering, durable promotion before routing materialization, both discussion
kinds, rejected mixed title/state edits without a partial mutation, close/reopen,
labels, assignees, milestone assignment, review requests, denied definition/merge/
write-token actions and downgrade. Browser proof covers the role selector through
save/reload, state actions with description editing forbidden, downgrade, focus,
dark/mobile layouts and automated accessibility checks. Builds, all-target
Clippy, formatting, architecture gates and relative documentation links passed.

A real embedded browser workflow used an isolated RustFS bucket and a synthetic
loopback GitHub OAuth provider with PKCE verification. An administrator granted
Triage through Settings, a separate signed-in user reopened/closed another
author’s issue, and write-token issuance was denied. Catalog grants, sessions and
issue changes survived process restart; a subsequent Read downgrade removed the
state controls. The existing Git, review, merge, replay, search and rebuild
qualification also passed. Both persistent preview repositories retained all
18 captured collaboration resources, apart from the newly computed `can_close`
field; no profile was enrolled or schema/index rebuilt. Native authority fencing,
remaining role capabilities and the full parity checklist remain unfinished.

Current-grant admission replaces cached routing membership across browser APIs,
Git and LFS. Credentials alone no longer grant repository access; an admitted
principal binds the current role to the immutable repository UUID and configured
issuer. Global catalog reads and body-scoped token issuance resolve their targets
explicitly. Late publication/upload checkpoints refresh the durable grant.
Git-token format 2 also binds that UUID; older tokens require revocation/reissue
and replicas must deploy together. This remains catalog authorization, with
qualified persistent grants and atomic native fencing still outstanding.

The current-grant slice passed 354 isolated server tests (five existing ignored),
all-target Clippy with warnings denied, frontend/server builds, formatting,
architecture gates and changed relative documentation links. Regressions cover
promotion and downgrade before routing refresh, revoked reads, encoded paths,
failed authority reads, repository UUID reuse, issuer isolation, and rejected
retired token records with working revocation.

Real browser sessions against native Cellar and an isolated RustFS bucket passed
role edits and issue actions. A previously issued write token lost Git receive
and LFS upload permission after its owner was downgraded; read access and the
same permission decisions survived process restart. The existing Git publication,
review/merge, replay, search, milestone and rebuild workflow also passed, leaving
56 durable Cellar objects. Updating the persistent preview preserved all 18
captured collaboration resources across both repositories exactly, without a
schema/index rebuild or profile enrollment. This proves the implemented slice,
not full role parity or production qualification of the embedded SDK.

The Author-menu slice passed 355 isolated server tests (five existing ignored),
107 UI unit tests and all 92 browser tests. Focused coverage includes boolean
query destinations, exact truncation boundaries, qualified result counts,
rename/rebinding, revoked access, private-claim exclusion, keyboard/focus/reload,
errors/retry and narrow dark layouts. Native peer routing, strict all-target
Clippy, architecture gates, formatting, type checks and frontend/server builds
also passed. The live GitHub Author control was checked for its searchable
single-selection interaction; current contributor ordering remains a known gap.

An embedded-browser workflow used real sessions, native Cellar and isolated
RustFS storage. A second synthetic user registered a public profile through the
form, filtered both issue and PR lists by the existing author, selected Yourself
to reach a truthful empty result, cleared to restore rows, and reloaded the saved
query. Desktop/light and narrow/dark menu screenshots were inspected. Existing
Git, review/merge, replay, search, rebuild and authorization-restart qualification
also passed, leaving 62 durable Cellar objects. The persistent preview retained
all 18 captured collaboration records across both repositories, with no profile
enrollment or schema/index rebuild. Five reaction ordering watermarks advanced
from 1279 to 1281; `api/reactions/` intentionally reports the repository receipt
revision. All reaction groups, record versions and other captured fields were
unchanged. This does not finish the full parity goal.

The private-account/schema-2 slice passed 359 isolated server tests (five existing
ignored), strict all-target Clippy, all-target checking, builds and architecture
gates. Native fixtures use an exact copy of the committed format-1 input schema.
They cover issuer separation, registration idempotence without public enrollment,
100-row progress, mid-upgrade restart/replay, preserved profiles/submission retries,
whole-batch rollback including DDL, and maximum-width Unicode identities at the
SQL byte limit. No dependency or snapshot/inventory baseline was changed.

Real-process proof started the previous binary against RustFS, then stopped it
and started the new binary over its committed schema-1 data. Profile reads were
rejected until the private CLI migration completed. Old UUIDs/data survived;
private registration followed by UI enrollment kept the same ID, survived another
restart, and gave the new user no repository access. The existing Git, review,
merge, search and authorization qualification also passed, leaving 73 durable
Cellar objects. The persistent preview explicitly upgraded its empty Directory
and retained all 18 captured collaboration resources exactly across both
repositories, with no profile enrollment or repository/index rebuild.
The Assignee list menu remains delivery work; this is not complete GitHub parity
or SDK release approval.

Current assignment candidates now resolve from durable membership, including
additions, removals and renamed members before routing refresh. Issue and PR
writes validate eligibility and triage permission from the same late snapshot;
list/detail rendering uses the final membership read. The regression fails on
the previous cached implementation and passes with this change. All 76 affected
API tests pass (one existing ignored), along with strict all-target Clippy,
server build, formatting and architecture gates. Real RustFS browser actions
prove visible rejection of a stale open-picker selection without an edit,
assignment to a newly added member on both surfaces, and renamed assignments
after process restart. The preview update preserved all 18 captured resources
across two repositories. That checkpoint retained subject assignments; the actor
upgrade below replaces them. The Assignee filter and cross-authority fencing
remain required work.

Actor assignment format 2 replaces provider subjects with stable account UUIDs
and name snapshots. Public DTOs include optional current profiles; enrollment
keeps an already assigned UUID, and membership removal preserves attribution.
Native Directory reads bind account IDs and public profiles in bounded batches;
final membership rechecks remain the permission boundary. This does not move
membership authority out of the catalog.

Explicit private inventory/migration consumes qualified per-record mappings,
checks source versions, converts eight discussions per transaction, and retains
progress plus successful/conflicting decisions through restart. Renaming the
assignment column and updating values in place avoids unbounded column-drop
rewrites. The canonical binary empty-set encoding preserves the existing SQL
default. Format-4 search preparation renames its TEXT actor lookup and uses the
existing bounded clear/rebuild phases. Normal serving has no retired-subject
reader or old request-shape adapter.

The shared shadcn metadata picker now has lazy loading, name/handle search,
checkboxes, focus return, mobile containment and unavailable-person removal.
An edit captures its original record version; a conflict retains choices rather
than applying them to an unseen version. Closing content becomes inert while
its exit animation finishes. These changes do not implement the Assignee list
menu or finish the broader issue/PR/project/milestone parity inventory.

The actor-assignment slice passed 369 server tests (five existing ignored),
107 UI unit tests and all 95 browser tests, plus strict all-target Clippy,
formatting and architecture gates. Final focused picker checks also cover the
error-message inset. Native tests preserve canonical defaults, reject retired
bytes during serving, retain versions/submissions, roll back schema and earlier
row updates on failure, and replay successful/conflicting decisions after restart.

A real previous-binary/current-binary RustFS upgrade converted 19 nonempty
assignment records with explicit qualified mappings. It stopped after eight,
restarted the process, resumed through the CLI, rebuilt format 4 and verified
all other fields unchanged apart from monotonic reaction ordering watermarks;
121 durable Cellar objects remained. A separate current-binary browser run
proved stale-selection rejection, new-member assignment on issues and PRs,
profile enrollment keeping the assigned UUID, navigation to that public profile,
and attribution after revocation/restart, leaving 59 durable objects.

The persistent preview's two native source inventories contained no legacy
assignments. Explicit assignment/index preparation preserved all 18 captured
resources; five reaction ordering watermarks advanced from 1291 to 1292, while
all other fields and the unenrolled operator profile remained unchanged. These
checks qualify the implemented assignment slice, not complete GitHub parity,
native grant fencing, production scale or the pending SDK release prerequisites.

## Primary references

- [GitHub issue/PR filtering](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/filtering-and-searching-issues-and-pull-requests)
- [GitHub organization issue types](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/managing-issue-types-in-an-organization)
- [GitHub search qualifiers](https://docs.github.com/en/search-github/searching-on-github/searching-issues-and-pull-requests)
- [shadcn/ui](https://ui.shadcn.com/docs)
- [Vercel Geist colors](https://vercel.com/geist/colors)
- [Vercel Geist typography](https://vercel.com/geist/typography)
- [GitHub refreshed PR list: collapsible navigation and compact mode](https://github.blog/changelog/2026-09-10-refreshed-repository-pull-requests-page-in-public-preview/)

## Assignee list filtering

Native assignee predicates use stable account IDs. Public handles resolve through
Directory; `@me` resolves the authenticated qualified account without creating a
profile or account. An unknown/unregistered account matches no assignments.
`assignee:*` and `no:assignee` select assigned and unassigned work. Explicit
`assignee:actor/UUID` supports historical or unenrolled people without exposing
provider claims. Boolean groups/exclusions, full counts, state destinations and
cursor identity use the same predicate and resolved IDs on both list surfaces.
Handle renames/rebinding invalidate old cursors rather than silently continuing
under another account.

The shared shadcn filter menu composes native historical name snapshots, current
repository members, and bounded public handle completion. It supports name/handle
search, yourself/nobody/anybody, clear, keyboard navigation, focus return, loading,
errors/retry, bounded/truncated results, and mobile/dark layout. Query navigation
preserves sort/direction and diagnostics while resetting pagination. Both
workspaces expose a native `assigned_to_me` selection for sidebar navigation;
negated or conditional self predicates do not manufacture personal selection.

Derived index format 5 adds searchable name snapshots to assignment rows. Existing
cells require explicit `cells rebuild-search`; constant-default column additions
leave old rows in place until bounded clear/reprojection completes. Canonical
assignment format 2, record versions, identities and public submissions do not
change. Update native peers together for the new list bindings/output and choice
read. This slice does not implement reviewers/involves/mentions, saved views,
native grant authority, or complete GitHub parity.


Verification: 372 server tests passed (five existing ignored), followed by four
focused assignee checks covering the additional historical-choice catalog test.
Strict all-target Clippy and architecture gates passed. All 98 browser scenarios
passed across the full run and two port-corrected checks; the full run used 5179
while two existing assertions require 5175. No assertions were weakened.
All 107 UI unit tests, type/build and formatting checks passed.

Real old-format-4/current-format-5 process checks verified a visible 503 before
preparation, interrupted rebuild/resume, unchanged canonical issue/PR records
and actor/username predicates. Its browser stage stopped at an asynchronous
option lookup. A separate completed current-binary RustFS browser run waited for
loaded choices and proved issue/PR filtering after revocation, URL sort/diagnostic
preservation, username completion and process restart; 61 durable objects remained.
Mobile dark screenshots were inspected. Owned fixtures and browser sessions were
closed. The persistent preview rebuilt both indexes to format 5 and retained all
18 captured resources exactly, including reaction watermarks and the unenrolled
operator profile. This qualifies the implemented slice, not full GitHub parity
or the outstanding native authority/SDK release prerequisites.


## Reviewer relationships and navigation

Native format-6 projections support `review-requested`, `reviewed-by`,
`review-involves` and direct `user-review-requested` with public handles or `@me`.
Requests and submitted reviews update relationship rows transactionally; comment
reviews participate too. Canonical request/review records and submission replay
remain their existing owners. An old review retry cannot fulfill a later request.
Reviewer rows survive ordinary discussion text/label refreshes. Issuer-qualified
identity digests keep otherwise equal subjects distinct.

The shared shadcn Reviews filter offers requested/reviewed/involved modes,
bounded searchable public choices and Yourself/Clear actions. It preserves query
boolean groups and URL sort/direction/diagnostics, resets cursors, and supports
keyboard/focus, loading/errors/retry and narrow dark layouts. The PR sidebar adds
Review requests based on native positive global self selection. Mixed issue search
accepts the same predicates; its toolbar retains the issue-specific layout.
Author and reviewer resolution/menu behavior share explicit `people/` owners,
without keeping renamed adapters or another Directory-resolution path.

Existing indexes require explicit preparation. Rebuild clears reviewer rows,
pages canonical SQL reviews, then pages canonical KV requests with persisted
source cursors. Each transaction handles at most eight source records, including
unrelated documents in sparse request ranges. Serving remains gated until all
sources complete. Canonical versions, histories, private Directory state, grants
and public submissions need no conversion for this derived-index step.

Remaining work includes review-status predicates, current-head/check rollups,
team requests, general participants/mentions/involves, saved views and the broader
issue/PR/project/milestone inventory. Merge-policy status is not approximated by
review-request presence. Full GitHub and production parity remain unproven.


Verification: 377 server tests passed (five existing ignored), followed by four
focused checks on the final composite-key review pager. Strict all-target Clippy,
architecture gates, frontend build/type checks, 107 UI unit tests and all 100
browser scenarios passed. Native tests cover qualified identities, cancellation,
comment-review fulfillment, permanent submission retry after a newer request,
ordinary document refresh and complete multi-page reconstruction.

A real format-5/format-6 process upgrade reconstructed 17 existing reviews and a
pending request, restarting in both the SQL-review and sparse-KV-request phases.
The three captured detail/history/request resources matched except for 18
reaction ordering watermarks advancing 59→63. `api/reactions/` deliberately
reports the repository receipt revision; browse projection writes can advance it.
All record versions, contents, timestamps, request state and reaction groups were
unchanged. Read-only comparison of canonical distinct reviewers and projected
reviewers matched, with the pending request retained. The initial strict-equality
harness stopped at those metadata revisions; browser verification was completed
separately against the current binary.

The completed current-binary RustFS run also restarted within both rebuild sources,
then used actual browser actions to request a review, choose sidebar/username
filters, submit a comment review and view reviewed/involved results. Completion
and relationship state survived another restart; 114 durable Cellar objects
remained. Canonical/projected distinct-reviewer counts matched again after review,
with no pending requests. Desktop and narrow dark screenshots were inspected.
Both persistent preview repositories explicitly rebuilt to format 6 and retained
all 18 captured resources exactly, with no profile enrollment. Owned qualification
browsers, identity provider, server and RustFS container were closed.

## Unified conversation

Issue and PR detail now share `components/discussion/conversation/`: typed cards,
one chronological reader, editors, composer, paging and permalink focus. Native
`collaboration/conversation/` references canonical comments/reviews and compact
activity, without duplicating opening or contribution snapshots. SQL reference
seeks use creation time/source/number, including historical clock rollback;
opaque before/after/focus windows have count and byte bounds. Derived index
format 10 requires explicit bounded `cells rebuild-search` preparation.

Hidden panels pause timeline reads and close their menus/portals. Cached editors
retain drafts and captured versions through windows, tabs and root refreshes;
pending writes remain observable. Posting into an older focused window offers an
explicit link without changing navigation. Review freshness follows the settled
source head; compact event actors resolve the same public profiles as cards.
Independent comment/review REST collections remain canonical CRUD APIs; the UI
has one history reader and one diagnostic widget behind `powerbar=1`.

This slice passed 84 HTTP tests (one existing ignored), 118 native Repository
regression tests, 107 UI unit tests and all 113 browser scenarios. Strict Clippy,
architecture gates, Rustfmt and frontend/server builds passed. Native tests also
cover serialized byte limits, indexed query plans, historical rows, cross-subject
cursors, rebuild invalidation, relationships and inline thread events. Browser
checks cover failed ranges, back/reload/direct links, old-window posts, lost reply
identity, stale edits, pending completion across tabs, hidden portals, accessibility,
themes and mobile containment.

Real embedded-UI RustFS qualification reconstructed 38 comments and 56 reviews
alongside compact events, posted from a retained draft, changed PR state, followed
an old sidebar decision and comment, and recovered them after restart. It also
interrupted the native `conversation` preparation phase and compared preserved
records after resumption. Both preview repositories were explicitly upgraded;
18 existing resource payloads compare unchanged after accounting for the public
activity actor DTO. No profile was enrolled, no Fern code copied, and Cloudflare
compute remains off. Complete parity, Projects/organization authorities, advanced
review/merge workflows, moderation, notifications and bulk/saved views remain open.


## Organization authority foundation

`cells/organization/` owns metadata, current member/owner grants, membership
versions/tombstones, versioned seven-day invitations and transactional SQL/KV
audit. Explicit private bootstrap registers the first owner's qualified account
without profile enrollment. Native checks precede reads, writes and receipt
replay. Invitations require the exact recipient's consent; direct member edits
cannot create grants, removal cannot be undone by old offers, and the final owner
cannot be removed/demoted. Closed offers retain recipient history without
returning current organization metadata to a former member. Collection/revision/
organization-bound cursors keep member and invitation pages distinct and bounded.

The shadcn/Geist People, Invitations, acceptance and settings views use these
APIs. Drafts survive version conflicts until the user reviews saved metadata;
links currently provide invitation delivery. The embedded server now recognizes
organization and public-profile deep links in its reserved account namespace.
Repository grants remain exclusively catalog-owned; organization ownership
creates no repository permissions. No owner is inferred from a catalog name.

Verification includes 146 native Cell regression checks, 86 HTTP checks, three
asset checks, strict all-target Clippy, architecture gates, Rustfmt, frontend/
server builds and 107 UI unit checks. All 116 existing browser scenarios passed;
seven final focused checks cover the nullable history response and related
profile flows. Thirteen final native/HTTP organization checks cover current
permissions, qualified identities, consent, stale versions, cancellation races,
last-owner protection, cursors, audit and restart. Object/fleet peer qualification
also exercised Organization alongside Repository and Directory through mTLS and
owner handoff.

The explicitly run two-account browser/OIDC qualification exercises the actual
embedded UI and APIs: public-profile enrollment, invitation, recipient consent,
visible member roster, removal and access-scoped historical display. Driver
readiness waits cover asynchronous username/offer reads; no request mocking or
permission bypass is used. A separate final RustFS process run saved organization
metadata through the browser, displayed the last-owner error, restarted and
re-read the saved profile in the browser. It also retained issue/PR/Git/search/
rebuild behavior across restart, with 79 durable native objects. Desktop and
390px screenshots were inspected; its owned services and browsers were closed.

This is phase-1 groundwork, not complete GitHub parity. Owner discovery/registry,
personal namespaces, teams, full organization lifecycle/email delivery, native
repository grant cutover and cross-Cell fencing remain unfinished. Projects,
organization issue types, advanced review/merge flows and the remaining Issues/
Milestones inventory still follow the collaboration plan. Cellar's documented
SQLite WAL-reset release blocker remains unchanged.

## Issue closure lifecycle

Issues now carry canonical nullable `state_reason` and `closed_at` metadata.
Completed/not-planned closure, reason changes and reopen operations share the
Repository transaction with the record version, milestone counts, activity,
search revision and durable command receipt. Reopening clears the close time;
changing an already-closed reason preserves it. Authors and current triage
members may change state/reason; stale, archived and unauthorized writes cannot
change the metadata. Invalid reason/state combinations have a typed rejection.

The shadcn split close action lives beside the comment button. Choosing an open
issue's reason does not write until Close is pressed. Closed issues support
reason changes and reopening. Header/list presentation distinguishes not-planned
closures, and typed timeline events explain the change. Reason-only changes
retain comment drafts; history reads do not disable independent state actions.
Delayed mutation replies cannot replace a newer issue refresh. Close/Reopen with
a comment is delivered by the subsequent atomic submission slice below.

`reason:completed`, `reason:"not planned"`, `reason:reopened` and `no:reason`
participate in the shared Boolean query, counts and cursor contract. Positive
reason predicates match issues only. The normalized canonical relation supplies
metadata without another derived index format or rebuild. Cursor fingerprints
include its format so old list cursors cannot silently change meaning.

Existing repositories require explicit `cells prepare-issue-state --owner OWNER
--name REPO` before issue detail, mixed search and issue writes. Serving never
creates missing lifecycle rows. Bounded frames retain both an issue cursor and
an activity cursor; preparation resumes across restart. Immutable historical
close/reopen events can prove dates or reopened state, while absent reasons stay
unknown. An edit timestamp is never substituted for a close date. Original
mutation outcome bytes and absent-field request digests remain unchanged.

This advances the issue foundation. Duplicate closure and typed duplicate links,
sub-issues/dependencies, date filtering,
organization types, saved/bulk workflows and the wider PR/Projects/Milestones
inventory remain delivery work. The Cellar dependency/release limitation is
unchanged.
Validation: the isolated server library suite passed 418 tests (six existing
opt-in tests ignored), the CLI passed 13, and final focused lifecycle tests passed
six. Strict all-target Clippy, architecture gates, formatting, UI typecheck/build,
107 UI unit tests and a clean 119-scenario browser run passed. The real embedded-
UI/RustFS run exercised reason selection/changes, reopen/reclose, retained drafts,
metadata/search restart, and the existing Git/PR/merge/milestone/index-rebuild
workflow, leaving 87 durable native objects. Desktop/mobile captures were
inspected; owned test browsers/services were closed. The persistent preview's two
repositories were explicitly prepared without an index rebuild; 18 canonical
resource payloads were preserved, apart from proven nullable lifecycle fields
and advancing reaction receipt watermarks. No profile or organization was created.

The change adds 816 net production lines for canonical lifecycle storage,
transactional behavior, bounded preparation, management/CLI and state controls.
Shared HTTP rendering removes duplicate enrichment; no dependency, compatibility
reader, second index format or speculative date-query surface was added.

## Atomic comment and state actions

The issue and PR composer now submits Close/Reopen with comment as one native
Repository command. An empty draft changes only state; changing an already-closed
issue's reason leaves the draft unsubmitted. The canonical issue/PR comment writers
are shared by plain comments and state actions, and existing state owners retain
permission, archive, version, merge-pending and merged-state policy. A rejected
state operation rolls back the comment, child counter, search contribution,
activity and milestone progress. Successful contributions precede their state
event in the chronological conversation.

Each action has a permanent, subject-scoped submission identity. Its digest binds
qualified actor, expected version, desired state/reason and original comment.
Display-name/current-role snapshots do not change that identity. Recovery precedes
later archive/version/merge checks and reads the existing canonical comment; it
cannot execute the state operation again. No new SQL format, preparation, second
comment writer or replacement for original command outcomes is needed.

The shared composer controller preserves the original action through failures,
metadata refresh and PR tabs. A confirmed rejection permits revision after reload;
an uncertain response offers Retry original action. Only the submitted draft is
cleared; newer text remains. Hidden PR responses cannot move the visible history.
Archived repositories retain drafts and permit recovery of an existing action.
HTTP state responses mark immutable-payload validation and authoritative native
domain rejections with `X-Crab-Write-Outcome: rejected`. Access, routing, decoding,
transport and submission-identity errors carry no such marker: an earlier attempt
may have committed before the failing retry. The composer retains that original
intent. Local UTF-8 body and encoded-envelope checks prevent an oversized new
comment from entering an unrecoverable submission loop.

Root dropdown menus dismiss immediately so an exiting menu item cannot steal
editor focus through Radix’s pointer-leave callback. The deferred unmount focus
return also preserves a newer deliberate focus outside the menu. Submenus already
suppress their own unmount return; other overlay types do not participate in this
menu-item lifecycle.

The new operation stays within current repository authority. Native repository
role cutover/cross-Cell fencing, full branch/reopen parity, pending reviews,
duplicate/dependency relationships, saved/bulk views and the wider PR/Projects/
Milestones inventory remain delivery work. The published Cellar release blocker
and dependency versions are unchanged.


Validation: the final isolated server library suite passed 425 tests, with six
existing opt-in qualifications ignored. CLI tests passed 13, UI unit tests 107,
and the final clean browser suite 126. Strict all-target Clippy, architecture
gates, Rustfmt, frontend formatting/typecheck and frontend/server builds passed.
The focus regression failed before the fix; the final delayed-menu and reason-edit
scenarios passed 30 repeated runs. Both issue and PR browser flows include lost
responses, original-intent recovery, retained newer drafts and confirmed rejection.

Real embedded-UI/RustFS qualification closed and reopened both kinds with comments,
verified the visible conversation and canonical records, restarted the process,
and recovered the exact native submissions without duplication. Search, milestones,
Git push/merge, explicit preparation/rebuild and organization safeguards remained
working across restart, with 101 durable objects in the final run. Desktop/mobile
captures were inspected; owned qualification services and browsers were closed.
The persistent preview was restarted with the rebuilt assets: 18 captured resource
payloads compare unchanged, without migrations/rebuilds or profile/organization
creation. Read-only browser checks passed on both embedded and Vite preview ports.

The slice adds 617 net production lines for one new native transaction boundary,
its permanent submission contract, commit-aware HTTP handling and a shared composer
controller. Plain and combined actions reuse canonical contribution/state writers;
separate UI state mutations were removed. No dependency, lockfile, SQL preparation,
compatibility path or second search index was introduced.


## Bounded bulk list triage

The Issues and PR workspaces now share row, Shift-range and select-page checkboxes.
Their selected-count header offers state, label, assignee and milestone actions
using shadcn/Geist controls. Mixed label/assignee membership is represented by an
indeterminate checkbox. Deltas preserve other active values, historical assignees
remain removable, and per-item limits are checked. Milestone choices search the
complete native catalog with bounded results. Diagnostics wrap the complete bulk
widget behind `powerbar=1`.

The layout follows GitHub's [multiple-item assignment](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/assigning-issues-and-pull-requests-to-other-github-users)
and [milestone selection](https://docs.github.com/en/issues/using-labels-and-milestones-to-track-work/associating-milestones-with-issues-and-pull-requests)
references inspected on 2026-10-22. Fern's pinned bulk validation/rollback/replay
cases informed acceptance; no Fern source or assets were copied.

The list, selection read and edit carry the stable repository UUID, preventing
reused owner/name routes from retargeting captured items. A native selection read
captures expected record and independent milestone assignment versions. A bounded Repository command composes canonical writers and
commits every selected change, activity, milestone count, projection and receipt
together. Stale/denied/invalid operations roll back earlier items. The permanent
submission ledger recovers the original action after restart or a lost response;
public replies contain the processed count, not copied canonical records.
Current Triage capability governs fresh edits. The browser holds the original
repository UUID/selection/action through uncertain replies, including later access
or routing failures, and
requires explicit reload after a confirmed conflict. Fresh filters/pages clear
idle selection; pending recovery retains its original subjects. A completed action
refreshes the current list query rather than restoring an old cursor. No UI implies
selection of results on other pages.

Deleted labels remain as retired IDs in source records. Bulk deltas merge the
active catalog, as the individual picker already does, so invisible tombstones
cannot block the next edit. The active catalog is read once per bulk transaction.
No new SQL schema, migration, discussion-index format, dependency or compatibility
path was added. Existing per-item operations retain their canonical ownership.

This advances the Issues/PR list foundation. Native repository grant cutover and
cross-Cell fencing, Projects/type bulk operations, the remaining keyboard/list
interactions, saved views, advanced relationships/reviews and complete Projects/
Milestones parity remain open. The subsequent shared-identity slice below extends stable repository UUID
binding to individual browser mutation workflows. The Cellar release blocker is unchanged.

Validation: the isolated server library suite passed 431 tests (six existing
opt-in qualifications ignored), CLI tests 13 and UI unit tests 107. All 139
browser scenarios passed, including oversized UTF-8/JSON envelopes, retained
intent after later access failure, repository UUID reuse and a pending reply after
a query change. The mobile header alignment regression passed. Twelve final
discussion HTTP checks included an already-signed-in reader upgraded to Triage,
current capability/admission without a new session, UUID mismatch and unmarked
submission-identity conflicts. Strict all-target Clippy, architecture gates,
Rustfmt, frontend formatting/typecheck and frontend/server builds passed.

The native maximum-bound case selected and edited 100 issues with 64 KB bodies;
its 101st item was rejected. Real embedded-UI/RustFS qualification performed all
four bulk actions on mixed Issues/PRs, verified canonical/visible changes, restarted
and recovered permanent native submissions. The final run retained 105 durable
objects and preserved the existing Git/merge/search/rebuild/organization workflow.
The real run caught an empty-query caller error hidden by the initial fixture;
bulk catalog search now omits that parameter, and its regression checks the native
contract. Driver waits bind to the loaded assignee checkbox rather than unrelated
row attribution. Owned qualification browsers/services were closed.

Final desktop/mobile, light/dark selected and idle captures were inspected on the
persistent preview. Read-only browser checks measured the count/checkbox alignment,
verified the whole `powerbar=1` bulk widget, and exercised embedded/Vite ports. The
restart preserved 18 captured repository resources exactly; no canonical data,
profiles, organizations, schema or index formats were changed by preview checks.

The slice adds 1,471 net production lines for one bounded native selection/write
boundary, its durable submission contract, commit-aware API composition and shared
page-selection/recovery/picker controls. Canonical state and metadata writers are
reused; no duplicated mutation policy, second database or speculative adapter was
introduced. The new rejection marker owner serves both state and bulk handlers.


## Repository identity for browser mutations

The shared HTTP boundary now requires a captured repository UUID on browser and
local-trust mutations, after current authentication, CSRF and durable grants and
before domain effects. Catalogs expose the UUID. Individual issue/PR comments,
state, metadata, review/merge, labels/milestones, settings, files, uploads, branches
and releases all supply it. Git/status/check token scopes already bind repository
UUIDs and keep their transport contract. No schema, preparation or dependency
change is required; no name-only mutation path remains for browser repository APIs.

The repository subtree is keyed by UUID. Replacing an owner/name route cannot
reuse another repository's draft/controller or retarget a queued action. Missing,
malformed, duplicate or mismatched identity returns unmarked `repository_changed`;
an earlier request may already have committed. State and plain-comment controllers
preserve the original action through that conflict. Current read access remains
required to recover it. This is an unreleased API precondition, not a parallel
membership authority or native cross-Cell authorization fence.


A paused request-body regression initially created an issue in a replacement
repository after the middleware check. The admitted principal now carries a
monotonic target constraint, preserved through grant refreshes and honored by local
trust too. Replacing routing while the body waits cannot write into the new Cell.
The case-insensitive membership route resolves the catalog independently, so
missing/invalid identity denies its repository capability rather than creating an
unconstrained local or browser principal. Native role fencing remains separate work.


Validation: 435 isolated server tests passed (six existing opt-in qualifications
ignored), along with 141 browser scenarios and 107 UI unit tests. Strict all-target
Clippy, architecture gates, frontend build/typecheck/formatting and Rustfmt passed.
The streaming-body race failed before the request constraint and passed afterward;
case-insensitive membership, duplicate headers and lost-reply recovery are covered.

The final native binary passed real RustFS wrong-UUID rejection with no issue
created, embedded UI bulk mutations, process restart and durable submission recovery.
The run retained 107 durable objects; owned qualification services/browsers closed.
The persistent preview preserved 18 resource payloads exactly, after accounting for
the catalog's new UUID field from its already-published list identity. Index format
10, profile enrollment and canonical records stayed unchanged. Read-only browser
checks exercised both embedded/Vite ports, mobile alignment and whole-widget timing.

This adds 109 net production lines for one shared request constraint, canonical
catalog identity, the error contract and all repository mutation callers. Credentials,
Cell data formats and dependencies stay unchanged. Native grant fencing, the wider
Issues/PR/Projects/Milestones inventory and the pinned Cellar release blocker remain
open; this slice does not establish complete parity or production release readiness.


## Repository issue pins

GitHub's [pinning reference](https://docs.github.com/en/issues/tracking-your-work-with-issues/administering-issues/pinning-an-issue-to-your-repository)
was checked on 2026-10-22: three repository pins, Write permission, cards above
search and sidebar pin/unpin. The public GitHub issue list was inspected to
confirm that closed cards remain visible while the list filters open work.
Fern's `apps/web/test/issues-authority.test.ts` at the assessment commit supplies
permission, maximum, complete-order concurrency, rollback and retry acceptance
references. No Fern code or assets were copied.

Native Repository `issues/pins/` owns one ordered collection with a separate
version and permanent qualified-actor submissions. Pinning leaves issue edit
versions untouched. Reads use at most three current issue/index point lookups;
comment counts use the canonical transactional discussion projection. Missing
initial KV means an empty new collection, with no SQL/index format change,
compatibility reader or automatic preparation. Pin/unpin events share the native
activity/conversation transaction; pure reorder has no issue timeline event.

`api/issues/pins/` composes current membership, public author enrichment and
Write capability. Fresh changes reject archive, stale collection versions,
nonexistent/duplicate targets and a fourth pin. Replaying an exact submission
requires current Read access and recovers its original receipt after archive or
role change without restoring an old order. The shared archive presentation
retains existing proven-rejection markers while leaving uncertain failures
unmarked. The regression covers pins and the sibling issue-state path.

`packages/ui/src/features/issues/pins/` owns shadcn cards above search, sidebar
pin/unpin and keyboard/drag ordering. The three-card layout uses Geist tokens,
full linked titles, current state/author/date and comment counts. Pending actions
keep the last confirmed layout with controls disabled; read/access errors withdraw
it. Lost replies preserve the original UUID, expected version and order across
later context errors. Success/recovery reads the current collection; confirmed
conflicts need explicit reload. Only the conversation refreshes after pinning,
so unrelated close/reopen controls cannot steal focus. The whole pin widget is
diagnostic only with `powerbar=1`.

Native grants/cross-Cell fencing, saved views, advanced issue relationships,
remaining review/merge features, Projects and full Milestones parity remain open.
The pinned Cellar release blocker is unchanged; this slice is not complete parity.


Validation: 440 server library tests passed (six existing opt-in qualifications
ignored), alongside 13 CLI tests and 107 UI unit tests. All five new pin browser scenarios passed,
including lost reply plus a later unmarked context failure, explicit conflict
reload, keyboard/drag ordering, focus, read-only controls and mobile light/dark
accessibility. The broad run passed 145 of 146 browser scenarios; an unrelated
organization invite contrast audit sampled a transitioning button. All four
organization scenarios passed on the isolated rerun without source/test changes.
Strict all-target Clippy, architecture gates, formatting and frontend/server
builds passed.

Real RustFS plus the current embedded UI exercised sidebar pin/unpin, card
ordering, current titles/comment counts, the three-card limit, closed cards,
conversation events, full process restart and permanent receipt recovery. It
preserved the wider Git/merge/search/bulk/organization workflow and retained 119
durable objects. Desktop/mobile and dark captures were inspected. The owned
qualification browser and services closed.

The persistent preview restart preserved 18 canonical resource payloads and
index format 10. Empty pin reads were verified on both repositories, including
the archived repository's disabled management capability. Read-only browser
checks exercised embedded/Vite ports, the sidebar pin control, whole-widget
timing and existing bulk selection/mobile alignment. No preview pin was changed,
profile enrolled, organization created, index rebuilt or data migrated.


The slice adds 964 net production lines for the native collection/receipt owner,
its bounded public HTTP read/write contract, generated shadcn Card primitive and
feature-local controls/recovery. Existing issue, search, activity, conversation,
identity and permission owners are reused; no second issue store, dependency,
lockfile, schema or compatibility path was added.


## PR list check and review summaries

Reference image 9 places a compact check outcome and passed/total count alongside
row metadata. GitHub's [commit rollup contract](https://docs.github.com/en/graphql/reference/commits#statuscheckrollup)
and [PR head contract](https://docs.github.com/en/graphql/reference/pulls#pullrequest)
were checked on 2026-10-22: rollups include both check runs and status contexts and
belong to the inspected source commit. The live public list failed to load during
this pass, so its failed screen was not treated as visual parity evidence.

Both PR lists and mixed issue search now include native `rollup` data. HTTP
captures selected head refs once through the verified targeted-ref data plane;
one native read checks all selected PR versions before returning current-head
review decisions, requested counts and check totals. Repeated heads reuse check
counts. Merged PRs use their recorded source head after branch deletion. Missing
refs are explicitly unavailable; read errors withdraw the list. No stale opening
head or failed-read fallback can paint green. This read path does not determine
merge eligibility or change the canonical merge policy.

Checks reuse canonical status/run readers, with latest per-name run versions.
The status owner selects latest context numbers through its covering index before
loading presentation fields, instead of decoding up to 1,000 historical status
records for every row. Source bounds remain 128 contexts/100 current runs per
commit and 96 review decisions per PR; the list is at most 100 selections. No
check log/output bodies are loaded. Request totals are indexed point reads.

Discussion index format 11 maintains outstanding requests transactionally with
reviewer insert/update/delete. Existing Cells need explicit bounded search-index
preparation. Clearing removes each source pull's aggregate before its participant
rows; review/request replay reconstructs it without duplicate counts. Native
historical-format-10 preparation and repeated rebuild checks preserve PR records.
No compatibility reader or automatic schema upgrade was added.

Feature-local shadcn tooltips show full outcome/request/decision counts and source
identity. Check links open the PR Checks tab; review links open its conversation.
Counts distinguish success, neutral and skipped results; their fraction counts
completed outcomes without failure. Row summaries work with density, selection,
keyboard focus, mobile themes and the whole list `powerbar=1` widget. They make
no additional browser requests. Review-state search, role associations, recent
pushes, saved views, Projects and the wider parity inventory remain open.


Validation: 444 server library tests passed (six existing opt-in qualifications
ignored), alongside 13 CLI tests, 107 UI unit tests and all 150 browser scenarios.
The four new browser scenarios cover both list kinds, exact check/review counts,
keyboard tooltips/Checks navigation, advanced/deleted heads, failed-read withdrawal,
density, mobile themes and whole-widget diagnostics. Native/HTTP cases exercise
latest named reruns, all check conclusions, case-insensitive status contexts,
request fulfillment/cancellation, stale source versions, qualified-identity privacy
and repeatable explicit preparation from format 10. Strict all-target Clippy,
architecture gates, formatting and frontend/server builds passed. The existing
full-row HTTP assertion was extended to assert the new captured-head summary.

The current binary passed real embedded-browser/RustFS qualification: external
status/check writes became visible in PR rows, pushing a new source head cleared
old check counts, deletion showed an unavailable source, and merged PR rows kept
the recorded source checks after source deletion and restart. Existing Git,
merge, bulk, pin, search and organization workflows also passed. The run retained
121 durable objects; owned qualification services and browsers closed. Desktop
and mobile captures were inspected.

The preview had stopped before its update with Cellar's node-lease watchdog fence.
Its stopped cache was preserved, and the updated server was restored once. All
18 captured canonical routes were read live before explicit preparation; their
payloads and 37 canonical SQL source tables matched afterward. Both repositories
were prepared to format 11, including the archived repository. Read-only browser
checks covered embedded/Vite ports, PR and mixed lists, mobile alignment and timing.
No profile/organization was created and no preview issue/PR record was edited.
The pinned Cellar release blocker and native authority fencing remain open.


The slice adds 427 net production lines for one bounded native summary read,
transactional request aggregate, shared source-ref composition and feature-local
shadcn tooltip/link presentation. Existing Git, status, run and review writers
remain canonical. Latest-status selection removes historical decoding from the
shared reader. No dependency, lockfile, environment option or compatibility path
was added.


## PR review dismissal

Submitted reviews now have a native dismissal lifecycle. Write users can dismiss
an approval or change request with a required reason; protected branches require
Admin permission. Native commands fence review versions and pending publication,
remove only that exact active decision, advance review/PR versions, and record
one event and permanent recovery intent in the same transaction. Older review
dismissals cannot erase replacements or revive previous approvals. Original
submission receipt shapes remain unchanged.

Review cards retain original feedback, show the dismissal actor/reason/time, and
use an effective dismissed state. The later event links to the original review.
Sidebar, list rollups and merge requirements use the canonical active decisions.
Shadcn action menus/dialogs support cancellation, inspection of changed content,
exact uncertain retry, responsive themes and focus return. Current Read access
permits recovery after Write revocation or archive. The public endpoint requires
the captured repository UUID; both review and dismissal actors use qualified
Directory attribution followed by a membership check.

`cells prepare-review-state` explicitly prepares existing repositories. It creates
an empty canonical SQL relation; serving does not create tables or read a retired
shape. No discussion-index format change is needed. Native review detail/list
reads and conversation entries return review and dismissal in one snapshot;
complete reasons count toward bounded output pages. This contributes to the
review-workflow checklist; review filters, custom/team dismissal restrictions,
pending reviews and the wider parity inventory remain open.

Validation: 454 server library tests passed (six existing opt-in qualifications
ignored), alongside 13 CLI tests, 107 UI unit tests and all 157 browser scenarios.
The new native cases cover exact active-decision removal, replacement reviews,
protected-branch permissions, stale versions, pending publication, transactional
rollback, schema preparation, immutable receipt replay and full-size reasons in
bounded pages. HTTP coverage checks both qualified actors, membership changes,
archive recovery, proof headers and all public review projections. Browser cases
cover required reasons, focus return, cancelled drafts, lost replies, explicit
conflict inspection, narrow dark screens and accessibility. Strict Clippy,
architecture gates, formatting and frontend/server builds passed.

Two real OIDC browser sessions qualified the embedded UI against RustFS/native
Cellar. Dismissal removed a required approval; a simulated lost reply after the
native commit recovered after Write revocation, archive and a server restart.
The reason persisted and exactly one dismissal event remained. Desktop/mobile
captures were inspected. The local preview explicitly prepared both repositories,
including the archived repository; all 18 captured canonical resources were
preserved, with only additive review fields and monotonic reaction revisions.
Embedded and Vite read-only browser checks passed. No preview profile,
organization, issue or PR was created or edited. The pinned Cellar release blocker
and native authority fencing remain open.

This slice adds 853 net production lines for the native dismissal relation and
transaction, bounded snapshot reads, explicit preparation, HTTP policy and the
recoverable shadcn interaction. Submitted review commands remain under one owner;
the duplicate HTTP review record/converter was deleted. Original receipt formats
remain canonical. No dependency, lockfile, environment option or compatibility
reader was added.

### Configured protection precedence

Tracing review-status filters exposed a dismissal permission mismatch: merge and
Git publication honored configured initial branch protections, while dismissal
treated an absent saved policy as unprotected. The prior binary reproduced a
Write user's visible dismissal action and successful mutation for a configured
protected branch. Native snapshot details now distinguish absent rules from an
explicitly empty saved set and carry the exact base ref. HTTP supplies the initial
branch classification; the command applies current stored rules ahead of that
classification in its transaction. Review controls share this precedence across
create/edit responses, details, lists and conversation entries. The existing
version-zero configuration contract remains; no new fallback or policy format
was added.

Native and two-user HTTP regression cases prove configured denial, explicit
clearing, saved protection overriding an unprotected seed, and recovery after
seed/role changes. The full library suite passed 456 tests with six existing
opt-in qualifications ignored; the seven focused dismissal cases and strict
all-target Clippy also passed. This correction adds 27 production lines for
policy provenance and the shared capability composition, without changing public
review fields, SQL storage, dependencies or receipt formats.

The current binary passed the real two-account RustFS/browser scenario: configured
protection hid the writer's action and returned a proven 403 rejection; saving an
empty rule set exposed the action and allowed dismissal. Its lost reply recovered
after Write revocation, archive and process restart, retaining the reason and one
event. The final binary runs in the local preview, where all 18 captured canonical
resources remained identical through restart and no profile was enrolled.
