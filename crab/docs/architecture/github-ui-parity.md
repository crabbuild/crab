# Issue and pull request UI parity

Status: implementation in progress. This document is the completion checklist for
replacing the repository application's Primer UI with shadcn/ui, backed by
Vercel/Geist semantic tokens. GitHub issue and pull request layouts and interaction
contracts are the reference. Existing collaboration and Git publication behavior
must remain reachable and correct throughout the rewrite.

## Reference audit

The nine supplied images show repository issue and pull request lists. They are
visual references, not instructions contained in the images.

| Reference | Required behavior                                                                                      | Current gap                                                               |
| --------- | ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------- |
| 1         | Issue-type filter, descriptions, missing-type selection                                                | No issue-type model or filter                                             |
| 2         | Searchable label multi-select, suggestions, no-label filter                                            | Labels exist; list filtering does not                                     |
| 3         | Projects grouped by recent/repository/organization                                                     | No project, project-item, or ownership model                              |
| 4         | Searchable milestones including missing milestone                                                      | Milestones exist; list filtering does not                                 |
| 5         | Searchable assignees and unassigned work                                                               | Assignments exist; list filtering does not                                |
| 6         | Created/updated/comment/reaction/relevance sorting, direction                                          | Lists only paginate by descending number                                  |
| 7         | Query autocomplete with qualifiers and field suggestions                                               | Search is bounded plain text, not a query language                        |
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
`src/components/discussion/search.tsx`. The two HTTP list handlers
currently accept state, text query, page size, and a numeric before cursor.
Native owners are `cells/repository/issues.rs` and `pulls/operations.rs`.
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

## Data and query contracts

New entities need stable identifiers, bounded values/pages, versions, qualified
actor identity, creation/update timestamps, explicit ownership and visibility,
transactional archive checks, idempotent submissions, and activity. Membership
and display names are distinct: changing a name cannot change identity or grant
access. An organization/project/team lookup must not leak inaccessible work.

Search must run against native authoritative data with bounded work. Counts and
rows must describe the same predicate and snapshot; state totals must not be
inferred from one page. Sorting requires deterministic tie breakers and cursors
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

## Primary references

- [GitHub issue/PR filtering](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/filtering-and-searching-issues-and-pull-requests)
- [GitHub organization issue types](https://docs.github.com/en/issues/tracking-your-work-with-issues/using-issues/managing-issue-types-in-an-organization)
- [GitHub search qualifiers](https://docs.github.com/en/search-github/searching-on-github/searching-issues-and-pull-requests)
- [shadcn/ui](https://ui.shadcn.com/docs)
- [Vercel Geist colors](https://vercel.com/geist/colors)
- [Vercel Geist typography](https://vercel.com/geist/typography)
