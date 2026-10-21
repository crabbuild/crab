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
| 5         | Searchable assignees and unassigned work                                                               | Assignments exist; list filtering does not                                |
| 6         | Created/updated/comment/reaction/relevance sorting, direction                                          | Native index and controls implemented; rollups still needed                                  |
| 7         | Query autocomplete with qualifiers and field suggestions                                               | Boolean grammar and native suggestions implemented; more qualifiers remain                        |
| 8         | PR workspace navigation, recent-push comparison, review filters                                        | PR reviews/requests exist; list projections and navigation are incomplete |
| 9         | Accurate state counts, merged/closed/draft icons, author roles, check rollups, bulk selection, density | List responses omit much of this information                              |

The issue sidebar also requires assigned/created/mentioned/recent views, saved
views, projects, milestones, labels, and a collapse control. The PR sidebar
requires authored/assigned/involves/review-requested views. A control is not
complete until its query or mutation has a real effect and survives reload;
placeholder menus and filtering only the currently loaded page do not qualify.

Current entry points are `packages/ui/src/features/issues/index.tsx`,
`packages/ui/src/features/pull-requests/index.tsx`,
`packages/ui/src/features/issues/workspace.tsx`, and
`packages/ui/src/components/discussion/search.tsx`. The two HTTP list handlers
accept typed query, sort/direction, page size, and an opaque before cursor.
Native owners are `cells/repository/search/results/`, `issues.rs` and
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
`no:assignee`. Unknown qualifiers and
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
claiming an unsaved result. Existing authors/assignees are not yet linked to
these profiles. Owners, organization/team/project grants and their explicit
catalog authority cutover remain required; the combined model checklist above
stays incomplete.

Actor/project/type predicates and complete GitHub workspace controls
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

Index format 2 requires explicit preparation for existing format-1 indexes,
reported as `outdated`. The management command rebuilds titles from canonical
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

## Primary references

- [GitHub issue/PR filtering](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/filtering-and-searching-issues-and-pull-requests)
- [GitHub organization issue types](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/managing-issue-types-in-an-organization)
- [GitHub search qualifiers](https://docs.github.com/en/search-github/searching-on-github/searching-issues-and-pull-requests)
- [shadcn/ui](https://ui.shadcn.com/docs)
- [Vercel Geist colors](https://vercel.com/geist/colors)
- [Vercel Geist typography](https://vercel.com/geist/typography)
