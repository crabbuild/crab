# Crab repository UI

React application embedded by `crab-server`. GitHub repository layouts and
interactions are the reference; Geist colors and typography define the theme.

## Design system

`src/styles/globals.css` owns the theme for both product CSS and Tailwind/shadcn
components. Use semantic roles rather than palette colors in feature code.

- Spacing: a shared `--spacing` unit of `0.25rem`. Product CSS uses Tailwind's
  `--spacing(n)` function; components use the matching `gap-*`, `p-*`, and `m-*`
  utilities. Half steps serve compact controls. Keep optical offsets and external
  renderer measurements explicit rather than treating them as page spacing.
- Type: Geist Sans for the interface and Geist Mono for source and identifiers.
  The scale is 12px caption (`text-xs`), 13px secondary label (`--text-caption`),
  14px body (`text-sm`), 16px label (`text-base`), 18px subheading (`text-lg`),
  20px section (`text-xl`), 24px page (`text-2xl`), and 32px large heading
  (`text-3xl`) at the browser's default root size. Tokens use rems so user font
  preferences scale both CSS and utility-based views.
  Custom size utilities use an explicit length hint, such as
  `text-[length:var(--text-caption)]/5`, so `cn` preserves foreground colors.
- Layout: `--page-gutter` and `--page-block` align repository, discussion, account,
  and Projects content. They become more compact below 900px. Full-width source,
  table, and roadmap workspaces retain their own scrolling boundaries.
- Surfaces: `background` for content, `muted` for subtle separation, `secondary`
  for selection, and `popover` for overlays. Status colors retain their meaning
  in both themes. `border` separates surfaces; `input` defines visible control
  boundaries with at least 3:1 contrast; `input-hover` strengthens hover edges.
  `ring` supplies neutral keyboard focus with a 2px ring at 25% opacity and a
  solid control edge. Primitive controls own their focus treatment; native form
  controls share the same tokens, while plain links use a visible outline.
  Keep focus and selection neutral; blue remains the information/link color.
  `destructive` and `destructive-foreground` pair for solid action buttons.
  `destructive-muted`, `destructive-muted-foreground`, and `destructive-border`
  pair for warnings, errors, and badges. Use these surface roles for callouts
  instead of drawing saturated action-colored borders.
- Corners: 4px small, 6px medium, 8px default, and 12px raised surfaces. Use radius
  tokens and `rounded-*` utilities; controls use the medium radius, panels use
  the default, and pills use `radius-full`.

The theme follows [Geist's semantic color roles](https://vercel.com/geist/colors)
through [shadcn CSS variables](https://ui.shadcn.com/docs/theming). Light surfaces
stay white with subtle neutral separation; dark cards and popovers are raised
above the canvas. `color-scheme` keeps native controls and scrollbars in theme.

Review changes in both themes, at desktop and mobile widths, with keyboard focus,
long content, empty/loading/error states, and native write recovery intact.

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

Vite proxies `/api` and `/auth` to the local server on port 8788, preserving
the browser's Host and Origin. For authenticated development, configure Crab's
`auth.public_url` as `http://127.0.0.1:5175/` and start Vite with
`npm run dev -- --port 5175 --strictPort`. The exact IdP callback is
`http://127.0.0.1:5175/auth/callback`. See the
[local SSO fixture](../../crates/crab-server/deploy/oidc/README.md) for sample users
and signed group claims. Browser tests use port
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
  `pins/` owns cards above the search list, sidebar pin/unpin, keyboard/drag ordering
  and recovery. Only current Write capability enables changes; confirmed failures
  require reload and uncertain responses retain their original submission.
  `src/features/pull-requests/` separates lists, creation, detail,
  conversation, commits, reviews, checks, and merge/recovery. `pull-requests/rollups/`
  owns list check/review summaries in both workspaces; it uses native counts and
  captured source commit identity, with no per-row fetches or merge eligibility claims. PR detail retains
  draft/write state across tab switches and refreshes. Hidden panels pause
  conversation reads and close portaled menus; cached editors and pending writes
  stay mounted until their discussion unmounts.
- `src/features/repository/`: browsing, refs, tree, file rendering, commit details,
  and diffs. Preview engines, workers, and their tests stay under `files/`.
  `files/symbols/` owns parsing and the Symbols workspace. Its shadcn Resizable
  divider supports pointer and keyboard resizing; narrow layouts stack the pane
  below source. Tablet layouts retain separate tree, divider, and source columns;
  mobile layouts stack the tree above the file. Symbol and line-hash navigation
  scroll the document vertically without changing horizontal source scrolling.
  Breadcrumbs scroll horizontally while Copy path stays visible.
  File creation shares the repository tree workspace, with an inline filename
  and branch header. `files/code-editor.tsx` owns line numbers, undo history,
  indentation and wrapping controls, and draft preview for both Create and Edit.
  Markdown previews resolve relative links against the draft's directory.
  `files/commit.tsx` shares commit fields across the creation review dialog and
  existing edit/upload/delete panels; cancelling review preserves the draft.
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
Both lists group reaction sorting under **Sort → Reactions**. Arrow Right/Left
enters/leaves the submenu; Escape dismisses the menu. Sorting retains the query
and direction and discards the previous page cursor.

Use the primitive APIs directly: buttons contain icons with `data-icon`, inputs
with addons use `InputGroup`, menus compose their groups/items, and forms use
`Field`/`FieldGroup`. Use `Select` for styled option menus rather than browser
dropdowns. Workbench controls use these shared components, including search,
chart axes, schema actions, presets, and collapsible run history. Its CodeMirror
SQL editor is a custom `input-group-control` inside `InputGroup`, which owns the
single border and focus ring while CodeMirror retains completion and shortcuts.
The shared Button defaults to `type="button"`; mutations
must opt into `type="submit"`. Keep native permission, version and replay checks
in the server even when the UI disables an action.

The header's rightmost avatar opens profile settings and sign-out for SSO
sessions. Local sessions expose profile settings only; Git access remains a
separate header control.

Organization foundation pages live in `src/features/organizations/` at
`/-/organizations/{uuid}` and `/-/organizations/{uuid}/invitation`. The first owner
is explicitly bootstrapped through the server's private management CLI; opening a
page never creates authority. People, pending invitations, recipient decisions
and profile settings use native organization APIs with version checks. Public
username search selects stable accounts; invitations currently use a manually
shared link, with no email delivery. Organization discovery and owner-name routing
are still separate work. Standalone teams use native organization authority.

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

### Repository projects

The repository **Projects** tab provides template-based creation, saved table,
board and dated roadmap views, title/status/state filters, typed custom fields,
project settings/README, archiving and reusable repository templates. Board cards
can move by dragging or through their accessible item editor. Every displayed
page has exact native counts and bounded pagination. Planning status is separate
from issue close state and PR merging.

`src/features/projects/` owns the workspace, templates, fields/views editors,
discussion membership controls, and sub-issue controls. Its command provider lives
above route loading: lost responses retain the original account/repository/UUID
and payload until native retry/cancellation resolves them. Forms retain their
original draft version. `components/ui/table.tsx` is the owned shadcn table.

Issue and PR metadata link directly to project memberships. Issue conversations
support creating/linking/reparenting sub-issues and show current completion
progress. Projects show current issue/PR titles/states, parent/progress, and linked
work. Draft conversion creates an issue and replaces its project draft in one
native transaction. Read users cannot manage projects; existing Write grants and
repository archival apply everywhere. Prepare existing Cells using the private
`cells prepare-projects` command documented in `crates/crab-server/CELLAR.md`.

Browser coverage is `tests/browser/projects.e2e.ts`; native coverage lives beside
`cells/repository/projects/` and `api/projects/`.

### Shared organization projects

The organization **Projects** workspace at `/-/organizations/{uuid}/projects`
coordinates work across repositories. Projects have a brief, delivery status,
health, lead, start/target dates, linked repositories, drafts, typed planning
fields, and saved table/board/roadmap views. Repository and planning-status
filters query the native project collection; rows include current source titles,
states, assignees, parents, progress and linked work. Planning status remains
separate from closing an issue or merging a PR.

`src/features/projects/workspace/` owns the organization library, workspace and
source-aware item/repository editors. Shared selectors, views, settings and the
account-bound submission controller stay in `src/features/projects/`. Metadata
refresh keeps open editors mounted and waits for the confirmed project version
before enabling another write. A stale pagination cursor offers a first-page
reset. Project and organization planning dropdowns use the owned shadcn Select.
Project, custom-field and milestone dates use the shared DatePicker, composed
from the owned Calendar, Popover and Select. Dates remain nullable `YYYY-MM-DD`
civil dates; selecting or clearing a date never converts it through UTC. Start
and target controls constrain each other, and the native API validates ranges.

Current organization members can read and create projects; the creator in their
current membership incarnation and organization owners manage them. Project
access grants no source repository access. Inaccessible work is redacted, and
repository UUIDs prevent name reuse from redirecting project references. Source
permissions are refreshed after enrichment; native organization authority is
checked again before returning a composed response.

Shared projects link existing work and support draft notes. Source creation,
draft conversion, title/state saved filters and custom project templates remain
repository-scoped. Linking does not migrate repository projects or grant team
access. Workflow automation and cross-organization/personal project discovery are
separate features. Preparation and API contracts are in
`crates/crab-server/CELLAR.md`; browser proof is
`tests/browser/project-workspaces.e2e.ts`.
