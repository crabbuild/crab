# Extract the pure Cell coordination kernel

Status: IN PROGRESS — pure lifecycle state, typed kernel-owned effect intents/IDs, actor admission/drain/scheduling seams, activation-generation fencing, and in-flight completion drain are wired and tested; scheduling plus hydration, renewal, persisted-work inventory, deactivation observations, migration admission, and simulator movement release now use kernel decisions, while complete decision extraction and parity coverage remain
Priority: P0
Effort: XL
Risk: High
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependency: plan 004's architecture guard and characterization baseline

## Executor instructions

Implement on `codex/005-pure-cell-coordination-kernel` after plan 004 lands.
Read root and `crates/` agent guidance, the full design authority, and the
complete `actor.rs`, `control.rs`, `authority.rs`, `publication.rs`, plus their
callers/tests. Preserve unrelated work. Keep the kernel crate-private. Do not
add a second runtime path or change persistent/wire formats.

Use a unique external Cargo target beneath `$HOME/Workspace/crabbuild-target`.
Stop if `$HOME/Workspace` is unavailable.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-cell-runtime/src/actor.rs \
  crates/crab-cell-runtime/src/control.rs \
  crates/crab-cell-runtime/src/authority.rs \
  crates/crab-cell-runtime/src/publication.rs \
  crates/crab-cell-runtime/tests
```

Rebuild the behavior map if any lifecycle, acknowledgement, or publication
branch changed. Stop if plan 004's architecture gate is absent.

## Why this plan exists

`CellRuntimeActor::run` currently interleaves deterministic protocol decisions
with Tokio channels, timers, object-store calls, SQL worker effects, and task
completion. That implementation has strong integration tests but cannot be
exhaustively scheduled. The canonical runtime needs one pure transition owner
that production, simulation, and the TLA+ abstraction all follow.

## Current state and evidence

- `crates/crab-cell-runtime/src/control.rs` already provides pure persisted
  authority transitions (`renew`, activate/takeover, recovery, publish).
- `src/actor.rs` owns volatile lifecycle and directly selects messages, timers,
  effect completions, renewal work, drains, migration, fencing, and shutdown.
- `src/publication.rs` owns the I/O protocol for preparing, publishing,
  reconciling, retaining, and releasing cuts.
- `src/authority.rs` owns exact conditional object-store transitions.
- `src/actor.rs`'s `ActiveCell` stores flags (`busy`, `renewing`, `fenced`,
  `drain`, `migrating`, `shutdown`) whose combinations form an implicit state
  machine.
- `CoordinationInput::Schedule` receives only adapter observations (queue
  presence, deactivation readiness, publication pressure, and lease liveness);
  the kernel decides whether to dispatch, fence, wait, or deactivate. The actor
  no longer repeats those policy predicates in `start_next`.

The kernel must decide what happens; adapters still perform I/O. Persisted
control transitions remain in `control.rs` and are invoked as pure callees.

## Required kernel contract

Create a crate-private coordination module with explicit, small types:

- `CoordinationState`: volatile lifecycle and in-flight operation identities;
- `CoordinationInput`: message admission, timer, effect completion, publication
  result, lease result, drain, fence, migration, and shutdown events;
- `CoordinationDecision`: new state plus ordered effects and caller outcomes;
- stable local operation/effect IDs so stale completions can be rejected;
- an effect enum that describes intent without holding bytes, object-store
  clients, Tokio handles, clocks, SQL connections, or errors erased to strings.

The exact names may change to fit local vocabulary, but these semantic
boundaries may not. One production adapter in `actor.rs` must call this kernel;
there must not be old and new decision paths selected by a flag.

## Scope

- Make volatile actor lifecycle states explicit.
- Move deterministic admission, sequencing, stale-completion, fencing,
  quiescing, renewal, publication-outcome, and shutdown decisions into the
  kernel.
- Keep execution of SQL, LTX, object-store, peer, timer, and filesystem effects
  in existing adapters.
- Add exhaustive transition-table unit tests and production parity tests.
- Preserve public APIs and all serialized formats.

## Out of scope

- Seeded randomized simulation and TLA+ files.
- Resident routing, hydration, eviction, placement, or balancing.
- Performance changes in `crab-ltx`.
- Standalone replication removal.
- New configuration or feature flags.

## Implementation steps

### 1. Write the state and event inventory before moving code

For every `Message` variant and every branch of `CellRuntimeActor::run`, record:

- precondition and current `ActiveCell` flags;
- pure decision;
- external effect;
- completion event;
- response-release condition;
- fence/cleanup behavior if the effect fails or arrives stale.

Place the maintained state table in the coordination module's rustdoc or a
nearby design table. Do not encode impossible flag combinations into a larger
boolean product; use enums for mutually exclusive lifecycle states.

### 2. Define the pure API and invariants

The step function must be synchronous and deterministic. Given identical state
and input it returns identical decisions. It may call pure `control.rs`
transitions but cannot read time or generate random IDs internally; the adapter
supplies observed time and IDs as inputs.

At minimum, assert after every step:

- at most one owned serving epoch;
- fenced/quiescing cells admit no new mutation;
- each accepted command has exactly one terminal caller outcome;
- acknowledgements require an exact published root or accepted follower proof;
- authority release is ordered after accepted work and publication obligations;
- stale effect completions cannot mutate current state;
- retained unpublished work cannot be silently discarded.

### 3. Extract one transition family at a time

Recommended order:

1. admission and caller cancellation;
2. effect dispatch/completion and stale IDs;
3. publication and ambiguous-CAS outcomes;
4. renewal/fence/takeover outcomes;
5. drain, migration, deactivation, and shutdown.

After each family, delete the corresponding decision branch from the actor.
The actor may translate an effect to async work, but may not re-decide it.
Commit each internally coherent family separately so reviewers can compare old
and new behavior.

### 4. Make effect completion explicit and cancel-safe

Every spawned operation must return a typed completion event carrying its
operation ID. Dropped callers do not cancel accepted durable work. Actor
shutdown must either drain the operation to its required durability boundary or
fence and retain the recovery obligation; no task may detach silently.

### 5. Add parity and transition proof

Add table tests for every legal state/input pair and representative illegal or
stale input. Keep the plan 004 characterization suite unchanged. Add a test
adapter that executes returned effects through existing fakes and compares the
same externally visible outcome as the pre-extraction fixtures.

Run a source check that the actor has exactly one call site for each moved
decision family and no duplicate boolean decision remains.

The scheduling seam has named pure transitions for publication backpressure and
lease loss. They prove that adapter observations cannot dispatch work after a
lease fence or while retained publication bytes are at the configured
high-water mark.

Background hydration and renewal use the same observation-only boundary:
foreground queue state, publication quiescence, and node-lease liveness are
inputs to the kernel, while the actor only reserves resources and executes a
`Started` effect. Fence decisions close admission before the adapter launches
the task. Persisted-work inventory refresh uses the same boundary and fences
on lease loss; the actor only records the returned inventory after matching the
generation and typed effect identity.

### 6. Update documentation

Update the design's state machine and ownership table with the implemented
types and source locations. Explain why `control.rs` owns persistent authority
transitions while the coordination kernel owns volatile scheduling decisions.

## Verification

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-005-coordination \
  cargo test -p crab-cell-runtime --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-005-coordination \
  cargo test -p crab-http-server --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-005-coordination \
  cargo test -p crab-ltx --features replica --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-005-coordination \
  cargo clippy -p crab-ltx -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings

make -C crab architecture-check
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
git diff --numstat 4a77b6f1252a -- crates/crab-cell-runtime/src
```

The final numstat must show that the new module replaces comparable actor
decision complexity. If production LOC grows materially, the PR description
must identify the removed state combinations or duplicate paths that justify it.

## Acceptance criteria

- [ ] Production actor decisions flow through one pure coordination step API.
- [ ] The kernel has no async runtime, I/O, filesystem, object-store, SQL,
      clock, or randomness dependency.
- [ ] Persistent `Control` transitions remain canonical in `control.rs`.
- [x] Every external effect has an explicit ID, completion event, and stale
      completion rule.
- [ ] Fence, drain, migration, shutdown, lost-CAS, caller-cancel, and follower
      acknowledgement contracts have named transition tests.
- [ ] Plan 004 characterization tests pass unchanged.
- [ ] Public APIs, stored keys, wire messages, and serialized shapes are
      unchanged.
- [ ] The old decision branches are deleted; no feature flag selects between
      implementations.
- [ ] All verification commands pass.

## Stop conditions

- Extraction requires changing acknowledgement or recovery semantics.
- A supposedly pure decision needs unknown provider behavior; read the locked
  dependency source/contract before proceeding.
- A public or serialized contract must change. Split that into a reviewed
  migration plan.
- The actor retains a second implementation merely to reduce migration risk.

## Maintenance note

New lifecycle decisions belong in the kernel first. New adapters may execute a
new effect, but must not infer policy from async completion ordering.
