# Standalone replication compatibility audit and decision

Status: IN PROGRESS — tagged export/caller/proof audit is recorded; authorized retain/deprecate/remove decision is still pending
Priority: P1
Effort: M
Risk: High
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependency: plan 015's canonical qualification receipts

## Executor instructions

Implement on `codex/016-standalone-replication-decision`. This plan is an audit
and recorded product decision. Do not delete or deprecate code here. Read all
standalone exports, implementations, examples/tests/docs, every workspace
caller, Git tags/releases, and available external consumer evidence. Treat
tagged source as potentially shipped even though the crate is `publish = false`.

## Drift check

```bash
git fetch --tags origin
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-ltx/src/lib.rs \
  crates/crab-ltx/src/replica.rs \
  crates/crab-ltx/src/replica \
  crates/crab-ltx/src/schedule.rs \
  crates/crab-ltx/src/paged* \
  crates/crab-ltx/tests \
  crates/crab-ltx/examples \
  crates/crab-ltx/UPSTREAM.md \
  crates/crab-ltx/PARITY.md \
  crates/crab-ltx/SCALABILITY.md
```

If a production workspace caller or newer tagged public use exists, add it to
the contract map before any recommendation.

## Why this plan exists

`crab-ltx` contains canonical Cell mechanics (`CellReplica`) and older
standalone replication/scheduling/paged surfaces. No current production
workspace caller was found, but the standalone source exists in tag `v1.2.4`
and has docs/examples/tests. Repository policy allows hard removal only after a
named shipped-contract decision and proof migration; absence of an internal
caller is not sufficient.

## Required audit output

Create a decision record under the existing architecture documentation with:

- exact exported standalone symbols/features/modules;
- internal callers by dependency kind and target;
- examples, docs, and tests that teach/support them;
- storage prefixes, serialized shapes, and on-disk formats;
- first/last tagged release containing each surface;
- external consumers found through available code/package/release searches;
- unique invariants/tests not yet covered by canonical `CellReplica`;
- migration feasibility and explicit non-goals;
- one named decision: **retain**, **deprecate with deadline**, or **hard remove**;
- approver/owner, decision date, target release, and evidence links.

Unknown external usage must be recorded as unknown, not converted to “none.”

## Implementation steps

1. Generate the export/caller inventory with `rg`, Cargo metadata, rustdoc JSON
   or equivalent stable tooling. Manually inspect macro/feature-gated exports.
2. Inspect tags containing each surface. Record whether binaries/examples were
   distributed and whether docs described them as supported. Do not infer
   non-shipment solely from `publish = false`.
3. Search available public source/release/package references. Record query,
   date, scope, and limitations. Never require credentials for private systems
   not in scope.
4. Map every standalone test invariant to canonical proof: exact equivalent,
   partial equivalent, or missing. Include scheduling, corruption, range/paged,
   recovery, compaction, provider, and scale evidence.
5. Compare maintenance cost and architectural conflict for the three options.
   “Retain” requires a distinct supported purpose and qualification owner;
   “deprecate” requires a deadline/migration; “hard remove” requires no retained
   contract or an approved breaking release/migration boundary.
6. Present the evidence to the named maintainer/product owner and record the
   explicit decision. If no authorized decision is available, mark the record
   `Decision pending` and stop; do not choose on behalf of the owner.
7. Link the decision from `UPSTREAM.md`, `PARITY.md`, `SCALABILITY.md`, and the
   canonical design without changing runtime behavior.

## Verification

```bash
rg -n "pub (mod|use|struct|enum|trait|fn)|cfg\(feature" \
  crates/crab-ltx/src/lib.rs crates/crab-ltx/src/replica.rs \
  crates/crab-ltx/src/replica crates/crab-ltx/src/schedule.rs
cargo metadata --format-version 1 --locked > /tmp/crab-016-metadata.json
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

Review the final inventory against the tagged tree and current tree. A second
reviewer must be able to trace every exported symbol and unique proof to a row.

## Acceptance criteria

- [x] Every standalone export, caller, example, test, stored shape, and tagged
      release is inventoried.
- [x] External-consumer research records scope/date/limitations and does not
      overclaim absence.
- [x] Every unique standalone proof maps to canonical evidence or a specific
      missing proof.
- [ ] The record contains one explicit retain/deprecate/hard-remove decision,
      named approver, date, target release, and migration boundary; otherwise it
      clearly says `Decision pending` and no execution begins.
- [x] No source/API/test deletion or behavioral deprecation occurs in this plan.
- [x] Architecture docs link one canonical decision record.

## Stop conditions

- No authorized owner can decide a shipped-contract boundary.
- A live production/external consumer cannot migrate under the proposed option.
- Tagged storage compatibility is unclear.
- Canonical qualification in plan 015 is incomplete.

## Maintenance note

Rerun the audit if a new tag ships before execution. The recorded decision, not
the original recommendation, controls plan 017.
