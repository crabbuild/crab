# Hysteretic pressure shedding and paced Cell movement

Status: IN PROGRESS — hysteretic classifier, deterministic victim selector, shared actor eviction path, held movement permits, fail-closed Queue/Workflow persisted-work inventory, deterministic quiesce/durability/release/lost-reply/receiver-failure schedules, and local cross-process winner/crash/lost-response probes pass; failed receiver activation now rolls authority back to Idle; protected multi-process fault proof remains
Priority: P0
Effort: XL
Risk: High
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependency: plan 013's signed observations and pure planner

## Executor instructions

Implement on `codex/014-cell-pressure-drain`. Read the complete placement,
lifecycle, actor drain/release, peer membership, authority CAS, publication,
primitive scheduler, and shutdown paths. Treat movement as a protocol: planner
proposes, actor quiesces, durability closes, authority releases, receiver later
acquires normally. Do not add hot dual-writer migration.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-cell-runtime/src \
  crates/crab-http-server/src/server.rs \
  crates/crab-http-server/src/cells/router.rs
```

Stop if current drain/release ordering or follower acknowledgement semantics
changed without corresponding kernel/simulator proof.

## Why this plan exists

Weighted placement for new Cells does not correct an overloaded existing node.
Naive rebalancing can oscillate, drain too many Cells at once, amplify object-
store load, and lose accepted work. The runtime needs pressure states,
hysteresis, victim eligibility, rate budgets, and fail-safe handoff using its
existing authority protocol.

## Pressure state machine

Use explicit thresholds and dwell windows derived from the resource envelope:

```text
Normal -> Constrained -> Shedding -> Recovering -> Normal
```

Transitions require sustained evidence and separate enter/exit thresholds.
Stale observations cannot declare recovery. Thresholds should be fixed product
policy initially; do not add per-node environment variables in this plan.

## Safety rules

- Never move a Cell with retained unpublished work, in-flight migration,
  critical primitive lease, backup pin operation, or unknown accounting.
- Quiesce closes new admission before draining accepted work.
- Exact root/follower obligations finish according to the current contract.
- Release authority before another node acquires; planner intent is not proof.
- Receiver failure leaves the Cell safely idle/available for normal takeover,
  never dual serving.
- Rate limits bound simultaneous drains, activations, and bytes/jobs affected.

## Implementation steps

1. Add a pure pressure classifier over signed local observations with explicit
   enter/exit thresholds, minimum dwell, and stale-sample behavior. Test noisy
   traces, threshold equality, missing metrics, and time reversal rejection.
2. Extend the pure planner to select eligible shedding victims using resource
   relief, idle age, locality cost, primitive obligations, and stable tie-breaks.
   Current owner stickiness is overridden only in sustained shedding/drain.
3. Add a node-local movement budget: maximum concurrent quiesces, releases,
   receiver activations, and per-interval work. Use the coordination kernel's
   logical timer inputs; no detached sleeps.
4. Implement actor quiesce/drain/release through the canonical lifecycle. The
   action returns typed outcomes including deferred, completed, fenced, and
   retained-obligation. Retained-obligation is not counted as freed capacity.
5. After release, route/probe the preferred receiver and let it activate through
   normal control CAS. On receiver failure, replan after bounded backoff; do not
   restore the old owner unless it wins normal authority again.
6. Integrate operator/node drain with the same paced controller. Shutdown may
   have a deadline but cannot skip correctness; unresolved safe obligations are
   reported explicitly.
7. Include queue/workflow/activity pressure and lease deadlines in eligibility.
   Work may be allowed to expire/reclaim on another owner only where the
   primitive's durable schema already guarantees it.
8. Add deterministic simulator schedules for oscillating pressure, membership
   loss mid-drain, lost release response, receiver crash, stale observations,
   and simultaneous drain. Add a multi-process integration test with measured
   maximum movement concurrency.

## Verification

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-014-pressure \
  cargo test -p crab-cell-runtime coordination_sim --locked -- --nocapture
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-014-pressure \
  cargo test -p crab-cell-runtime --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-014-pressure \
  cargo test -p crab-http-server --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-014-pressure \
  cargo clippy -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

## Acceptance criteria

- [x] Pressure transitions use separate enter/exit thresholds, sustained dwell,
      and stale-observation rules; noisy inputs do not flap.
- [x] Victim selection is pure/deterministic and excludes every unsafe state.
- [x] Concurrent drains, releases, receiver activations, and movement rate are
      measurably bounded.
- [x] The local handoff path uses quiesce -> durability -> release -> normal
      acquire with no planner-owned or dual-writer authority; the
      `released_cell_is_acquired_by_one_successor_runtime` test transfers an
      idle control between two independent runtimes and checks the successor
      owner/active-cell ledger.
- [ ] Receiver failure, owner crash, lost release reply, and membership loss
      preserve single ownership and acknowledged state; the pure simulator now
      covers the lost-release-reply and receiver-crash ordering, and
      `failed_idle_receiver_does_not_leave_authority_owned` and
      `failed_takeover_receiver_does_not_leave_authority_owned` prove failed
      rooted receiver activation returns the exact root to unowned Idle. A
      pinned recovery overlay remains owned until replay can be sealed. A
      protected multi-process receipt is still required for the
      owner/membership path.
- [x] Operator drain and pressure shedding share one controller.
- [x] Queue/Workflow durable messages, leases, dedup identities, and runs
      participate in eviction eligibility; activity/effect jobs remain covered
      by the shared job reservation and coordination pending-effect set.
- [ ] Simulator and multi-process movement tests pass without retry masking.

The process-level race probe is available behind the test-only
`process-test-support` feature. It uses two OS processes and a shared local
filesystem CAS adapter, so the command verifies an actual cross-process
authority race rather than two runtimes in one address space:

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-014-process-movement \
  cargo test -p crab-cell-runtime --features process-test-support --test actor \
  independent_processes_allow_one_idle_cell_winner --locked -- --exact --nocapture
```

The companion `crashed_process_is_fenced_before_successor_restore` case exits
one acquired process without draining, fences that stale session, and restores
the exact root in a successor process. The
`lost_release_response_is_reconciled_before_successor_acquire` case commits
the release while dropping its response, verifies the publisher reconciles the
ambiguous result, and restores the unchanged root in a successor runtime.
The local `failed_idle_receiver_does_not_leave_authority_owned` case forces a
receiver activation failure after the idle takeover CAS and verifies the
canonical rollback leaves the exact root unowned and available for normal
acquisition. The companion
`failed_takeover_receiver_does_not_leave_authority_owned` case applies the same
rollback to a fenced-owner takeover. Recovery overlays are deliberately not
released by this cleanup because their follower proof must be replayed and
sealed before the Cell becomes idle.
These local proofs do not replace the protected three-Pod membership-loss or
directional-partition receipt required for release.

## Stop conditions

- Any victim can be selected while its resource/obligation state is unknown.
- A deadline would require dropping an accepted durability obligation.
- Receiver activation bypasses normal authority CAS.
- The controller needs unbounded queues or detached tasks.

## Maintenance note

Tune thresholds only with retained qualification receipts. A new movement cause
must use the same eligibility and pacing path.
