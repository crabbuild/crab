# Plan 032: Prove scale-up, scale-down, and transfer safety end to end

> **Executor**: Use the isolated `cell-safe-rebalance` worktree. Read root
> and `crates/AGENTS.md`, this plan, and the existing
> qualification README before running anything. Tests must observe real
> authority, restored content, and resources. Never manufacture evidence or
> weaken a profile threshold.
>
> **Drift check**: `git diff --stat cebc909940f137e4bd8445e524e77a154bf51a29..HEAD -- crates/crab-cell-runtime/tests crates/crab-http-server/tests crates/crab-cell-runtime/qualification crates/crab-cell-app/tests/reference_application.rs`.
> Confirm the current fixture behavior and receipt schema after any drift.

## Status

- Priority: P0; effort: XL; risk: HIGH; category: tests/release evidence.
- Depends on: plans 027–031. Planned at
  `cebc909940f137e4bd8445e524e77a154bf51a29`, 2026-09-21.
- Status: TODO.

## Why and current state

`crates/crab-cell-runtime/tests/actor.rs:2056+` proves one successor can
acquire a Cell after an explicit handle drain; `tests/actor.rs:1856+` proves
the current eviction predicate blocks persisted Queue/Workflow rows.
`crates/crab-cell-app/tests/reference_application.rs` exercises typed
primitive operations through public app/host contracts.
`crates/crab-http-server/tests/qualify_compose_cluster.sh` and
`crates/crab-cell-runtime/qualification/README.md` define the existing
multi-process/local and protected receipt boundaries. The README explicitly
says local/RustFS evidence is not provider/Kubernetes/scale qualification.
Code and mocked tests alone do not prove a user action reaches a different
serving node.

## Scope and evidence rules

In scope: focused integration tests in `crates/crab-cell-runtime/tests/`,
`crates/crab-cell-app/tests/reference_application.rs`,
`crates/crab-http-server/tests/`, qualification harness/documentation,
and only the narrow production fix required by a test-proven defect.
Out of scope: changing qualification thresholds, snapshots, expected-failure
lists, baselines, provider credentials, release attestations, or pretending a
local emulator is protected evidence. Store raw run artifacts outside Git;
never print credentials or private endpoints.

Use a stable seeded workload with SQL/KV/Blob/Queue/Cron/Workflow/Activity/
Effect cases. Each acknowledged marker gets an independent exact-value
observation after movement. Bind control owner session, epoch, revision,
root, and node reservations to each phase. A typed `CellNode` path and a
separate process/RustFS path are both required; neither substitutes for a
protected provider/Pod run.

## Steps and gates

1. Extend the public typed fixture to provision Cells and acknowledge unique
   bounded markers on two nodes, then add a third node. Trigger the
   controller, wait for a bounded release/restore result, and read every moved
   marker through the successor. **Gate:** source owner is gone, successor
   epoch rises, exact root/marker survives, and one node serves at a time.
2. Start planned scale-down with a clean Cell plus a Queue ready/leased
   message, a Workflow pending activity, and an Effect lease. At deadline,
   assert the unsettled Cells remain on their live source and the report names
   blockers. Settle each through public typed APIs; resume drain. **Gate:**
   zero owned Cells and baseline reservations only after settlement; no lost
   acknowledged marker.
3. Inject failures at quiesce, publication, release-response loss, receiver
   admission, receiver death, and simultaneous receiver claims. Reuse the
   runtime's process-test-support fault hooks where available. **Gate:**
   before release the original owner or pinned recovery obligation remains;
   after release the exact Idle root or one newer owner remains. No dual
   output-capable owner, root regression, or leaked permit.
4. Run a local three-process/RustFS case through the existing Compose harness
   or a focused sibling script. Measure actual concurrent moves, projected
   bytes, receiver activations, convergence time, latency, and residual
   reservations. Validate its receipt with the current verifier in a fresh
   process. **Gate:** user action -> real authority side effect -> visible
   successor result, all bounds enforced, and the receipt identifies this as
   local evidence.
5. Record the separate protected provider/Kubernetes/scale work as an open
   release gate if no isolated environment and pinned candidate image exist.
   Do not mark it complete from the local run. **Gate:** qualification docs
   state exact tests run, profile/image/source binding, and remaining gates.

## Commands and proof

Preflight `test -d "$HOME/Workspace" && test -w "$HOME/Workspace"`; create
only `$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance`. Stop if
unavailable. Keep real-repository fixtures under `$HOME/Workspace/Github`
and read-only; keep generated artifacts out of tracked source.

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-app --test reference_application --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-cell-runtime --features process-test-support --test actor --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo test -p crab-http-server --locked --test public_cell_qualification
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-safe-rebalance cargo clippy -p crab-cell-app -p crab-cell-host -p crab-cell-runtime -p crab-http-server --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

All commands exit zero. Inspect Compose targets/scripts before running them:
any binary lookup must use the external target path; do not trigger a local
second build. Build the candidate image before a Compose run. If missing
dependencies cause a failure, `make install` from `crab/` with the same
target, retry once, then report. Run broad suites and real provider/Pod proof
in CI or a dedicated isolated environment. Never run bucket-wide GC.

## Acceptance criteria

- [ ] Scale-up and scale-down each pass a Level-3 user-action-to-visible-result
  test using real authority CAS and distinct node runtimes/processes.
- [ ] Every acknowledged marker is exact after movement; owner epoch/root
  remain monotonic and there is never dual serving.
- [ ] Unsettled Cells stay on a live source; deadline reports blockers and
  later settlement permits clean drain with zero reservations.
- [ ] Fault cases prove pre/post-release behavior and the measured movement
  concurrency/byte/rate limits; local and protected evidence are labeled
  separately.
- [ ] No threshold/baseline/snapshot was relaxed; focused tests, format, and
  Clippy pass. Any unavailable protected gate stays explicitly open.

## STOP and maintenance

Stop on lost acknowledgements, dual authority, root regression, leaked
reservation, an unavailable mounted build volume, or a harness that can only
simulate the claimed boundary. Keep the raw receipts and their source/image
digests for review. New primitive or movement causes must extend the
settlement and fault matrix, not merely a planner unit test.
