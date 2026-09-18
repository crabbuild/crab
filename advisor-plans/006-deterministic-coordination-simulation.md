# Deterministic adversarial coordination simulation

Status: IN PROGRESS — production-kernel replay, bounded exhaustive schedules, expanded fault vocabulary, the 512-seed corpus, four negative checks, and scheduled/manual trace artifacts pass; provider-level and liveness qualification remain
Priority: P0
Effort: L
Risk: Medium
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependency: plan 005's production-used pure coordination kernel

## Executor instructions

Implement on `codex/006-cell-coordination-simulation`. Read the kernel and all
its production adapters/tests before editing. The simulator must exercise the
same step function as production; a copied model is not acceptable. Keep it
sans I/O and reproducible. Use a unique external Cargo target and stop if the
workspace volume is unavailable.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-cell-runtime/src \
  crates/crab-cell-runtime/src/actor.rs \
  crates/crab-cell-runtime/tests \
  .github/workflows
```

Stop if production no longer uses the same pure kernel or if any kernel input
cannot be constructed without I/O.

## Why this plan exists

Async integration tests cover selected schedules. They do not systematically
explore lease expiry during publication, reordered completions, lost replies,
membership changes, simultaneous pressure, or shutdown races. A deterministic
simulator turns those schedules into replayable evidence and shrinks failures
to a seed plus an event trace.

## Required simulation model

The simulator owns only modeled external state:

- logical monotonic time;
- one or more coordination-kernel instances;
- authoritative control/catalog versions;
- queued, running, completed, duplicated, delayed, and dropped effects;
- modeled local roots, retained cuts, follower proofs, and caller outcomes;
- signed node sessions/membership inputs needed by current kernel behavior.

It must not start Tokio, sleep, open SQLite, use the network, or access an
object store. Use an explicitly stable PRNG/choice function whose algorithm is
owned by the test module so dependency upgrades cannot reinterpret seeds.

## Fault vocabulary

Support deterministic injection of at least:

- effect delay, reordering, duplication, loss before apply, and lost response
  after apply;
- owner crash/restart with selected local volatile state loss;
- lease expiry/renewal boundary and stale session completion;
- authority CAS conflict with exact successor or different winner;
- caller cancellation before and after admission;
- follower proof success/failure and delayed exact publication;
- drain/shutdown interleavings;
- clock advance without wall-clock sleeps.

## Implementation steps

### 1. Add a test-only simulator adapter

Place it next to the coordination kernel under `#[cfg(test)]`, or expose only
the minimum crate-private test support necessary for an integration test. Each
step records a compact trace entry containing seed, step number, selected event,
pre-state digest, decisions, and post-state digest. Never log payload bytes or
credentials.

### 2. Define executable invariants

Check after every simulated event:

- no two live owners serve the same Cell epoch/authority version;
- epoch and published sequence never regress;
- no command receives two terminal outcomes;
- no command is acknowledged without accepted durability proof;
- fenced/quiescing owners admit no new mutations;
- a retained unpublished root remains recoverable or explicitly fenced;
- an exact applied CAS may reconcile after response loss, but a different
  winner never does;
- shutdown/release cannot overtake accepted durable work.

The invariant failure must include the full replay command and minimized trace.

### 3. Build bounded exhaustive schedules

For small state (one Cell, two nodes, one or two commands), enumerate event
choices to a bounded depth instead of relying only on randomness. Deduplicate
equivalent states by a stable semantic digest that excludes trace order and
non-semantic IDs.

### 4. Add seeded broad exploration

Run a short fixed corpus in ordinary crate tests and a larger seed/range in a
scheduled CI job. Seeds are constants with a stated historical regression, not
an opaque expected-failure file. On failure, CI archives the seed and text/JSON
trace; it must not retry until green.

### 5. Prove the harness detects defects

Add test-only broken transition variants or faulted adapters for at least:

- acknowledging before durability;
- accepting after fence;
- adopting a different CAS winner;
- releasing authority before retained publication drains.

Each negative test must fail the corresponding invariant within a bounded
number of steps. Broken variants may not compile into non-test builds.

### 6. Document replay and triage

Document one exact command accepting a seed and optional maximum steps. Record
the stable PRNG algorithm/version and trace schema. Add the fast corpus to the
normal Cell runtime test entry point; add the broad run to a scheduled/manual
workflow with bounded time.

## Verification

Replay one bounded seed locally (the environment is intentionally test-only):

```bash
CRAB_COORDINATION_SEED=41 CRAB_COORDINATION_STEPS=256 \
  CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-cell-runtime coordination_sim::replay_requested_seed_from_environment \
  --locked -- --exact --nocapture
```

The fixed 512-seed corpus is `broad_seed_corpus_is_replayable`; scheduled/manual
CI archives its source revision, seed range, step bound, and test output.

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-006-simulation \
  cargo test -p crab-cell-runtime coordination_sim --locked -- --nocapture

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-006-simulation \
  cargo test -p crab-cell-runtime --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-006-simulation \
  cargo test -p crab-http-server --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-006-simulation \
  cargo clippy -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings

cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

Also run the documented replay command twice for the same seed and compare the
trace byte-for-byte. Run each broken-variant test and confirm it identifies its
intended invariant rather than timing out.

## Acceptance criteria

- [x] Simulation invokes the production-used pure coordination kernel.
- [x] Same seed/configuration produces a byte-identical semantic trace.
- [x] Bounded exhaustive exploration covers the small two-node state space.
- [x] The fast historical seed corpus runs in ordinary crate tests.
- [x] A scheduled/manual broad job archives source revision, seed, config, and
      failure trace without retry masking.
- [x] Every listed safety invariant is checked after every event.
- [x] Each deliberately broken variant is detected by the intended invariant.
- [x] No simulator dependency or broken mode appears in production artifacts.
- [x] All local verification commands pass.

## Stop conditions

- The simulator needs a copied protocol decision rather than the production
  kernel.
- A seed cannot be replayed deterministically.
- State deduplication drops a field that can change a future protocol decision.
- Broad CI cannot retain its failure trace.

## Maintenance note

Every fixed coordination race should add its minimized seed or bounded schedule
to the fast corpus. Remove a seed only when another named test subsumes the same
schedule and reviewer-visible evidence proves it.
