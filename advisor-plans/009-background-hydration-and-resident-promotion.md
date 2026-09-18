# Bounded background hydration and resident promotion

Status: IN PROGRESS — bounded worker hydration, sparse/resident state, worker-job ledger admission, counted-store zero-I/O proof, and a restored sparse warm-restart promotion probe are wired; cancellation and provider-scale qualification remain
Priority: P0
Effort: L
Risk: High
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependency: plan 008's actor-owned local routing

## Executor instructions

Implement on `codex/009-cell-background-hydration`. Read `managed.rs`, sparse
VFS/paging code, actor/worker lifecycle, admission budgets, SQL worker tests,
and the complete resident-routing implementation. The runtime must schedule
existing verified hydration mechanics; do not create another page-download or
SQLite-open path. Use a unique external Cargo target.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-ltx/src/managed.rs \
  crates/crab-ltx/src/paged* \
  crates/crab-cell-runtime/src/actor.rs \
  crates/crab-cell-runtime/src/worker.rs \
  crates/crab-cell-runtime/tests
```

Stop if hydration has gained a canonical scheduler elsewhere or if the local
route bypasses the actor.

## Why this plan exists

`ManagedDb::hydrate_step` already verifies and materializes sparse content, but
its contract deliberately leaves scheduling/cancellation to the caller. No
runtime caller promotes a restored sparse Cell to a fully resident local
database. A “warm” SQL request can therefore still issue object-store ranges.

## Target lifecycle

Represent local data residency independently from serving authority:

```text
Sparse -> Hydrating -> Resident
   ^          |           |
   +----------+-----------+  (eviction/reopen/source loss)
```

Serving may continue while sparse/hydrating. Only verified completion promotes
to resident. Fence/drain/shutdown cancels further scheduling and safely joins or
abandons the in-flight step according to the existing scratch-file contract.

## Scope

- Add explicit actor-owned residency state and progress.
- Execute bounded `hydrate_step` work on existing fixed workers or a separately
  admitted bounded worker class.
- Schedule only when foreground work and pressure policy permit.
- Account hydration memory, disk reservation, and worker/job usage.
- Prove resident SQL reads issue zero object-store operations.

## Out of scope

- New hydration data format or downloader.
- Eviction policy, fleet placement, or pressure shedding.
- A user-facing environment/config knob.
- Assuming sparse equals resident because recently accessed pages are cached.

## Implementation steps

1. Add `Sparse`, `Hydrating`, and `Resident` to actor-owned local state. Restore
   and sparse open begin `Sparse`; a full local restore may begin `Resident`
   only when the existing verifier proves all required content is installed.
2. Add a typed background hydration effect to the pure kernel. Its inputs
   include current generation/root, a fixed internal work bound, and operation
   ID. Completion reports progress, complete, cancelled, or typed failure.
3. Execute one bounded `ManagedDb::hydrate_step` per admitted background job.
   Do not hold actor locks or a Tokio mutex across blocking SQLite/filesystem
   work. Foreground commands remain preferred.
4. Before each step, reserve its declared memory/disk/job cost. Release every
   reservation on success, failure, cancellation, fence, and shutdown. A lack
   of budget defers hydration; it does not fail foreground traffic.
5. Promote to `Resident` only after the LTX layer confirms completion for the
   same root/generation. Ignore stale completions after root replacement.
6. Add tests for progress, stale completion, corrupt range, cancellation,
   restart, foreground starvation resistance, and reservation cleanup.
7. Extend the counted-store route test: activate sparsely, allow hydration to
   complete, reset counters, then perform representative SQL and KV reads and
   assert zero object-store calls.

## Verification

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-009-hydration \
  cargo test -p crab-ltx --features replica --test cell_roots --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-009-hydration \
  cargo test -p crab-cell-runtime --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-009-hydration \
  cargo test -p crab-http-server --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-009-hydration \
  cargo clippy -p crab-ltx -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

## Acceptance criteria

- [x] The runtime, not the HTTP router, owns hydration state and scheduling.
- [x] Hydration uses `ManagedDb::hydrate_step`; no second page-fetch path exists.
- [x] Work per step and concurrent hydration jobs are statically bounded and
      represented in runtime resource accounting.
- [x] Foreground work cannot be starved by hydration.
- [x] Fence, shutdown, and root change cleanly cancel or stale-reject work and
      release all reservations.
- [x] Corrupt or incomplete content never promotes to resident.
- [x] After verified promotion, local SQL/KV reads perform zero object-store
      operations in the counted-store test.
- [x] Existing sparse read, truncate/regrow, checksum, and source-loss tests
      pass unchanged.

The restored sparse warm-restart probe is
`restored_sparse_route_promotes_before_zero_origin_reads` in
`crates/crab-cell-runtime/tests/actor.rs`. It publishes and drains a Cell,
acquires the exact idle root through a new runtime, waits for verified
hydration to promote the local route, then performs a SQL read after resetting
the counted-store observer. The read performs zero origin calls. This is local
restart evidence; provider-scale and protected release receipts remain open.

## Stop conditions

- The existing hydration API cannot provide bounded progress or cancellation
  without a persistent-format change.
- Hydration requires unbounded scratch capacity.
- Foreground correctness would depend on background completion.
- Resource reservations cannot be released on every exit path.

## Maintenance note

Residency is an optimization, never authority. Future read optimizations may
change scheduling, but only verified LTX state can declare `Resident`.
