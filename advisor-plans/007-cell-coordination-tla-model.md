# Small-state TLA+ model for Cell coordination

Status: IN PROGRESS — SHA-256-pinned TLC safety and fair stable-provider liveness checks, CI path filters, simulator artifacts, and the complete code/model action ledger pass; provider qualification remains
Priority: P1
Effort: L
Risk: Medium
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependency: plan 005's production-used pure coordination kernel

## Executor instructions

Implement on `codex/007-cell-coordination-tla`. This is a specification and CI
plan, not a production Rust refactor. Read the kernel state/input/effect types,
their full tests, the design safety section, and existing workflow pinning
conventions. Do not vendor a tool binary or JAR without explicit approval.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-cell-runtime/src \
  crates/crab-cell-runtime/docs \
  .github/workflows
```

If the kernel state machine changed, update the model-to-code delta ledger
before running TLC. Stop if the pure kernel is not the production decision
owner.

## Why this plan exists

Simulation explores many executable schedules, but it cannot claim exhaustive
coverage. A deliberately small formal model can exhaust ownership, lease,
publication, acknowledgement, crash, and recovery interleavings and make each
abstraction difference from Rust reviewable.

## Scope

- Add a model directory under `crates/crab-cell-runtime/model/`.
- Model one Cell, bounded nodes/commands/roots, lease time, authority versions,
  retained publication, follower proof, crash/restart, drain, and shutdown.
- State safety and bounded liveness properties.
- Maintain a code-to-model delta ledger.
- Add positive and intentionally broken TLC configurations.
- Provide one checksum-verifying runner used locally and in CI.

## Out of scope

- Modeling SQLite page bytes, object-store provider internals, cryptography,
  primitive SQL schemas, placement optimization, or unbounded liveness.
- Claiming the TLA+ model proves the Rust adapter or external providers.
- Adding runtime dependencies.

## Model contract

Model these invariants:

- single serving owner for a Cell at a time;
- monotonic epoch and published sequence;
- no acknowledgement without exact root or valid follower proof;
- different CAS winner fences instead of reconciling;
- no admission after fence/quiesce;
- accepted work is published/retained before release;
- retained work is recoverable after crash;
- under explicit fairness and eventual provider response, drain/recovery can
  complete.

Every omitted Rust field or collapsed transition must appear in a table with:
Rust source/type, model variable/action, abstraction, and why the omission
cannot invalidate the checked property.

## Implementation steps

### 1. Pin the TLC toolchain

Select an official TLC release available at implementation time. Record its
exact version, official URL, SHA-256, and license in a small toolchain file.
The runner downloads only to an external cache, verifies the checksum before
execution, and fails closed on mismatch. CI actions and container images remain
pinned by full commit/digest following repository convention.

### 2. Write the smallest useful state machine

Use bounded constants for two nodes, two commands, a few roots, and discrete
time. Keep actions aligned with kernel inputs/effects: admit, complete effect,
prepare/publish, reconcile, renew/expire, follower proof, fence, crash/restart,
drain/release. Avoid helper actions that combine production-visible steps and
hide an interleaving.

### 3. Add positive configurations

Provide a fast configuration for normal PR validation and a broader scheduled
configuration. Pin worker count, constants, symmetry sets, and state constraints
so runs are reproducible. The runner emits TLC version, config, source revision,
states generated/distinct, depth, elapsed time, and verdict.

### 4. Add negative configurations

Create separate broken modules/configurations for at least early acknowledge,
dual serving ownership, adopt-different-winner, and early release. A wrapper
test expects TLC to produce the named counterexample/property; a generic TLC
failure or timeout does not count.

### 5. Integrate CI and artifacts

Run the fast positive and negative matrix when model, coordination kernel, or
runner paths change. Run the broader matrix on schedule/manual dispatch. Upload
counterexamples and the execution summary on failure. Do not retry a failed
model check automatically.

### 6. Cross-reference executable simulation

For each modeled action, link the corresponding coordination input/effect and
at least one deterministic simulator case. Document what remains simulation-
only (adapter errors, richer membership, resource pressure) and model-only
(exhaustive bounded state enumeration).

## Verification

The implementation must expose one command, for example:

```bash
crates/crab-cell-runtime/model/check.sh fast
crates/crab-cell-runtime/model/check.sh negative
crates/crab-cell-runtime/model/check.sh broad
```

Also run:

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-007-tla \
  cargo test -p crab-cell-runtime coordination --locked

node crates/crab-cell-runtime/docs/validate.mjs
cargo fmt --all -- --check
make -C crab architecture-check
git diff --check
```

Expected: fast and broad positive configurations finish with no invariant
violation; every negative configuration emits the expected named violation;
checksum tampering causes the runner to fail before Java starts.

## Acceptance criteria

- [x] The model checks every invariant listed above over its bounded state,
      including the fair stable-provider drain/recovery liveness property.
- [x] The delta ledger maps every kernel state/input/effect or explains its
      deliberate abstraction.
- [x] One official TLC artifact is pinned and checksum verified; none is
      committed to the repository without approval.
- [x] Fast and broad configurations are deterministic and report state counts.
- [x] Four broken variants produce their expected counterexamples.
- [x] CI path filters include both the model and coordination-kernel sources.
- [x] Failure artifacts contain enough information to rerun locally.
- [x] Documentation clearly limits the formal claim to the modeled protocol.

## Stop conditions

- Tool acquisition cannot be pinned and checksum verified.
- A model action cannot be mapped to a production kernel transition.
- State-space reduction requires assuming away the failure being proved.
- CI cannot retain counterexamples.

## Maintenance note

Any coordination-kernel change must update the delta ledger and, when relevant,
the model in the same PR. A passing stale model is not protocol evidence.
