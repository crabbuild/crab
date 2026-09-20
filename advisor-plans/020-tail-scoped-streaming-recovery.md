# Tail-scoped affected-Cell recovery and bounded streaming

Status: PROPOSED
Priority: P0
Effort: XL
Risk: High
Planned against: `c86dd43423ae` (`origin/main`, 2026-09-20)
Design authority: `advisor-plans/follower-affine-failover-design.md`
Dependencies: plans 018 and 019; plan 010's verified multipart-source slice

## Executor instructions

Implement on `codex/020-tail-scoped-recovery`. Read the complete recovery
coordinator, node-frame verifier, recovery-overlay builder, manifest pin/load,
catalog/control APIs, streaming publication plan 010, and actor recovery tests.
Change discovery order without weakening witness, catalog, control, or pinning
validation. Use a unique external Cargo target.

## Drift check

```bash
git fetch origin main
git diff --stat c86dd43423ae..origin/main -- \
  crates/crab-cell-runtime/src/node_log_recovery.rs \
  crates/crab-cell-runtime/src/node_log.rs \
  crates/crab-cell-runtime/src/recovery_manifest.rs \
  crates/crab-cell-runtime/src/catalog.rs \
  crates/crab-cell-runtime/src/authority.rs \
  crates/crab-http-server/src/cells/scheduler.rs \
  crates/crab-ltx/src/bundle.rs \
  crates/crab-ltx/src/replica.rs \
  crates/crab-ltx/src/node_frame.rs \
  crates/crab-cell-runtime/tests/actor.rs
```

Stop if node-frame scope, catalog lookup, control authority, recovery overlay,
or streaming publication ownership changed. Before starting, prove plan 010's
file-backed `Bundle` and retrying multipart source/promotion path are present.
If recovery cannot call that one owner without duplicating it, complete the
narrow API extraction first and record it as part of this plan.

## Why this plan exists

`recover_node_session` calls `recoverable_cells` before sealing the log. That
function scans all 256 application catalog shards and loads control for every
entry to find Cells owned by the failed session. Only afterward does recovery
read frames whose authenticated `NodeFrameScope` already carries application,
Cell, incarnation, Cell epoch, and commit sequence.

`ensure_sealed` also retains one complete witness as `Vec<VerifiedNodeFrame>`,
then overlay construction and `RecoveryManifestStore::pin` materialize complete
bundles. The path is bounded by hard limits but does unnecessary work and memory
for large catalogs and tails.

Current server ordering is:

```rust
let cells = recoverable_cells(&catalog, &authority, session, limit).await?;
let recovery = NodeLogRecovery::from_fenced_with_disk(/* ... */)?;
coordinator.recover(recovery_fence, cells).await?;
```

`recoverable_cells` scans shards `0..=255` before `ensure_sealed` returns the
authenticated frames that identify the actual affected Cells.

## Target ordering

```text
claim dead session
  -> seal every reachable follower and compare receipt/evidence
  -> stream one complete witness through node-frame verification
  -> collect bounded unique authenticated Cell scopes for this application
  -> group scopes by catalog shard; load each affected shard once
  -> exact entry match + current control load for only those scopes
  -> reject any scope not owned by dead session/exact incarnation+epoch
  -> stream frames into per-Cell recovery builders
  -> publish immutable bundles, then one manifest
  -> attach every pinned overlay, then seal recovery authority
```

Cells with no uncovered follower frames need no recovery overlay. Object-covered
state remains reachable from their existing roots. The global failed-session
log is not sealed until every uncovered frame has either contributed to a
pinned overlay or caused recovery to fail.

## Scope

- Derive a bounded, sorted unique set of affected scopes from verified frames.
- Replace the 256-shard discovery scan with one shared load of each affected
  shard, then exact catalog-entry/control validation for those scopes. Current
  catalog heads have no key ranges, so do not claim one object lookup per Cell.
- Stream one selected complete witness through overlay builders and immutable
  publication using plan 010's bounded verifier/uploader patterns.
- Preserve conflict comparison across all reachable witnesses.
- Preserve idempotent retry after partial immutable publication/control attach.
- Delete `recoverable_cells` whole-catalog use from the production path.

## Out of scope

- Skipping catalog/control authorization because a frame names a Cell.
- Sealing from only the local follower without checking other reachable members.
- Changing node-frame or recovery manifest formats.
- Follower selection/placement or same-host transport.
- Garbage collection of orphan immutable bundles; existing content addressing
  and retention policy remain responsible.

## Files in scope

Only modify the drift-check paths above and their adjacent focused tests. The
catalog persistent format, node-frame/wire format, recovery manifest format,
and placement/claim logic are read-only. If a new catalog index or serialized
field appears necessary, stop: that is a separate migration design.

## Implementation steps

### Step 1: select and compare a complete witness with bounded state

Collect every reachable seal receipt and compute the maximum durable watermark.
Sort complete candidates deterministically. Scan the first candidate whose
retained base covers the required first sequence and whose durable watermark is
the maximum. While verifying it, write `(sequence implied by offset, digest)` to
a fixed-width scratch file and collect unique scopes; reserve that file through
the existing recovery disk budget. Then scan every other reachable witness,
including partial witnesses, and compare every overlapping verified digest by
seeking the scratch table. If a selected candidate is corrupt/incomplete, try
the next complete candidate; any verified conflict fails closed.

**Verify:** `cargo test -p crab-cell-runtime node_log_recovery --locked` covers
partial overlap, divergent overlap, corrupt first candidate with valid second,
no complete candidate, and scratch reservation/cleanup.

### Step 2: validate only affected catalog shards and controls

Require every frame scope's application to equal `CellCatalog::application()`;
cross-application frames fail. Enforce the existing Cell limit before growing
the scope set. Group the sorted scopes by catalog shard, load each touched shard
once with its proof, and require one exact matching entry per scope. Then load
only those controls and require failed-session owner, incarnation, Cell epoch,
and predecessor root equality. A mismatch is a typed recovery failure, never a
skipped frame.

**Verify:** a large generated catalog with K affected shards observes exactly K
shard loads and one control load per affected Cell; unknown/wrong-application,
stale generation, wrong owner, and missing-root cases all fail.

### Step 3: add one file-backed bundle construction/publication contract

In `crab-ltx/src/bundle.rs`, add the smallest builder that verifies each LTX
entry while writing payloads/footer to a budgeted temporary file, then returns
the existing file-backed `Bundle`. Expose the plan-010 verified multipart source
and staged content-addressed promotion through one narrow callable API; factor
the current `CellReplica::put_bundle` implementation into that owner. It must
verify expected digest/length, clean staging on every error/cancellation, and
reconcile exact existing immutable objects. Do not expose a caller-trusted path
or duplicate multipart policy in `crab-cell-runtime`.

**Verify:** `cargo test -p crab-ltx bundle --locked` proves memory/file parity,
provider retry, cancellation cleanup, divergent existing-object rejection, and
exact existing-object acceptance.

### Step 4: reread once into bounded per-Cell builders

Reread the selected sealed witness exactly once. Reverify every frame and its
digest against the pass-one scratch table, then append its LTX body to the
matching file-backed per-Cell builder. Bound open files with a fixed internal
pool (32 maximum; reopen-on-append beyond it). Preserve transaction continuity,
checksums, commit sequence, predecessor, final position, and duplicate/conflict
validation. Scope metadata remains bounded by the existing Cell limit; payload
and digest tables remain disk-backed.

**Verify:** a generated tail exceeding the open-file pool produces exact roots,
never exceeds 32 builder descriptors, and reports peak heap independent of tail
payload size within a fixed tolerance.

### Step 5: publish manifest last and preserve retry semantics

Make `RecoveryManifestStore::pin` publish file-backed bundles through the one
shared uploader, never `read_all`. Publish each bundle sequentially, then the
bounded manifest, attach every control, and only then seal the failed session.
On retry, accept exact existing immutable objects/attachments and reject
divergence.

**Verify:** the existing crash matrix after bundle, manifest, each attach, and
before/after seal converges to one exact result without leaked scratch/staging.

### Step 6: remove production whole-catalog discovery

Move the old whole-catalog recovery algorithm into a private `#[cfg(test)]`
reference oracle before removing its production caller/export. Use it only for
generated equivalence tests; it must not remain selectable in production.
Update plan 018 work counters for verified frames/bytes, affected shards/Cells,
control reads, bundle bytes, and peak scratch bytes.

**Verify:** `rg "recoverable_cells" crates/crab-http-server crates/crab-cell-runtime/src`
shows no production call/export, while the generated oracle comparison passes.

### Step 7: run the integration proof

Preserve manifest-last publication and attach-all-before-session-seal order.
On retry, accept exact existing immutable objects and exact already-attached
controls only; reject divergent state.

**Verify:** scheduler and process actor tests pass with the new ordering, and a
failure before final seal leaves the claim retryable under existing semantics.

## Git workflow

Use three reviewable commits after rebasing on current `origin/main`:

1. `refactor(cell): derive recovery scopes from verified tails`
2. `perf(cell): stream bounded recovery overlays`
3. `test(cell): prove tail-scoped recovery equivalence`

Commit one keeps the existing materialization path while changing discovery;
commit two installs the single bounded publisher and deletes materialization.
Commit three adds scale/crash qualification. Rebase before push.

## Verification

```bash
test -d "$HOME/Workspace/crabbuild-target" && \
  test -w "$HOME/Workspace/crabbuild-target"
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail-recovery \
  cargo test -p crab-ltx node_frame --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail-recovery \
  cargo test -p crab-cell-runtime node_log_recovery --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail-recovery \
  cargo test -p crab-cell-runtime --test actor --features process-test-support --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail-recovery \
  cargo test -p crab-http-server cells::scheduler --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-020-tail-recovery \
  cargo clippy -p crab-ltx -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
cargo fmt --all -- --check
git diff --check
```

## Acceptance criteria

- Production recovery loads only unique affected catalog shards, once each,
  and controls proportional to unique affected Cell scopes; it does not scan
  all 256 shards or reread one shard per Cell.
- Every selected frame is verified during comparison and exactly once again
  during bundle construction; no frame is silently dropped or attached to a
  mismatched generation.
  no frame is silently dropped or attached to a mismatched Cell generation.
- Reachable witnesses with conflicting frame bytes still fail recovery.
- Peak heap is bounded by one page, the affected-scope cap, and fixed builder
  buffers; witness digests and bundle payloads use disk-budgeted scratch.
  Scratch remains bounded by the existing maximum recoverable tail/bundle size.
- Manifest-last publication, control attachment, and session seal ordering are
  unchanged and crash-idempotent.
- Exact roots after takeover match the whole-catalog reference implementation
  across generated multi-Cell schedules.

## Test plan

- Equivalence property: old reference inventory/recovery versus tail-scoped
  recovery for generated catalogs, roots, and frame interleavings.
- Scope failures: unknown app/Cell, stale incarnation, stale Cell epoch, wrong
  owner, missing root, duplicate equal frame, conflicting frame.
- Witness failures: one unreachable member, partial witness, divergent members,
  gaps, corrupt node frame, active log without complete witness.
- Crash matrix: after bundle PUT, manifest PUT, first/middle/final control attach,
  and before/after session seal; retry reaches one exact result.
- Scale: large catalog with few affected Cells and large multi-Cell tail with
  bounded memory/scratch observations.

## Done criteria

- [ ] Only **Files in scope** changed.
- [ ] Whole-catalog discovery is absent from production and exists only as the
      private test oracle.
- [ ] One file-backed builder and one multipart publisher own bundle creation
      and immutable publication.
- [ ] Focused tests, Clippy, formatting, and isolated Compose qualification pass.
- [ ] Complexity tests prove affected-shard/control counts and bounded heap/FDs.
- [ ] `git diff --name-only c86dd43423ae...HEAD` contains no unplanned path.

## Stop conditions

- Exact catalog lookup cannot provide the same authorization proof as shard scan.
- Streaming would change immutable bundle digest/layout without a reviewed
  migration/compatibility decision.
- A frame can be acknowledged without enough authenticated scope to find its
  exact Cell control.
- The external Cargo target volume is unavailable.

## Maintenance note

Any new node-frame scope or recovery attachment must update the scope collector,
exact-control validation, property oracle, and qualification counters together.
