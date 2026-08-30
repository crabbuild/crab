# Plan 011: Make add publication recoverable and prepared-aware

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving on. If a
> STOP condition occurs, stop and report; do not add a fallback staging path.
> When done, update this plan's status row in `plans/README.md` unless a
> reviewer told you they own the index.
>
> **Drift check (run first)**:
> `git diff --stat 86139f55..HEAD -- crab/src/cmd/add.rs crab/src/git/clean.rs crab/src/cmd/install.rs crab/src/cmd/doctor.rs crates/crab-staging/src/index.rs crates/crab-staging/src/lib.rs crates/crab-staging/src/stream.rs crab/tests/e2e_add_commit_push.rs packages/web/content/docs/cli/reference/crab-add.mdx packages/web/content/docs/cli/getting-started/mirror-mode.mdx`
> If any in-scope publication, lease, clean-filter, or mirror-hook code changed,
> compare it with the current-state facts below before editing. A changed commit
> boundary is a STOP condition until the plan is reconciled.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: none
- **Category**: bug, tech-debt, docs
- **Planned at**: commit `86139f55`, 2026-08-29
- **Delivery status**: DONE — focused, full-workspace, E2E, and RustFS
  acceptance proof passes; delivered in the combined v1 hardening change set.

## Why this matters

`crab add` commits pointer blobs to Git's index before it marks their staging
batches published. A crash or marker failure can leave a valid Git pointer
whose durable local recipe is invisible to push. Separately,
`crab add --skip-git-add` leaves direct prepared-only recipes open; a later
ordinary `git add` cannot reuse them and can fail with a divergent staged
recipe. This plan makes the Git-index/staging boundary recoverable and gives
the clean filter one prepared-aware promotion path.

## Current state

- `crab/src/cmd/add.rs:1302-1365` closes staging, commits the Git index, then
  calls `mark_closed_staging_batches_published`; marker failure is only logged.
- `crab/src/cmd/add.rs:2310-2318` publishes batches one at a time rather than in
  one SQLite transaction.
- `crab/src/cmd/add.rs:2410-2418` leaves every `--skip-git-add` batch open.
- `crates/crab-staging/src/index.rs:451-495` models only `open`/`published`
  batches and path leases keyed by `(batch_id, path_bytes)`; there is no durable
  Git-index publication intent.
- `crates/crab-staging/src/index.rs:2008-2027` exposes only verified recipes
  with published path leases to push. Keep this fail-closed boundary.
- `crab/src/git/clean.rs:479-483` asks only for a published recipe. On a miss,
  `crab/src/git/clean.rs:1998-2018` stages provisional segment rows.
- `crates/crab-staging/src/index.rs:2988-3038` compares provisional and target
  segment sequences during adoption. A direct prepared-only target has no
  segment sequence, so skip followed by `git add` can report divergent recipes.
- `crates/crab-staging/src/lib.rs:2204-2227` is the existing exemplar for
  create → record exact recipe lease → publish, with rollback on error. Keep
  error sources typed through `StagingError`; do not stringify them away.
- `crab/src/cmd/install.rs:585` installs a mirror hook that runs
  `crab add . --skip-git-add` immediately before push, even though open batches
  are not push-visible.
- Public docs claim Crab runs `git add` and recommend skip followed by manual
  `git add` (`packages/web/content/docs/cli/reference/crab-add.mdx:28-30`,
  `:76-81`). Production writes the Git index directly.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Staging tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked` | all pass |
| Add focused tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked cmd::add::` | all matching tests pass |
| Clean focused tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked git::clean::` | all matching tests pass |
| Add E2E | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --test e2e_add_commit_push --locked` | all pass |
| Docs links | `cd packages/web && npm run check:links` | exit 0 |

Before any Cargo command, verify `/Volumes/Workspace` is mounted and use the
exact external target above. Do not fall back to a checkout-local `target/`.

## Scope

**In scope**:

- `crates/crab-staging/src/index.rs`
- `crates/crab-staging/src/lib.rs`
- `crates/crab-staging/src/stream.rs` only for representation-aware tests
- `crab/src/cmd/add.rs`
- `crab/src/git/clean.rs`
- `crab/src/cmd/doctor.rs`
- `crab/src/cmd/install.rs`
- `crab/tests/e2e_add_commit_push.rs`
- `packages/web/content/docs/cli/reference/crab-add.mdx`
- `packages/web/content/docs/cli/getting-started/mirror-mode.mdx`

**Out of scope**:

- Changing pointer v1 serialization or Git object IDs.
- Making open recipes visible to push.
- Adding a second staging database or legacy compatibility reader.
- Remote classification, xorb packing policy, shard layout, or GC.
- New config/environment switches. Recovery is mandatory product behavior.
- A staging schema v2, compatibility reader, backfill, or dual-write period.
  Replace the unshipped schema and keep the surviving schema version named v1.

## Git workflow

- Branch: `advisor/011-transactional-add-publication`
- Use small conventional commits, for example
  `fix: journal add index publication` and `test: cover prepared skip promotion`.
- Do not push or open a PR unless instructed.
- Read `crates/AGENTS.md` before editing the shared staging crate.

## Steps

### Step 1: Characterize the two failures before changing ownership

Add tests that prove current semantics and fail on the defects:

1. A direct prepared-only `crab add --skip-git-add <new-file>` followed by
   ordinary `git add <file>` must produce the exact pointer and a published
   recipe without creating segment authority for that file.
2. The same workflow for a changed already-indexed path must replace the old
   pointer and publish only the new recipe.
3. Inject process-boundary states around direct index replacement: intent
   durable/index old, intent durable/index new, and index new/publication
   incomplete. Reopen/reconcile must roll back the first and publish the latter
   two only when every recorded path matches its expected pointer OID.
4. A mismatched or malformed index entry must never publish its recipe.

Use `crab/tests/e2e_add_commit_push.rs:377` as the CLI E2E style and the
SQLite-focused lifecycle tests near `crates/crab-staging/src/index.rs:4978` as
the transaction style.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --test e2e_add_commit_push --locked skip_git_add -- --nocapture`
→ new skip tests reproduce the failure before the fix, then pass after Steps
2-4. Keep the tests committed with the fix, not as expected failures.

### Step 2: Add one durable publication-intent contract

In `crates/crab-staging/src/index.rs`, add the journal directly to the one
canonical staging schema, whose version remains v1. It records one add
transaction ID plus exact `(batch_id, path_bytes, recipe_hash,
expected_pointer_oid)` rows. Provide narrow APIs through
`crates/crab-staging/src/lib.rs` to:

- create the complete intent in one transaction before Git index mutation;
- atomically publish every recorded batch and clear the intent after Git index
  replacement succeeds;
- list unresolved intents for product-layer reconciliation;
- atomically roll back an intent and return recipes that became unleased.

The staging crate must not read Git. `crab/src/cmd/add.rs` owns comparison of
the journal's expected pointer OIDs with the actual Git index. Publishing a
partial subset of one add transaction is forbidden.

This is a hard cutover. Delete obsolete schema branches, readers, tests, and
compatibility code. A pre-cutover local staging database is disposable: fail
with one actionable instruction to remove/recreate staging and restage the
affected paths. Do not backfill it, infer intent from old rows, or accept both
shapes at runtime.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-staging --locked publication_intent`
→ tests pass for create, atomic publish, exact rollback, reopen, idempotent
reconciliation, and fail-closed rejection of the retired schema.

### Step 3: Wire add and recovery to the journal

In `crab/src/cmd/add.rs`:

1. Derive pointer bytes/OIDs before replacing the Git index.
2. Persist the complete intent before calling the index writer.
3. On a known pre-mutation error, roll back the intent and its batches.
4. On uncertain mutation, preserve the intent and return the original error.
5. After a successful index replacement, publish all intent batches in one
   SQLite transaction. If that transaction fails, return an actionable error;
   do not log success.

Add an idempotent product-layer reconciler invoked before add and push staging
lookup. For every unresolved intent, compare all exact paths/OIDs with the
current Git index. Publish only on a complete match; roll back only on a
complete non-match that proves the old index remained. Mixed/ambiguous state
must fail closed and be reported by `crab doctor` with the transaction ID and
repair instructions—never guess per path.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked publication_reconciliation`
→ success, pre-mutation failure, post-index crash, repeat reconciliation, and
mixed-index refusal tests all pass.

### Step 4: Make the clean filter reuse exact open prepared authority

Add a staging query that finds a caller-verified local recipe by exact recipe
hash regardless of segment/prepared physical authority. It may return an open
recipe only to the local clean-filter promotion path; do not reuse it in
`published_recipe_for_file`.

Change `crab/src/git/clean.rs` so the sealed recipe is checked before
provisional adoption. On an exact local match, discard provisional rows and
create the clean filter's normal published path lease for that exact recipe.
An unequal recipe, corrupt payload authority, or ambiguous multiple match must
fail; it must not silently rechunk into a second authority.

Do not mark the original multi-path skip batch published: that would expose
sibling paths not processed by this `git add`. Plan 012 will reclaim the
superseded open lease after the new published lease becomes current.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --lib --locked clean_stream -- --nocapture`
→ existing clean tests and new prepared-open promotion tests pass; assertions
prove zero new segment bytes for the promoted file.

### Step 5: Remove the ineffective mirror warm-up and correct docs

After the lifecycle tests pass, remove `crab add . --skip-git-add` from newly
installed mirror pre-push hooks. A pre-push hook must push recipes already
published when commits were created; staging dirty worktree content cannot
make uncommitted bytes part of the ref being pushed. Update hook installation
tests and document how existing hooks are refreshed using the existing install
command—do not add a runtime hook shim.

Update the add reference to say Crab writes pointer objects and the Git index
directly. Describe `--skip-git-add` as local preparation whose exact recipe is
consumed by a later clean-filter add after Step 4, and explain that it does not
make recipes push-visible by itself. Update mirror-mode docs to match the new
hook.

**Verify**:
`cd packages/web && npm run check:links`
→ exit 0, and
`rg -n "crab add \. --skip-git-add" crab/src/cmd/install.rs packages/web/content/docs/cli/getting-started/mirror-mode.mdx`
→ no matches.

## Test plan

- Unit: publication-intent schema, all-or-nothing transitions, reopen and
  idempotency, exact local recipe lookup across segment and prepared authority.
- Integration: direct prepared skip → `git add`; segment skip → `git add`;
  multi-path skip then add one path; crash after Git index replacement;
  mismatched/ambiguous index state; push after reconciliation.
- Regression: normal `crab add` still stages pointer blobs directly, ordinary
  `git add` still publishes newly chunked recipes, and push never sees open
  batches.
- Failure assertions must inspect published/open lease counts and physical
  segment/prepared bytes, not only command exit status.

## Done criteria

- [x] A durable, exact, all-or-nothing publication intent exists before index mutation.
- [x] Add returns no success when index publication is committed but staging publication is unresolved.
- [x] Reopen reconciliation publishes only a complete exact index match and refuses mixed state.
- [x] Direct prepared `--skip-git-add` followed by `git add` succeeds without segment duplication.
- [x] Push remains restricted to published recipes.
- [x] Only canonical staging schema v1 is readable/writable; retired schema code and tests are deleted.
- [x] Newly installed mirror hooks no longer run the ineffective skip command.
- [x] Focused staging, add/clean, E2E, format, and docs-link commands pass.
- [x] The combined v1 hardening change set is committed locally; coupled plans
  share staging and push ownership instead of preserving parallel paths.

## STOP conditions

Stop and report if:

- Git index replacement can partially commit the exact path set on any
  supported Git version; the transaction/reconciliation model must be revised.
- Exact pointer OIDs cannot be derived before index replacement without
  changing pointer serialization.
- Prepared authority cannot be validated without loading unbounded payloads;
  do not weaken verification.
- Fixing installed mirror hooks appears to require preserving the obsolete
  hook shape at runtime; stop instead and define a hard reinstall cutover.
- Any proposed recovery would publish an open sibling path not proven present
  in the Git index.

## Maintenance notes

The publication intent is the durable boundary between Git and staging. Future
add/index optimizations must either use it or establish a stronger single
transaction; warnings are not recovery. Reviewers should scrutinize SQLite
transaction scope, exact path encoding, index ambiguity, and cancellation
between intent creation and Git mutation. Plan 012 depends on the resulting
promotion/publication APIs and owns final superseded-lease reclamation.
