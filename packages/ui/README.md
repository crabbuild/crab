# Crab repository UI

React application embedded by `crab-server`. GitHub repository layouts and
interactions are the reference; Geist colors and typography define the theme.

## Development

Use Node.js 22.12 or newer. From this directory:

```sh
npm ci
npm run dev
npm run typecheck
npm run test
npm run test:browser
npm run build
```

Vite proxies `/api` to the local server on port 8788. Browser tests use port
5175, mock transport per scenario, and check both interactions and accessibility.
Build the UI before building `crab-server`; its build script embeds `dist/`.

## Ownership

- `src/components/ui/`: owned shadcn/Radix primitives, explicit variants and
  accessible control behavior. Generate additions with the shadcn CLI using
  `components.json`, then review the generated source.
- `src/components/discussion/`: shared Markdown rendering, composer, search,
  metadata, mutations, conflict review, and permalink focus. Its `conversation/`
  owns one chronological reader, typed comment/review/activity cards, bounded
  windows, and the shared composer. `bulk/` owns page/range selection, versioned
  bulk state/metadata actions and uncertain-intent recovery for both list kinds.
  Lists, preparation and writes bind the stable repository UUID; queued selections
  cannot follow a reused owner/name route.
  Metadata deltas retain unrelated values; milestone search covers the native
  catalog. Bulk controls require current Triage capability. Markdown tests stay
  beside the renderer.
- `src/features/issues/`: issue models, routing, list, creation, detail and
  comments. `state.tsx` owns close-reason selection, reason changes and reopen
  actions beside the comment button. Failures preserve drafts and reject
  stale writes. Close/reopen can submit the current draft in the same native
  transaction; close-reason edits leave it unsubmitted. Not-planned closures
  have distinct header/list presentation and typed conversation events.
  `src/features/pull-requests/` separates lists, creation, detail,
  conversation, commits, reviews, checks, and merge/recovery. PR detail retains
  draft/write state across tab switches and refreshes. Hidden panels pause
  conversation reads and close portaled menus; cached editors and pending writes
  stay mounted until their discussion unmounts.
- `src/features/repository/`: browsing, refs, tree, file rendering, commit details,
  and diffs. Preview engines, workers, and their tests stay under `files/`.
- `src/features/settings/`, `releases/`, and `imports/`: feature screens with
  local forms and helpers. Labels and milestones own their selection controls.
- `src/app/`: bootstrap, session, error boundary, and appearance controls. The
  application applies the persisted light/dark/system choice before paint.
- `src/lib/`: shared HTTP transport, wire records, repository routes, formatting,
  code themes, and class merging. Feature consumers use the native HTTP APIs.
- `src/components/navigation/`, `request/`, and `layout/`: links, request/timing
  presentation, and pane resizing shared across features.
- `src/styles/globals.css`: canonical Geist semantic tokens, bundled fonts,
  Tailwind utilities and product layouts. Change semantic roles rather than
  introducing another theme or compatibility token names.
- `tests/browser/`: integration scenarios with shared fixtures under an explicit
  test owner. Assertions cover real rendered controls, keyboard continuation,
  stale responses, conflicts, permissions, timing widgets and merge recovery.

The remaining ownership and native model/query work is tracked in
`crab/docs/architecture/github-ui-parity.md` at the repository root. Advanced
filters need authoritative server predicates and counts; filtering one loaded
page cannot implement repository search.

Issue/PR search shares the native query grammar. Its combobox fetches authorized
repository suggestions and validation; it does not duplicate parsing in the
browser. The native response supplies UTF-16 replacement offsets. Keep input
focus while selecting suggestions and retain the settled request when focus
moves, so timing details cannot shift a clicked control.

Use the primitive APIs directly: buttons contain icons with `data-icon`, inputs
with addons use `InputGroup`, menus compose their groups/items, and forms use
`Field`/`FieldGroup`. The shared Button defaults to `type="button"`; mutations
must opt into `type="submit"`. Keep native permission, version and replay checks
in the server even when the UI disables an action.


Organization foundation pages live in `src/features/organizations/` at
`/-/organizations/{uuid}` and `/-/organizations/{uuid}/invitation`. The first owner
is explicitly bootstrapped through the server's private management CLI; opening a
page never creates authority. People, pending invitations, recipient decisions
and profile settings use native organization APIs with version checks. Public
username search selects stable accounts; invitations currently use a manually
shared link, with no email delivery. Organization discovery, owner-name routing,
teams and cross-repository Projects are still separate work.

The shared composer retains one original comment/state intent through uncertain
responses, metadata refresh and PR tabs. Retry original action recovers that
submission; newer text remains unsent. State replies need an explicit confirmed-
rejection marker before the controller releases an intent. A bare late permission
or routing error on a retry can follow a successful earlier commit. New comments
are checked against UTF-8 body and encoded request limits before submission.
Drafts remain available after archive, and
hidden responses cannot move the visible conversation.

Repository catalog entries carry their stable `id`. Repository mutations pass the
repository to `useMutation`; account/organization mutations pass `null`. Raw file,
branch, upload, settings and release writers send `X-Crab-Repository-Id` themselves.
The repository subtree is keyed by UUID so a reused owner/name cannot inherit an
old draft/controller. Preserve a queued action's UUID rather than replacing it
when the server reports `repository_changed`; an earlier attempt may have committed.
