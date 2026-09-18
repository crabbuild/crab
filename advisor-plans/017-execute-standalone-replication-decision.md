# Execute the recorded standalone replication decision

Status: BLOCKED until plan 016 records an approved decision
Priority: P1
Effort: L-XL depending on decision
Risk: High
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependency: approved plan 016 decision and complete proof map

## Executor instructions

Create `codex/017-standalone-replication-decision` only after the decision
record names exactly one option and approver. Read the entire decision, every
mapped source/test/doc, current `crab-ltx` exports, canonical Cell tests, and all
consumers. Do not implement a different option because it seems easier. Use a
unique external Cargo target.

## Drift check

```bash
git fetch --tags origin
git diff --stat 4a77b6f1252a..origin/main -- crates/crab-ltx crates/crab-cell-runtime
```

Regenerate the export/caller/tag inventory and compare it to plan 016. Stop if
new consumers, exports, stored formats, or tags are absent from the decision.

## Why this plan exists

The standalone surface cannot remain in an ambiguous half-retired state after
the compatibility audit. Execution must follow the approved shipped-contract
decision, migrate every unique proof first, and leave exports, tests, stored
data guidance, and qualification consistent. Keeping all three branches in one
conditional plan prevents an implementer from assuming that “no workspace
caller” authorizes hard removal.

## Universal rules

- Port unique safety proof before removing its old test owner.
- Never reinterpret standalone storage prefixes as Cell roots.
- If migration is approved, use an explicit offline export/import tool with
  exact-root verification; do not add a production fallback reader.
- Do not keep aliases, shims, renamed wrappers, indefinite feature aliases, or
  dead tests merely to reduce diff.
- The `replica` feature currently enables required Cell remote mechanics. Keep
  its name unless the same approved breaking release explicitly renames it;
  never add an indefinite alias.
- Streaming work and canonical qualification must already be complete.

## Implementation steps

Select exactly one branch below from the approved plan-016 decision. Do not
combine branches or infer a different migration boundary from the current
workspace caller inventory.

## Branch A: retain

Choose this branch only if plan 016 says **retain**.

1. Define the standalone purpose and owner distinct from `CellReplica`.
2. Narrow exports to that purpose without breaking the recorded contract.
3. Remove only proven internal duplication by sharing low-level mechanics; do
   not make Cell authority policy leak into the standalone API.
4. Add a separate support/qualification matrix and receipts for every retained
   provider, scheduling, paging, recovery, and scale claim.
5. Update docs so users can distinguish standalone from canonical Cell runtime.

Retain acceptance: every retained export has a caller/use case, owner, tests,
and qualification; no ambiguous “legacy” surface remains undocumented.

## Branch B: deprecate with deadline

Choose this branch only if plan 016 says **deprecate**.

1. Apply the exact approved deprecation release and deadline.
2. Add compile-time deprecation only to public symbols covered by the decision,
   with migration documentation; do not add runtime warnings or fallback reads.
3. Ship any approved offline migration tool and prove source -> exact canonical
   root -> restore -> byte/state equality. It must be idempotent and leave the
   source unchanged on failure.
4. Keep safety tests until the removal release, while porting unique invariants
   to canonical tests immediately.
5. Create the separately reviewed removal change at the deadline; do not leave
   the deprecated surface indefinitely.

Deprecate acceptance: tagged migration path is executable and exact, deadline
is machine/owner tracked, and no new caller can be added without a gate.

## Branch C: hard remove

Choose this branch only if plan 016 says **hard remove**.

1. Port every missing unique invariant to `CellReplica`/runtime tests and make
   those tests pass before deleting standalone code.
2. Remove standalone modules, exports, examples, tests, docs, feature branches,
   and dependencies as one coherent breaking change. Delete rather than wrap.
3. Search the full workspace and generated docs for retired names/prefixes.
   Remaining references must be historical migration notes or explicit negative
   tests, not callable code.
4. Re-run minimal/default and `replica` feature builds to prove the manifest and
   exports remain coherent. Review `Cargo.lock` if dependency removal changes it.
5. Update architecture, provenance, parity, scalability, and release notes with
   the approved breaking boundary and proof migration table.

Hard-remove acceptance: no callable standalone surface remains; all retained
invariants pass on the canonical path; no compatibility reader/alias exists.

## Verification for every branch

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-017-standalone \
  cargo test -p crab-ltx --no-default-features --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-017-standalone \
  cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-017-standalone \
  cargo test -p crab-cell-runtime --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-017-standalone \
  cargo test -p crab-http-server --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-017-standalone \
  cargo clippy -p crab-ltx -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
make -C crab architecture-check
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

Also run plan 015's exact provider/fault qualification for the selected branch.
For hard removal, `rg` every retired symbol/module/prefix from plan 016 and
classify the zero or historical-only results in the PR.

## Acceptance criteria

- [ ] The implemented branch exactly matches the approved decision record.
- [ ] Inventory drift is reconciled before edits.
- [ ] Every unique standalone invariant has canonical proof before its old test
      is removed.
- [ ] No stored prefix is reinterpreted and no runtime fallback reader is added.
- [ ] No alias/shim/parallel canonical path remains unless explicitly retained
      as the approved supported contract.
- [ ] Documentation, exports, manifests, examples, tests, and release notes all
      match the selected decision.
- [ ] Minimal, replica, runtime, server, architecture, docs, lint, and provider
      qualification all pass.

## Stop conditions

- Decision is pending, ambiguous, lacks an approver, or no longer covers current
  exports/consumers/tags.
- A unique safety invariant has not yet been ported.
- Migration cannot prove exact restored state.
- Removal would require an unapproved dependency patch/vendor change.

## Maintenance note

Close this plan with the final export list and qualification receipt IDs. If the
decision is deprecation, this plan remains incomplete until the deadline removal
change lands or a new approved decision supersedes it.
