# Plan 018: Add durable inline pull-request review threads

> **Executor instructions**: Follow this plan in order. Run each verification
> gate before continuing. Stop on any condition in **STOP conditions**; do not
> invent a dependency API, migration shortcut, compatibility path, or second
> Git-write implementation. When complete, update plan 018's row in
> `advisor-plans/README.md` unless a reviewer says they own the index.
>
> **Drift check (run first)**:
>
> ```sh
> git diff --stat 892720ce6a6..HEAD -- \
>   crates/crab-http-server/src/cells.rs \
>   crates/crab-http-server/src/cells/repository.rs \
>   crates/crab-http-server/src/cells/repository \
>   crates/crab-http-server/src/cells/migrations \
>   crates/crab-http-server/src/api.rs \
>   crates/crab-http-server/src/pulls.rs \
>   crates/crab-http-server/src/pulls \
>   crates/crab-http-server/src/auth_tests/pulls.rs \
>   crates/crab-http-server/src/server_peer_e2e_tests.rs \
>   crates/crab-http-server/README.md \
>   crates/crab-http-server/REFERENCE.md \
>   packages/repository/package.json \
>   packages/repository/package-lock.json \
>   packages/repository/src \
>   packages/repository/tests/browser/pulls.e2e.ts
> ```
>
> If an in-scope surface changed, reconcile every excerpt, operation ID, schema
> version, route, and dependency assumption below before editing. A conflicting
> Cell migration or occupied operation ID is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: XL
- **Risk**: HIGH
- **Depends on**: none
- **Category**: direction, migration
- **Planned at**: commit `892720ce6a6`, 2026-09-19

## Why this matters

Crab currently supports pull-request summaries, general comments, review
decisions, and commit-scoped review summaries, but reviewers cannot discuss a
specific changed line. That makes non-trivial reviews ambiguous and forces
authors to translate prose back to code manually. This plan adds durable,
paginated line/range conversations, replies, resolution/reopen, explicit
outdated handling, and one-click suggested changes while preserving Crab's
Cell durability, raw Git path support, exact-head conflict checks, and single
canonical content-publication path.

This is one product feature but four coupled contracts: persisted collaboration
state, exact diff anchoring, authorization, and an accessible diff UI. Do not
ship a UI-only or in-memory subset.

## Product contract

Implement this exact first release:

1. A member can select one line or an inclusive range of at most 200 lines on
   one side of a textual changed file and publish an inline thread immediately.
   Lines are one-based. `side` is `old` or `new`; a range never crosses sides.
2. Thread creation is allowed only on an open pull whose live `base_oid` and
   `head_oid` exactly match the submitted comparison. The server derives and
   persists the old/new blob OIDs; it does not trust blob IDs from the browser.
3. The root message is part of the thread. Members may reply to current,
   outdated, resolved, closed-pull, and merged-pull threads. Root authors and
   reply authors may edit their own text with version compare-and-swap.
4. The pull author or a repository writer may resolve and reopen a thread.
   Resolution records actor and time. Resolution does not delete discussion.
5. A thread is `outdated` when either its anchored base OID or head OID differs
   from the pull's currently displayed comparison. Do not fuzzy-remap anchors.
   Current threads render at their selected lines; outdated threads remain in
   a paginated “Outdated conversations” section even when the path disappeared.
6. An optional suggestion is a structured replacement for the complete
   selected `new`-side lines. It may be empty (delete the lines), must be at
   most 64 KiB without NUL, and is stored with the root message. Suggestions
   are invalid on the old side.
7. Applying one suggestion uses the existing `PATCH /contents` contract with
   the pull head branch, exact current head, exact anchored new blob, raw
   `path_hex`, complete replacement file content, and a bounded commit message.
   The content endpoint remains authoritative for write access, protected
   branches, stale heads/blobs, unsupported content, and publication. After a
   successful commit the thread naturally becomes outdated. Do not auto-resolve
   it and do not add a second server-side commit builder.
8. The existing general review form and approval/request-changes behavior stay
   intact. Inline threads are not pending-review drafts in this release.

## Current state

### Persisted model

`crates/crab-http-server/src/pulls/storage.rs:89-111` has only general comment
and review records:

```rust
pub(super) struct PullComment {
    pub number: u64,
    pub author: Identity,
    pub body: String,
    pub version: u64,
    // timestamps
}

pub(super) struct PullReview {
    pub number: u64,
    pub author: Identity,
    pub body: String,
    pub state: ReviewState,
    pub commit_oid: String,
    pub version: u64,
    // timestamps
}
```

`crates/crab-http-server/src/cells/migrations/0001_repository_identity.sql:211-284`
contains sequences, replay submissions, records, and foreign keys for pull
comments/reviews. `crates/crab-http-server/src/cells/repository/pulls.rs` owns
their commands and queries. Creation uses a 16-byte submission ID plus a
payload digest for exact replay; edits use a version compare-and-swap. Preserve
both patterns.

The repository module is schema 1 only at
`crates/crab-http-server/src/cells.rs:1903-1961`. It registers commands 1-31
and queries 1-28. `operation()` currently marks every operation schema 1..=1.
The new tables require schema 2 and new operations must not run against schema
1.

### HTTP model

`crates/crab-http-server/src/pulls.rs:39-56` exposes general pull and comment
routes. `crates/crab-http-server/src/pulls/reviews.rs:28-52` exposes only
review list/create/detail/edit and derives `current` from the current head OID.
The pull router applies an 80 KiB body limit.

`crates/crab-http-server/src/api.rs:682-714` is the canonical diff read path:
it resolves exact base/head snapshots, reads optional blobs, and compares the
snapshots. Thread creation must reuse typed code extracted from this path; do
not make an internal HTTP request or implement another Git reader.

`crates/crab-http-server/src/contents.rs:34-79,500-670` is the canonical file
mutation path. Its update request already requires `branch`, `expected_head`,
`expected_blob`, `path_hex`, `content`, and `message`, then builds and publishes
one commit. Suggested-change application must call this HTTP route from the UI.

### UI model

`packages/repository/src/pulls.tsx:663-680` renders the shared comparison and
then one general review form:

```tsx
<ComparisonView
  repo={repo}
  base={data.base_oid}
  head={data.head_oid}
  theme={theme}
  codeThemes={codeThemes}
/>
{data.state === "open" && !repo.archived && (
  <ReviewForm repo={repo} pull={data} csrf={csrf} refresh={pull.retry} />
)}
```

`packages/repository/src/content.tsx:577-604,766-884` uses the same
`ComparisonView` for pull creation, commit comparison, and pull files. It lazily
loads each diff and renders `@pierre/diffs` `MultiFileDiff`; review behavior
must therefore be optional and enabled only by pull detail. The pinned package
is `@pierre/diffs` 1.4.0 in `packages/repository/package.json`.

`packages/repository/src/content-editor.tsx:114-137,548-560` shows the existing
CSRF-aware `/contents` client contract. Extract a shared small mutation client
if needed; do not copy its fetch/error behavior into a third implementation.

### Applicable conventions

- Raw repository paths travel as lowercase hex and are stored as bytes. Do not
  require UTF-8 paths. Match `contents::validate_path` and `api::encode_hex`.
- Discussion bodies are at most 64 KiB, contain no NUL, and are required where
  stated. Match `app::body` and `repository::validate_body`.
- Cell lists use descending numeric cursors, maximum 50 returned items, maximum
  200 examined records, and 1 MiB encoded output. Match pull child pagination.
- Cell commands own validation and replay safety even when the HTTP handler
  validates first. Handler validation is not a trusted storage boundary.
- Rust uses typed `Result`/`thiserror`, no production panic/unwrap/expect.
- React must preserve keyboard operation, visible focus, screen-reader labels,
  dark/light themes, 360 px layout, and existing Primer/design tokens.
- `packages/repository/dist` is generated and must not be edited manually.

## Commands you will need

Use a checkout-specific external Cargo target on every compiling Cargo command.
Stop if `$HOME/Workspace` is not mounted and writable.

| Purpose | Command | Expected success |
| --- | --- | --- |
| Install UI deps | `npm ci --prefix packages/repository` | exit 0; lockfile unchanged |
| Inspect diff types | `rg -n "lineAnnotations|selectedLines|on[A-Za-z]*Line|Annotation" packages/repository/node_modules/@pierre/diffs -g '*.d.ts' -g '*.ts'` | declarations prove public line selection and annotation rendering |
| UI typecheck | `npm run typecheck --prefix packages/repository` | exit 0 |
| UI unit tests | `npm run test --prefix packages/repository` | all pass |
| UI browser tests | `npm run test:browser --prefix packages/repository -- pulls.e2e.ts` | all pull tests pass |
| UI format | `npm run format:check --prefix packages/repository` | exit 0 |
| Build embedded UI | `npm run build --prefix packages/repository` | exit 0; `dist/index.html` exists |
| Focused server tests | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-018-review-threads" cargo test -p crab-http-server --locked --lib review_thread` | all matching tests pass |
| Pull auth tests | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-018-review-threads" cargo test -p crab-http-server --locked --lib server::auth_tests::pulls` | all pass |
| Remote-owner test | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-018-review-threads" cargo test -p crab-http-server --locked --lib server::peer_e2e_tests::public_collaboration_requests_reach_remote_owner_over_mtls_and_publish_ltx` | pass |
| Full server library | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-018-review-threads" cargo test -p crab-http-server --locked --lib` | all non-ignored tests pass |
| Strict lint | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-018-review-threads" cargo clippy -p crab-http-server --all-targets --locked -- -D warnings` | exit 0 |
| Rust format | `cargo fmt --all -- --check` | exit 0 |
| Diff hygiene | `git diff --check` | exit 0 |

Run the UI build before any server compile because
`crates/crab-http-server/build.rs` embeds the generated repository assets.

## Dependency contract gate

Before writing UI code, install the pinned lockfile and read the actual 1.4.0
declarations under `packages/repository/node_modules/@pierre/diffs`. Record in
the implementation PR which public props/types provide:

- a click or selection callback with side and one-based line information;
- inclusive range selection or enough public selection state to implement it;
- line annotations with custom React content in split and unified views.

The upstream documentation is `https://diffs.com/docs`; declarations from the
pinned package are the final contract. Do not query shadow DOM, depend on
private element structure, monkey-patch rendered rows, or upgrade the package.
If 1.4.0 cannot support all three needs through public APIs, STOP and report the
missing contract. A dependency upgrade requires a separately approved lockfile
change.

## Scope

### In scope

- `crates/crab-http-server/src/cells/migrations/0002_pull_review_threads.sql` (new)
- `crates/crab-http-server/src/cells.rs`
- `crates/crab-http-server/src/cells/initializer.rs`
- `crates/crab-http-server/src/cells/repository.rs`
- `crates/crab-http-server/src/cells/router.rs`
- `crates/crab-http-server/src/cells/scheduler/tests.rs`
- `crates/crab-http-server/src/cells/repository/pull_review_threads.rs` (new)
- `crates/crab-http-server/src/cells/repository/pull_review_threads_codec.rs` (new)
- `crates/crab-http-server/src/api.rs`
- `crates/crab-http-server/src/peer.rs`
- `crates/crab-http-server/src/pulls.rs`
- `crates/crab-http-server/src/pulls/storage.rs`
- `crates/crab-http-server/src/pulls/review_threads.rs` (new)
- `crates/crab-http-server/src/pulls/review_thread_storage.rs` (new)
- `crates/crab-http-server/src/auth_tests/pulls.rs`
- `crates/crab-http-server/src/server_peer_e2e_tests.rs`
- `crates/crab-http-server/README.md`
- `crates/crab-http-server/REFERENCE.md`
- `packages/repository/src/content-editor.tsx`
- `packages/repository/src/content.tsx`
- `packages/repository/src/pulls.tsx`
- `packages/repository/src/pull-review-model.ts` (new)
- `packages/repository/src/pull-review-model.test.ts` (new)
- `packages/repository/src/style.css`
- `packages/repository/tests/browser/pulls.e2e.ts`
- `advisor-plans/README.md` status only

If existing test module wiring requires a one-line declaration in another
test owner, add that exact declaration and document it in the final scope.

### Out of scope

- Pending/draft review batches, “start a review,” and submit-all-comments.
- Reviewer requests, CODEOWNERS, notifications, reactions, moderation,
  deletion, or comment edit history.
- Fuzzy line remapping after a push. Outdated means exact comparison mismatch.
- Suggestions spanning files/sides, batch suggestion application, suggestion
  commits created inside the thread endpoint, or auto-resolution after apply.
- Cross-repository/fork pull requests; current pulls use branches in one repo.
- Changes to approval counting, merge requirements, or protected-branch policy.
- `@pierre/diffs` version changes, `package.json`, or package-lock changes.
- Editing migration `0001_repository_identity.sql` or generated `dist` files.
- New config/env flags, fallback readers, legacy aliases, or dual storage paths.

## Git workflow

- Branch: `advisor/018-inline-pull-review-threads`.
- Rebase on current `origin/main` before implementation and rerun the drift
  check. Do not implement concurrently with another repository schema/operation
  ID change in the same worktree.
- Use concise conventional commits. Recommended boundaries:
  `feat(http-server): persist pull review threads`,
  `feat(repository): render inline review threads`,
  `test(http-server): qualify pull review conversations`.
- Do not push or open a PR unless instructed.

## Data and API design

### Schema 2

Add, without modifying migration 1:

- `repository_pull_review_thread_sequences(pull_number, last)`.
- `repository_pull_review_thread_submissions(pull_number, request_id,
  payload_digest, thread_number)` for exact creation replay.
- `repository_pull_review_threads` with pull/thread number; creator identity;
  root `body`; optional `suggested_text`; exact `base_oid`, `head_oid`; raw
  `path` BLOB (1..1024 bytes); nullable old/new blob OIDs; side code; inclusive
  `start_line`/`end_line`; resolution flag, optional resolver identity/time;
  version and timestamps.
- `repository_pull_review_reply_sequences(pull_number, thread_number, last)`.
- `repository_pull_review_reply_submissions(pull_number, thread_number,
  request_id, payload_digest, reply_number)`.
- `repository_pull_review_replies` with pull/thread/reply number, author, body,
  version, and timestamps.

Use STRICT tables, composite primary/foreign keys, cascading deletion from the
pull, the existing JavaScript-safe integer ceiling, 40-character OID checks,
and an all-null-or-all-present constraint for resolver identity/time. Require
`start_line >= 1`, `end_line >= start_line`, and
`end_line - start_line < 200`. Require the side's blob OID to be present;
require `suggested_text IS NULL` for old-side anchors. Empty suggested text is
valid and distinct from NULL.

### Cell operations

If the drift check confirms they remain free, allocate:

| ID | Operation | Schema | Bound |
| --- | --- | --- | --- |
| command 32 | `CreatePullReviewThread` | 2..=2 | 256 KiB in, 256 KiB out |
| command 33 | `UpdatePullReviewThread` | 2..=2 | 256 KiB in, 256 KiB out |
| command 34 | `CreatePullReviewReply` | 2..=2 | 128 KiB in, 128 KiB out |
| command 35 | `UpdatePullReviewReply` | 2..=2 | 128 KiB in, 128 KiB out |
| query 29 | `GetPullReviewThread` | 2..=2 | 4 KiB in, 256 KiB out |
| query 30 | `ListPullReviewThreads` | 2..=2 | 8 KiB in, 1 MiB out |
| query 31 | `GetPullReviewThreadSubmission` | 2..=2 | 4 KiB in, 256 KiB out |
| query 32 | `GetPullReviewReply` | 2..=2 | 4 KiB in, 128 KiB out |
| query 33 | `ListPullReviewReplies` | 2..=2 | 4 KiB in, 1 MiB out |
| query 34 | `GetPullReviewReplySubmission` | 2..=2 | 4 KiB in, 128 KiB out |

Existing operations 1-31/1-28 must become schema 1..=2. Add a separate
descriptor constructor for schema-2-only operations; do not mark new table
operations compatible with schema 1. The module becomes schema 1..=2 with
migrations `[1, 2]`. Include the new SQL and new source/codec files in
`repository_source_digest()` and change its domain tag from v1 to v2.

Do not renumber or reuse an existing ID. Register all new codecs in
`RepositoryModule::register`. Update the deterministic release-inspection test
for schema max, command/query counts, and the newly computed code digest only
after confirming two fresh registry builds produce the same digest.

Make the update contract explicit rather than overloading sentinel values:

```rust
struct UpdatePullReviewThreadInput {
    key: PullReviewThreadKey,
    actor: RepositoryAuthor,
    can_resolve: bool,
    version: u64,
    body: Option<String>,
    suggested_text: Option<Option<String>>,
    resolved: Option<bool>,
}
```

The nested suggestion option distinguishes “unchanged” from “clear.” The Cell
command compares `actor` with the stored author for content edits and requires
`can_resolve` for resolution edits. The HTTP handler sets `can_resolve` only
when the actor is the loaded pull author or `Principal::can_write` is true.
Use equivalent explicit typed inputs/outcomes for replies; do not encode
permission or mutation intent in empty strings.

`ListPullReviewThreadsInput` supports optional exact raw path, optional
resolved flag, optional current/outdated filter expressed by the live base/head
pair, `before`, and `limit`. Filter current in SQL as exact equality of both
anchor OIDs; filter outdated as the negation. Do not page first and filter in
the HTTP handler. Replies use normal descending cursor pagination.

### HTTP routes

Add these routes under the existing pull admission/CSRF boundary:

```text
GET|POST  /api/repos/{owner}/{name}/pulls/{pull}/threads
GET|PATCH /api/repos/{owner}/{name}/pulls/{pull}/threads/{thread}
GET|POST  /api/repos/{owner}/{name}/pulls/{pull}/threads/{thread}/replies
GET|PATCH /api/repos/{owner}/{name}/pulls/{pull}/threads/{thread}/replies/{reply}
```

List parameters: `path_hex`, `resolved=true|false`, `outdated=true|false`,
`before`, and `limit` (default 30, maximum 50). Reject unknown/invalid
combinations. Thread creation JSON:

```json
{
  "request_id": "UUID",
  "base_oid": "40 hex",
  "head_oid": "40 hex",
  "path_hex": "raw path hex",
  "side": "new",
  "start_line": 12,
  "end_line": 14,
  "body": "Explain the concern",
  "suggested_text": "replacement lines or null"
}
```

The handler must load the pull and canonical displayed comparison, require an
open pull with live branches and exact submitted OIDs, and call a typed helper
extracted from `api.rs` to:

- compare the exact snapshots and prove the path is changed;
- derive old/new blob IDs and content classification;
- reject binary/non-UTF-8/over-1-MiB content;
- prove the side exists and every selected line exists;
- reject old-side suggestions.

Then persist the derived anchor in one replay-safe Cell command. A branch may
advance after validation; this is safe because the thread remains anchored to
the exact validated commit/blob and will read as outdated. Never silently
retarget it.

Thread PATCH accepts `version` and either root content fields (`body` plus
`suggested_text`) or `resolved`, not an empty update. The root author may edit
content. Only the pull author or repository writer may change resolution.
Mixed content+resolution updates require both permissions. Reply POST uses a
request ID/body; reply PATCH uses version/body and author ownership.

Return author display name, anchor fields, lowercase `path_hex`, display path,
resolution metadata, version/timestamps, `outdated`, `can_edit`, `can_reply`,
and `can_resolve`. `outdated` is derived from the exact currently displayed
base/head; it is not persisted. Raise the pull router body limit only enough
for a 64 KiB body plus 64 KiB suggestion and JSON overhead (160 KiB); field
validation remains authoritative.

### UI behavior

Keep `ComparisonView` generic by adding an optional review configuration. Pull
creation and ordinary commit comparisons pass nothing and remain unchanged.
The pull Files Changed tab passes pull number, base/head, head ref, repository
access/archive state, CSRF, and refresh callbacks.

For each lazily loaded textual diff:

- fetch current threads for that `path_hex` only;
- render root/replies as public `@pierre/diffs` line annotations;
- use the dependency's public line selection callback for click plus Shift-click
  range selection on one side;
- show a composer beside the selection; publish, cancel, and reset selection;
- show reply, edit-own-message, resolve/reopen, loading, conflict, and failure
  states without optimistic lies;
- collapse resolved threads by default but keep a labeled expand control;
- give controls explicit path/side/line accessible names and restore focus
  after submit/cancel.

Above the diff workspace, fetch `outdated=true` conversations separately and
render a paginated collapsed section with path, side/range, short anchored head,
root message, replies, and resolution controls. This section must work when a
path no longer exists in the current changes response.

Suggestion composition is an explicit mode in the root composer, only for
`new`-side selections. Store the replacement without Markdown parsing. Display
it as escaped code/diff content, never `dangerouslySetInnerHTML`.

Place the pure suggestion algorithm in `pull-review-model.ts`:

1. Accept original UTF-8 file text, inclusive one-based start/end, and
   replacement text without a required trailing newline.
2. Split while preserving whether the original used LF or CRLF and whether it
   ended in a line terminator.
3. Replace complete selected lines, normalize replacement line separators to
   the file's existing separator (LF when the file has no separator), and
   preserve the original final-newline state unless the selected range includes
   the final logical line.
4. Reject an out-of-bounds range instead of guessing.

Show “Apply suggestion” only when the thread is current, unresolved, new-side,
has a suggestion, the pull is open, the repository is not archived, the actor
has write access, and the loaded new content is ordinary UTF-8 no larger than
the existing 900 KiB `/contents` update ceiling. On apply, call shared content
mutation code with:

```json
{
  "branch": "pull.head_ref",
  "expected_head": "pull.head_oid",
  "expected_blob": "thread.new_blob_oid",
  "path_hex": "thread.path_hex",
  "content": "result of pure replacement",
  "message": "Apply suggestion from review thread #N"
}
```

Surface stale/protected/unsupported failures in the thread card. On success,
refresh the pull comparison and thread lists; the old thread must move to the
outdated section.

## Steps

### Step 1: Prove the diff dependency and lock the product types

Run the dependency contract gate. Then add the pure TypeScript model/types and
unit tests for anchors, response shapes, line labels, and suggestion
replacement. Cover LF, CRLF, no final newline, empty deletion, multibyte text,
one line, final line, and out-of-bounds input. No React or network code yet.

**Verify**:

```sh
npm run test --prefix packages/repository -- pull-review-model.test.ts
npm run typecheck --prefix packages/repository
git diff -- packages/repository/package.json packages/repository/package-lock.json
```

Expected: tests/typecheck pass; the dependency files have no diff.

### Step 2: Add schema 2 and typed Cell operations

Add migration 2, the dedicated thread domain/codec files, repository exports,
operation descriptors, registration, digest inputs, and descriptor tests.
Creation must atomically insert replay submission plus thread; reply creation
does the same at thread scope. Replays with identical payload return the prior
record; changed payload under the same request ID returns `RequestConflict`.
All edit and resolution mutations are version-CAS and enforce author/resolver
permissions again in the command input/outcome contract.

Add focused Cell tests modeled on existing pull tests for:

- schema-1 database migration to schema 2 without changing existing pulls;
- command/query rejection at schema 1 and success after migration;
- non-UTF-8 path round trip;
- creation/reply replay and request conflict;
- thread/reply independent numbering per parent;
- invalid side/range/OID/body/suggestion inputs;
- edit ownership, resolution permission, version conflicts, reopen metadata;
- path/resolution/current/outdated filters and bounded pagination;
- encode/decode round trips and 1 MiB output bound.

**Verify**:

```sh
npm run build --prefix packages/repository
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-018-review-threads" \
  cargo test -p crab-http-server --locked --lib review_thread
```

Expected: every new focused test passes; release descriptor reports schema max
2, 35 commands, and 34 queries.

### Step 3: Add exact-anchor HTTP/storage behavior

Extract the smallest typed comparison helper from `api.rs`; make both the
existing diff response and thread anchor validation call it. Add the dedicated
thread storage adapter rather than growing the already 863-line
`pulls/storage.rs` or 1,642-line Cell pulls module. Centralize pull comparison
selection in `pulls.rs` so detail, summary reviews, and threads do not disagree
about merged/live base/head state.

Implement routes, validation, views, replay lookup, authorization, pagination,
and error mapping. Do not expose issuer/subject values in JSON. Keep Cell
routing action names stable and specific, for example
`repository.pull.review_thread.read` and `.write`.

Extend `auth_tests/pulls.rs` with a real Git fixture that creates a pull,
creates a range thread as a second member, replies as the author, rejects an
outsider and unauthorized resolver, resolves/reopens with CAS, advances the
head, and proves the conversation moves from `outdated=false` to
`outdated=true` without losing its raw anchor or replies. Also cover missing
CSRF, archived repository mutation, binary/large/missing path, old-side
suggestion, stale comparison, invalid line, request replay, and pagination.

**Verify**:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-018-review-threads" \
  cargo test -p crab-http-server --locked --lib server::auth_tests::pulls
```

Expected: all pull route tests pass, including named current-to-outdated and
authorization cases.

### Step 4: Render and mutate inline conversations

Add the review thread component/model integration, optional comparison props,
public dependency callbacks/annotations, current and outdated views, composer,
reply/edit, resolve/reopen, and error/loading/empty states. Preserve lazy diff
loading and do not fetch path threads until that diff loads. Keep each path's
request cancel-safe on unmount/base/head change.

Extend the browser fixture routes and test a single range conversation through
create, reply, edit, resolve, reopen, split/unified rendering, head change, and
the outdated section. Run accessibility checks after composer open, rendered
thread, resolved collapse, and the 360 px viewport.

**Verify**:

```sh
npm run typecheck --prefix packages/repository
npm run test --prefix packages/repository
npm run test:browser --prefix packages/repository -- pulls.e2e.ts
npm run format:check --prefix packages/repository
```

Expected: all commands pass; existing pull creation/general review coverage
still passes.

### Step 5: Apply suggestions through the canonical content route

Extract/reuse the existing content mutation client from `content-editor.tsx`.
Wire the tested pure replacement helper to Apply suggestion with the exact
payload above. Do not copy server content-building logic into the thread API.
On conflict, leave the suggestion visible and current UI state honest; on
success, reload the pull and show the thread under outdated conversations.

In `pulls.e2e.ts`, assert the exact PATCH `/contents` request including branch,
head, blob, raw path, transformed LF/CRLF content, and message. Return a new
commit/head from the fixture and assert the thread becomes outdated. Cover
protected/stale error rendering and absence of Apply for readers, old-side,
resolved, outdated, binary, oversized, closed, or archived cases.

**Verify**: repeat all four UI commands from Step 4. Expected: all pass; no
new endpoint or server commit builder exists for suggestion application.

### Step 6: Qualify remote ownership, migration, and documentation

Extend `public_collaboration_requests_reach_remote_owner_over_mtls_and_publish_ltx`
to create a thread, reply, resolve, lose/switch the Cell owner as the test
already does, and read the exact conversation afterward. The existing ignored
RustFS variant then exercises the same path when explicit isolated credentials
are supplied; do not print credentials or run against a shared prefix.

Update README/REFERENCE route and feature maps, schema 2 operation inventory,
authorization, exact anchor/outdated semantics, suggestion publication path,
and qualification evidence. State explicitly that pending reviews and fuzzy
remapping are not implemented.

**Verify**:

```sh
npm run build --prefix packages/repository
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-018-review-threads" \
  cargo test -p crab-http-server --locked --lib \
  server::peer_e2e_tests::public_collaboration_requests_reach_remote_owner_over_mtls_and_publish_ltx
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-018-review-threads" \
  cargo test -p crab-http-server --locked --lib
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-018-review-threads" \
  cargo clippy -p crab-http-server --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: all non-ignored server tests, lint, formatting, and diff hygiene pass.
The provider-backed ignored test is a release/environment gate, not a reason to
use undeclared credentials locally.

### Step 7: Review scope and code size

Review `git diff --numstat`. New dedicated domain/component files should keep
growth out of already large owners. Remove duplicated path/OID/body validation,
diff reading, fetch handling, and content mutation code. Confirm each production
branch is required by the product contract above.

**Verify**:

```sh
git diff --numstat 892720ce6a6 -- \
  crates/crab-http-server packages/repository
git status --short
```

Expected: only in-scope files changed; no generated `dist`, dependency, snapshot,
baseline, or unrelated files appear.

## Test plan summary

### Rust Cell tests

- Migration 1 -> 2 and operation schema gating.
- Exact replay/conflict for thread and reply creation.
- Raw non-UTF-8 path persistence.
- Validation, CAS, permissions, pagination, filters, codec limits.
- Existing general comments/reviews and approval decisions remain unchanged.

### Rust HTTP/integration tests

- Real Git diff anchor validation for added, removed, context, and range lines.
- Current -> outdated after push; vanished path remains listable.
- Member/author/writer/outsider, CSRF, archive, closed pull, and resolver rules.
- Public write -> remote Cell owner -> LTX publication -> owner transition ->
  visible exact thread/reply/resolution state.

### TypeScript unit tests

- Inclusive line range replacement across LF/CRLF, final newline, empty
  deletion, Unicode, invalid ranges, and unchanged surrounding bytes.
- Pure current/outdated and apply-eligibility decisions if extracted.

### Browser tests

- Select line/range in split and unified views; create/cancel/failure.
- Reply/edit/resolve/reopen and resolved collapse.
- Outdated section after head change/path disappearance.
- Exact suggestion PATCH and success/conflict/protection behavior.
- Keyboard/focus, axe, dark theme, and 360 px layout.

## Done criteria

- [x] Repository module has an append-only schema-2 migration; old data survives
      and new operations cannot execute at schema 1.
- [x] New command/query IDs are unique, codecs registered, source digest covers
      all new behavior, and release descriptor determinism is tested.
- [x] Thread/reply mutations are durable, replay-safe, version-CAS, paginated,
      and authorized at both HTTP and Cell command boundaries.
- [x] Thread anchors are server-validated against exact changed snapshots and
      retain raw paths plus old/new blob IDs.
- [x] Current threads render inline; resolved threads collapse; outdated or
      vanished-path threads remain visible without fuzzy remapping.
- [x] Suggestions apply only through exact-head/exact-blob `/contents`; a
      successful apply creates a normal commit and makes the thread outdated.
- [x] General review summary and decision behavior remains covered and passes.
- [x] UI typecheck, unit tests, pull browser test, format check, and build pass.
- [x] Full non-ignored `crab-http-server --lib`, strict Clippy, Rust format, and
      remote-owner collaboration tests pass with the external Cargo target.
- [x] README/REFERENCE describe routes, auth, schema, outdated semantics,
      suggestion behavior, limits, and deliberate first-release exclusions.
- [x] `git status --short` lists only approved scope and `git diff --check`
      exits 0.
- [x] Plan 018 status is updated in `advisor-plans/README.md`.

## STOP conditions

Stop and report instead of improvising if:

- The pinned `@pierre/diffs` declarations lack public line selection, inclusive
  range state, or React line annotations in both split and unified views.
- Implementing the UI would require private DOM/shadow-DOM coupling or a
  dependency/lockfile change.
- Schema version 2, command IDs 32-35, or query IDs 29-34 are occupied after
  rebasing, or another active change edits the repository migration descriptor.
- Release activation cannot safely gate schema-2-only operations during a
  schema-1 -> schema-2 rollout under the existing Cell compatibility contract.
- Exact changed-path/blob/line validation cannot reuse the canonical remote-Git
  snapshot/diff primitives without a second read algorithm.
- Applying a suggestion cannot use the existing `/contents` exact-head and
  exact-blob path without weakening branch protection or publication checks.
- A required change falls outside scope, any focused verification fails twice,
  `$HOME/Workspace` is unavailable, or a test would require shared/live cloud
  credentials rather than an explicitly isolated provider prefix.

## Maintenance notes

- Any future rebase/fuzzy-remap feature must be a new migration/API contract;
  never mutate these immutable anchors in place.
- Pending review batching would need a draft owner/lifecycle and atomic submit
  semantics. Do not retrofit it by hiding published threads in the UI.
- If content update limits or protected-branch rules change, review suggestion
  eligibility and browser error states; `/contents` remains authoritative.
- Reviewers should scrutinize schema compatibility, operation IDs, replay
  digests, raw path handling, SQL-filtered pagination, exact comparison reuse,
  and the absence of a second Git commit path.
- Provider-backed RustFS qualification remains an explicit release gate. Local
  in-memory remote-owner proof is required for this implementation but does not
  claim provider evidence.
