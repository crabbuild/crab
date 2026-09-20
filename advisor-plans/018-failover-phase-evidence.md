# Failover phase telemetry and qualification baseline

Status: PARTIAL — phase evidence and typed identity binding implemented; timestamp/work extensions and protected runs pending
Priority: P0
Effort: M
Risk: Medium
Planned against: `c86dd43423ae` (`origin/main`, 2026-09-20)
Design authority: `advisor-plans/follower-affine-failover-design.md`
Dependency: plan 015's existing receipt infrastructure; provider sign-off is not required to start

## Executor instructions

Implement on `codex/018-failover-phase-evidence`. Read the complete recovery
scheduler, metrics registry/tests, Compose cluster qualifier, its CI projection,
and Cell failover/deployment docs. Add observation only; do not change election,
claim, recovery, or placement decisions in this plan. Use a unique external
Cargo target.

## Drift check

```bash
git fetch origin main
git diff --stat c86dd43423ae..origin/main -- \
  crates/crab-http-server/src/cells/scheduler.rs \
  crates/crab-http-server/src/metrics.rs \
  crates/crab-http-server/tests/qualify_compose_cluster.sh \
  .github/workflows/http-server-container.yml \
  crates/crab-cell-runtime/docs/failover-and-followers.md \
  crates/crab-cell-runtime/docs/deployment.md
```

Stop if another change has introduced canonical phase telemetry or changed the
cluster receipt schema. Extend that owner instead of adding duplicate metrics.

## Why this plan exists

`schedule_node_recovery` currently starts one timer around the complete job and
`Metrics::record_recovery_finished` records only
`crab_cell_node_log_recovery_seconds`. The Compose test polls 45-60 seconds for
expiry and restored data but records no recovery phase boundaries. Optimization
cannot be ranked or regression-gated from that evidence.

The deployment guide also says follower proof/recovery/placement are disabled,
while the implementation and version-6 Compose receipt exercise them. That
stale operator contract must be corrected before hardening claims are added.

Current timing ownership in `crates/crab-http-server/src/cells/scheduler.rs` is
one outer timer:

```rust
let started = std::time::Instant::now();
let result = recover_node_session(/* ... */).await;
metrics.record_recovery_finished(started.elapsed(), failure);
```

`crates/crab-http-server/src/metrics.rs` maps that to one unlabeled
`crab_cell_node_log_recovery_seconds` histogram.

## Files in scope

Only modify:

- `crates/crab-http-server/src/cells/scheduler.rs`
- `crates/crab-http-server/src/metrics.rs`
- `crates/crab-http-server/tests/qualify_compose_cluster.sh`
- `crates/crab-cell-runtime/src/bin/qualification_receipt.rs`
- `.github/workflows/http-server-container.yml`
- `crates/crab-cell-runtime/src/node_log_recovery.rs`
- `crates/crab-cell-runtime/docs/failover-and-followers.md`
- `crates/crab-cell-runtime/docs/deployment.md`
- focused existing test modules adjacent to those owners

Do not modify claim/lease formats, scheduling constants, routing, placement, or
recovery ordering. If another file is required, stop and amend this plan before
changing it.

## Scope

- Add fixed job phases: `claim`, `scope_validation`, `witness`, `pin_attach`,
  and `seal`. Their timers live only inside `recover_node_session`.
- Record candidate scan/assignment separately from recovery-job phases. Lease
  detection begins before a job exists and is not included in the job sum.
- Split the coordinator API just enough to time `pin_attach` separately; keep
  current claim -> whole-catalog validation -> witness -> pin/attach -> seal
  ordering unchanged.
- Record bytes, frames, affected Cells, catalog/control reads, peer calls, and
  object calls where the existing owner already knows them.
- Extend the Compose cluster receipt from version 5 to version 6 with monotonic
  `owner_killed_ms`, `advertisement_expired_ms`, `recovery_sealed_ms`, and
  `first_served_ms` values plus selection/work summaries for both loss cycles.
- Correct deployment and failover docs to describe implemented behavior and
  explicitly list remaining qualification gaps.

## Out of scope

- Changing lease duration, scan interval, recovery concurrency, or claim TTL.
- Adding node/session/Cell identifiers as metric labels.
- Follower-affine selection, local transport, lane indexes, or discovery changes.
- Claiming protected Kubernetes or production latency evidence.

## Implementation steps

### Step 1: define observable phase ownership

Add one typed internal `RecoveryPhase` enum at the server composition
boundary. Keep phase labels fixed and reviewable. Record counts/histograms
through the existing `Metrics` owner; structured traces may include session
IDs, but Prometheus labels may not.

**Verify:** `cargo test -p crab-http-server metrics --locked` reports all five
phase label cases and no unregistered/high-cardinality label.

### Step 2: instrument the unchanged recovery order

Split `recover_node_session` and `RecoveryCoordinator` only at phase boundaries
without changing ordering. Preserve the claim heartbeat around every long
phase. `pin_attach` owns manifest publication and all control attachments;
`seal` owns only the final dead-session seal. Candidate discovery/assignment
has its own aggregate counter and timer outside the job phase histogram.

**Verify:** `cargo test -p crab-http-server cells::scheduler --locked` proves
each injected failure emits one terminal job result and no later phase.

### Step 3: add bounded work summaries

Count recovery work at existing boundaries: candidate/Cell count, follower
pages/frames/bytes, control reads, bundle bytes, peer requests, and object
reads/writes. Thread the smallest summary type needed; do not create a second
telemetry subsystem in `crab-cell-runtime`.

Add metric registration/render tests asserting exact names, bounded label
values, success/failure accounting, and no raw IDs in metric text.

**Verify:** the metric render test asserts exact counter deltas for one success
and one failure and asserts that fixture IDs are absent from rendered text.

### Step 4: version the local cluster receipt

Update `qualify_compose_cluster.sh` to emit receipt version 6. Each loss cycle
must contain the four timestamps above, selected stable node/session facts,
`selected_original_follower`, terminal result, phase map, and work counters.
Update every workflow `jq` projection in the same commit. The end-to-end
`owner_killed -> first_served` interval is qualification evidence, not a server
phase and must not be synthesized by summing unrelated histograms.

Add `qualification_receipt validate-cluster <raw-receipt>` as the one canonical
typed validator for version 6. It rejects unknown/missing phases, non-monotonic
timestamps, duplicate phase keys, malformed selection/work counters, forbidden
identifier-bearing metric labels, non-success terminal state, and missing exact-
root assertions. The shell producer, container workflow, and later signed-
receipt wrapper must all call this validator; remove the duplicated semantic
inline `jq`, retaining `jq` only for display/projection.

**Verify:** run the existing isolated Compose/RustFS qualifier command documented
at the head of `qualify_compose_cluster.sh`, then run
`cargo run --locked -p crab-cell-runtime --bin qualification_receipt -- validate-cluster <receipt>`;
it exits zero. Every malformed fixture listed in the test plan exits nonzero.
Do not invent three tail profiles here; plan 019/020 add deterministic scale
tests and plan 015 owns protected scale/latency receipts.

### Step 5: correct operator documentation

Update `deployment.md` and `failover-and-followers.md` with current enabled
behavior, metric names, receipt version, and the remaining protected-fleet
gates. Do not advertise unimplemented follower preference.

**Verify:** `node crates/crab-cell-runtime/docs/validate.mjs` exits zero and
searching both documents finds no statement that follower recovery is disabled.

## Git workflow

Use two reviewable commits after rebasing on current `origin/main`:

1. `feat(cell): record failover recovery phases`
2. `test(cell): capture phase-aware failover receipts`

Keep the documentation corrections in the second commit with the executable
receipt contract. Rebase again before push; do not merge main.

## Verification

```bash
test -d "$HOME/Workspace/crabbuild-target" && \
  test -w "$HOME/Workspace/crabbuild-target"
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-018-failover-evidence \
  cargo test -p crab-http-server metrics --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-018-failover-evidence \
  cargo test -p crab-http-server cells::scheduler --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-018-failover-evidence \
  cargo test -p crab-cell-runtime --bin qualification_receipt --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-018-failover-evidence \
  cargo clippy -p crab-http-server --all-targets --locked -- -D warnings
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

Run the full Compose/RustFS qualifier in its dedicated environment and retain
the versioned raw receipt. Do not run it against a shared bucket/prefix.

## Acceptance criteria

- Every recovery job reports exactly one terminal result and non-overlapping
  `claim`, `scope_validation`, `witness`, `pin_attach`, and `seal` durations
  whose sum is bounded by total job duration.
- Metrics use only fixed phase/result/reason labels and contain no node,
  session, Cell, repository, tenant, or bucket identifiers.
- The receipt separates lease detection from recovery execution and first
  successful serving response.
- Both existing owner-loss cycles retain exact-root, follower-only commit, epoch,
  and session assertions after the schema bump.
- One version-6 local cluster receipt establishes the correctness/end-to-end
  baseline; deterministic scale proof belongs to plans 019/020 and protected
  latency/scale evidence remains plan 015's release gate.
- Deployment docs match the executable implementation and identify unsigned
  local evidence as non-release evidence.

## Test plan

- Unit: phase/result mapping, metric rendering, summary arithmetic, error paths.
- Scheduler: claim failure, inventory failure, witness failure, pin failure,
  fenced completion, and success each terminate telemetry once.
- Compose: two owner losses, exact-root progression, timestamps, work counters,
  and receipt validation.
- Negative: reject malformed receipt timestamps, missing phase data, duplicate
  phase labels, and identifiers in forbidden label positions.

## Done criteria

- [ ] Only the files named in **Files in scope** changed.
- [ ] Exact phase labels and the version-6 receipt shape are asserted in tests.
- [ ] Focused tests, Clippy, formatting, and doc validation pass.
- [ ] One clean local versioned receipt is retained externally and its artifact
      hash is recorded through plan 015's existing convention.
- [ ] `git diff --name-only c86dd43423ae...HEAD` contains no unplanned path.
- [ ] `advisor-plans/README.md` records measured bottlenecks and marks this plan
      DONE.

## Stop conditions

- Metrics require high-cardinality labels to answer the question.
- Phase instrumentation would change recovery ordering or extend claim lifetime.
- Receipt schema ownership conflicts with plan 015 or a newer mainline version.
- The external build/qualification volume is unavailable.

## Maintenance note

When recovery phases change, update metric tests, receipt projection, and both
failover docs in the same change. Remove obsolete phases rather than retaining
aliases.
