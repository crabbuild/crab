# Plan 027: Sign measured rebalance inputs

> **Executor**: Work from the isolated `cell-safe-rebalance` worktree based
> on `origin/main`; retain the existing plan documents.
> Read root and `crates/AGENTS.md` before edits. This plan stands alone:
> complete its gates and report any missing proof. Do not change a signed wire
> contract on a guess.
>
> **Drift check**: `git diff --stat cebc909940f137e4bd8445e524e77a154bf51a29..HEAD -- crates/crab-cell-runtime/src/{actor.rs,node.rs,placement.rs} crates/crab-http-server/src/peer.rs`.
> Re-read changed functions and their tests if this is nonempty.

## Status

- Priority: P0; effort: L; risk: HIGH; category: correctness/feature.
- Depends on: none. Planned at `cebc909940f137e4bd8445e524e77a154bf51a29`, 2026-09-21.
- Status: IN PROGRESS; implementation in this branch, acceptance gates pending.

## Why and current state

Proactive movement cannot use backlog today. `PlacementObservation` already
has publication, hydration, and primitive backlog fields, but
`PlacementObservation::from_signed_advertisement` fills each with zero
(`crates/crab-cell-runtime/src/placement.rs:112-165`). The signed
`NodePlacementCapacity` has only memory/disk totals and Cell/job counters
(`src/node.rs:47-90`). `NodePublisher::advertisement` samples
`CellRuntime::stats` and signs that block (`crates/crab-http-server/src/peer.rs:406-486`);
the actor owns queued publications, accepted work, and hydration state
(`src/actor.rs:1570-1610`). `NodeDirectory::live` validates the signed
session before placement (`src/node.rs:1235+`). Maintain that boundary.

The version-1 placement block first appears in commit `d64443371f0`.
At planning time, `git tag --contains d64443371f0` returned no tags.
Recheck this before editing: a newly shipped tag changes the wire decision.
The canonical design requires signed, short-lived observations and treats
missing values as ineligible for proactive movement
(`crates/crab-cell-runtime/docs/canonical-ltx-scaling.md:574-600`).

## Scope and contract

In scope: `crates/crab-cell-runtime/src/actor.rs`, `node.rs`,
`node/tests.rs`, `placement.rs`, their focused tests,
`crates/crab-http-server/src/peer.rs`, peer tests, and the canonical scaling
doc. Out of scope: authority/control, storage layout, app descriptor,
provider construction, new environment variables, and movement execution.
Use `Result<T, Error>`, bounded integer counters, structured tracing, and
no production `unwrap/expect/panic`, matching neighboring Rust modules.

## Steps and gates

1. Run `git tag --contains d64443371f0`. If empty, update the one canonical
   signed placement shape; if nonempty, STOP and design a versioned rollout
   before changing bytes. Preserve session, timestamp, signature, and
   canonical decoding checks. **Gate:** focused `node::tests::placement_schema_is_mixed_version_safe_and_fail_closed`
   passes; an unknown version cannot receive proactive movement.
2. Add one actor-owned bounded snapshot of publication, hydration, and
   primitive backlog. Define each unit in doc comments; derive it from
   existing queues/effects/ledger, never a second scheduler or guessed zero.
   Reject or saturate invalid/oversized values consistently with current
   `CellRuntimeStats::placement_*` methods. **Gate:** actor snapshot tests
   show each backlog increases and returns to zero after its actual work
   completes; unknown accounting remains unknown.
3. Have `NodePublisher` sign that snapshot with the existing Cell/job and
   memory/disk sample. Map signed fields into `PlacementObservation`.
   A draining or stale node must be ineligible even if its prior sample had
   headroom. **Gate:** peer publisher test verifies signed fields against
   runtime state; forged, stale, malformed, and future-version samples fail
   closed; placement tests show backlog lowers score.
4. Update the canonical design's “Fleet observation” paragraph to describe
   the implemented units and rollout gate. **Gate:** `git diff --check`
   exits zero and the doc names no unimplemented input as measured.

## Verification

Before any compiling Cargo command, run
`test -d "$HOME/Workspace" && test -w "$HOME/Workspace"`, then create only
`$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance`. If unavailable,
STOP; never build into this checkout. Set
`CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance`
on **every** Cargo invocation. Run:

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-runtime placement --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-runtime node::tests --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-http-server peer::tests --locked --lib
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo clippy -p crab-cell-runtime -p crab-http-server --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: all exit zero. Missing dependencies: `make install` from `crab/`
with the same external target, then retry once; report the first actionable
failure. Do not edit snapshots/baselines to pass.

## Acceptance criteria

- [ ] A live signed sample carries measured Cell, memory, disk, job, and three
  backlog inputs with documented units; no backlog is synthesized as zero.
- [ ] Tampering, staleness, unsupported version, and draining reject proactive
  destination eligibility; ordinary authority routing remains intact.
- [ ] The producer and both placement conversion paths use the same signed
  contract, and focused runtime/server tests plus format/Clippy pass.
- [ ] No authority, app descriptor, storage key, or provider contract changes.

## STOP and maintenance

Stop if version 1 is in a release tag, the runtime cannot measure a required
backlog without a second source of truth, or signing requires a dependency
patch. Keep placement schema changes tied to signature and mixed-version tests.
Future new score inputs need monotonicity and missing-value tests.
