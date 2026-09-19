# Actor-owned resident Cell routing

Status: DONE — actor-owned resident-only lookup, zero-read route proof, bounded hit/miss/refusal metrics, and drain-before-release invalidation qualification pass local tests
Priority: P0
Effort: L
Risk: High
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependencies: plans 004 and 005

## Executor instructions

Implement on `codex/008-resident-cell-routing`. Read complete router, catalog,
authority, actor, peer, and server startup modules plus their tests. This change
must create one fast path inside `CellRuntime`; do not add a router-local cache
or trust stale metadata. Preserve all authorization and request admission that
occurs before Cell routing.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-http-server/src/cells/router.rs \
  crates/crab-cell-runtime/src/actor.rs \
  crates/crab-cell-runtime/src/catalog.rs \
  crates/crab-cell-runtime/src/authority.rs \
  crates/crab-cell-runtime/src/peer.rs
```

Rebuild the route call graph if any lookup/admission ordering changed. Stop if
server production code bypasses `CellRuntime` or the pure kernel is absent.

## Why this plan exists

`route_existing` currently loads catalog head/pages and exact control before it
asks the local actor for a handle. A warm resident request therefore performs
remote bucket operations even though the actor already owns the authoritative
local lifecycle and publisher state. This dominates latency and makes object
storage availability part of the warm read path.

## Current state and evidence

- `crates/crab-http-server/src/cells/router.rs:311` calls `route_existing`,
  takes an activation lock on miss, then repeats `route_existing`.
- `router.rs:356` calls `CellCatalog::lookup`, `CellAuthority::load`, then
  `CellRuntime::local_handle`.
- `crates/crab-cell-runtime/src/catalog.rs:295` lookup loads catalog metadata;
  `load_head` and `load_entries` perform object-store reads.
- `crates/crab-cell-runtime/src/actor.rs:397` `local_handle` requires caller-
  supplied `CatalogProof` and `VersionedControl`, so the runtime cannot answer a
  purely local query.
- The actor already receives fence, drain, migration, shutdown, renewal, and
  publication outcomes that determine whether a resident handle is safe.

## Required behavior

Add a narrow runtime lookup that takes stable Cell identity/target information
and returns one of:

- a safe local serving handle plus the current local routing proof;
- a typed local miss requiring the existing catalog/control path;
- a typed not-serving/fenced/quiescing outcome that cannot be mistaken for a
  safe local hit.

The exact public type may differ, but server code must not reconstruct or cache
the proof. The actor invalidates eligibility synchronously with the kernel
transition that fences, quiesces, migrates, releases, or shuts down the Cell.

## Scope

- Add actor-owned resident lookup and lifecycle eligibility.
- Try it in `route_target` before catalog/control reads.
- Preserve the existing cold/remote/activation path on a local miss.
- Instrument object-store operations in tests.
- Add hit/miss/fence/race and product-route tests.

## Out of scope

- Background hydration or a new resident tier.
- Idle eviction or placement.
- Skipping authentication, repository authorization, payload limits, or public
  request admission.
- Serving after lease/fence uncertainty.
- Changing catalog/control formats.

## Implementation steps

### 1. Define the local lookup contract

Extend the actor message/kernel input set with a read-only local-route query.
Eligibility must require the same Cell identity, serving lifecycle, non-fenced
authority, and compatible code/schema incarnation as normal `local_handle`.
Return an opaque handle/proof; do not expose mutable actor state.

### 2. Make invalidation atomic with lifecycle decisions

For renewal loss, conflicting control, drain, migration, deactivation, and
shutdown, the transition that changes lifecycle must make subsequent local
lookups miss/refuse before any external cleanup await. Tests must pause cleanup
and prove the lookup is already unavailable.

### 3. Reorder router work

After public authentication/authorization and request-level admission, call the
runtime local lookup. On hit, route immediately. On a true miss, execute the
existing catalog/control/peer/activation path unchanged. On a local unsafe
state, follow the kernel-defined outcome; do not silently treat a fence as a
generic cache miss if doing so can reactivate before release.

### 4. Add counted-store evidence

Wrap the existing storage test double with per-operation counters. For a warm
local route, reset counters after activation and assert zero catalog/control
`get`, `get_range`, `head`, and `list` operations through handle acquisition and
one representative SQL/KV read that does not require hydration.

Keep a cold-route test proving metadata reads still occur and return the same
result. Add races for lookup versus fence, drain, shutdown, and authority-loss
completion.

### 5. Preserve observability

Add bounded metrics for local hit, local miss, and unsafe-state refusal. Do not
label by Cell ID. Existing activation/route errors remain typed and source-
preserving.

## Verification

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-008-resident-route \
  cargo test -p crab-cell-runtime --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-008-resident-route \
  cargo test -p crab-http-server cells::router --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-008-resident-route \
  cargo test -p crab-http-server --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-008-resident-route \
  cargo clippy -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings

make -C crab architecture-check
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

Expected named proof includes: warm hit with zero metadata-store calls, cold
miss with normal lookup, fence-before-cleanup invalidation, drain invalidation,
and existing `route_reuses_restores_idle_and_takes_over_stale_owner` behavior.

## Acceptance criteria

- [x] A warm resident route performs zero catalog/control object-store
      operations before returning its local handle.
- [x] Cold, remote-owner, and uncatalogued routes preserve existing behavior.
- [x] Fence, quiesce, migration, release, and shutdown invalidate local routing
      before asynchronous cleanup.
- [x] No server-local metadata/handle cache or second routing policy is added.
- [x] Authentication, authorization, payload, and admission checks remain in
      their existing order relative to public input.
- [x] Metrics are bounded-cardinality and distinguish hit/miss/refusal.
- [x] All existing publication, takeover, and source-loss tests pass.
- [x] All verification commands pass.

## Stop conditions

- Safe local eligibility cannot be decided from actor-owned state.
- The fast path would bypass a security or tenant boundary.
- A lease/fence race can return a handle after invalidation.
- Implementing the change requires a router-local cache.

## Maintenance note

Any future lifecycle state that cannot serve must explicitly define local-route
eligibility in the coordination kernel. Defaulting a new state to “hit” is
unsafe.
