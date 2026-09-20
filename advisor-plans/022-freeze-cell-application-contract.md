# Plan 022: Freeze a supported Cell application contract

> **Executor instructions**: This plan creates the application-author boundary;
> it does not build the production node host. Keep runtime internals private and
> use the existing typed primitive APIs. Do not implement proc macros, dynamic
> code loading, or HTTP. Run every verification gate and update the index when
> complete.
>
> **Drift check (run first)**:
> `git diff --stat 892720ce6a6..HEAD -- Cargo.toml Cargo.lock crates/crab-cell-runtime crates/crab-http-server/src/cells.rs crates/crab-cell-runtime/docs/application-framework.md crates/crab-cell-runtime/docs/application-framework-example.md`

## Status

- **Priority**: P0
- **Effort**: L
- **Risk**: HIGH — stable IDs, descriptors, and capability boundaries become application contracts
- **Depends on**: plans 018–021
- **Category**: architecture / API / direction
- **Planned at**: commit `892720ce6a6`, 2026-09-19
- **Implementation status**: deterministic authoring boundary and handwritten full-primitive reference application implemented, including one successful typed invocation per primitive through a bounded local multi-Cell router plus executable scope, role, and capability rejection checks; generated clients, complete descriptor semantic validation, and protected release evidence remain open

## Why this matters

The runtime exposes powerful low-level pieces, but application owners must
currently assemble `RegistryBuilder`, module descriptors, namespaces,
migrations, operation IDs, primitive bindings, and clients manually. The
repository's proposed application framework names `CellApplication`, typed
capabilities, and a node facade, but those types do not exist in source. A
production-readiness claim needs a small supported author API whose compiled
descriptor is deterministic and whose limits are explicit.

This plan creates that author boundary in a new dependency-light
`crab-cell-app` crate. It deliberately keeps node lifecycle, provider
construction, HTTP/authentication, and fleet policy outside; plan 023 owns the
operator host.

## Current state

- `crates/crab-cell-runtime/src/registry.rs` defines `CellModule`,
  `RegistryBuilder`, module/namespace/operation descriptors, migrations, and
  typed command/query dispatch.
- `crates/crab-cell-runtime/src/{sql,kv,blob,queue,cron,workflow}` expose typed
  registration and namespace capabilities.
- `crates/crab-http-server/src/cells.rs:148-182` manually registers only the
  repository module in production.
- `crates/crab-cell-runtime/docs/application-framework.md:643-735` is an
  aspirational design, not source proof. Use it as a vocabulary proposal only;
  resolve conflicts in favor of current invariants and this plan.
- `crates/crab-cell-runtime/Cargo.toml` is unpublished and has no application
  facade feature.

## Supported contract to implement

The first supported version is statically linked Rust and must expose:

1. `CellApplication`: stable application identity and module registration.
2. `CellType`: stable namespace ID, role/topology, canonical partition encoder,
   shard count, schema/migration inventory, and declared resource limits.
3. `ApplicationBuilder`: validates unique IDs/names, operation descriptors,
   migrations, workflow/activity inventories, effect targets, shard topology,
   and limits, then produces the existing runtime `Registry` plus canonical
   application descriptor bytes/digest.
4. `ApplicationHandle<A>`: application/tenant-bound access to typed namespace
   clients without exposing authority, replicas, local paths, or arbitrary
   operation IDs.
5. One handwritten full-primitive reference application proving SQL, KV, Blob,
   Queue, Cron, Workflow, Activity, and Effects register together.

Explicit semantic boundaries:

- one command is atomic only within one Cell;
- custom SQL plus command-owned effect emission may commit atomically;
- built-in Workflow decisions atomically persist their generated effects;
- built-in primitives compose across Cells through typed commands/effects, not
  through multi-Cell transactions;
- external Activities are at-least-once and require destination idempotency;
- a hot Cell is an application partitioning problem, not transparently split;
- SQL/results, Blob pieces, batches, histories, and per-Cell databases remain bounded.

## Commands you will need

| Purpose | Command | Expected on success |
| --- | --- | --- |
| Workspace check | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-app cargo check -p crab-cell-app -p crab-cell-runtime --locked` | exit 0 |
| Author API tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-app cargo test -p crab-cell-app --locked` | exit 0 |
| Runtime consumers | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-app cargo test -p crab-cell-runtime --release --locked` | exit 0 |
| Minimal boundary | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-app cargo tree -p crab-cell-app --edges normal` | no server/provider-construction dependency |
| Architecture | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-app make -C crab architecture-check` | exit 0 |
| Quality | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-022-app cargo clippy -p crab-cell-app -p crab-cell-runtime --all-targets --locked -- -D warnings && cargo fmt --all -- --check` | exit 0 |
| Docs | `node crates/crab-cell-runtime/docs/validate.mjs` | exit 0 |

## Scope

**In scope**:

- New `crates/crab-cell-app/` crate with `AGENTS.md` and sibling `CLAUDE.md` symlink
- Root workspace membership and lockfile changes caused solely by the new crate
- Narrow runtime API changes required for descriptor construction and typed handles
- Full-primitive reference application tests in the new crate
- Application framework docs updated to exactly match implemented source

**Out of scope**:

- `CellNode`, storage/provider construction, routing, scheduler processes, or shutdown
- HTTP/RPC endpoints, authentication, tenant authorization, or UI
- Proc macros, code generation, uploaded code, WASM, JavaScript, or dynamic libraries
- General SQL database, global transactions, exactly-once Activities, or live resharding
- Publishing crates.io packages or declaring semantic-version stability
- Aliases for aspirational names that are not implemented

## Git workflow

- Branch: `codex/022-cell-application-contract`
- Commits: one for crate/API, one for reference application/tests, one for docs
- Example: `feat(cell-app): add deterministic application descriptor`
- Do not push/open a PR unless instructed.

## Steps

### Step 1: Inventory persistent identifiers and limits

Build a source-backed table of every current primitive's stable namespace role,
operation IDs, codec versions, migration digests, input/output limits, shard
routing rule, and effect target rules. Use implementations and registration
validators, not prose docs. Add this inventory as test fixtures in
`crab-cell-app`; changing bytes must fail a named test.

Record which fields are persistent and which are diagnostics. Names must not
participate in Cell identity when stable IDs already exist.

**Verify**: fixture test reconstructs a current manual `Registry` and matches
its module/release digest.

### Step 2: Add the dependency-light author crate

Create `crates/crab-cell-app` with no dependencies on HTTP server, provider
configuration, TLS, CLI, or UI. Reuse `crab-cell-runtime` public contracts; do
not duplicate `Command`, `Query`, `WireValue`, descriptors, or primitive clients.

Add the scoped agent guide and symlink required by repository policy. Avoid new
external dependencies unless explicit approval is obtained.

**Verify**: workspace check and dependency-tree commands pass.

### Step 3: Implement deterministic application compilation

Implement `CellApplication`, `CellType`, and `ApplicationBuilder`. Builder
registration order must not alter canonical descriptor bytes/digest. Validate:

- unique application/module/namespace/operation identities;
- nonzero fixed shard counts and bounded canonical partitions;
- ordered contiguous migration versions/digests;
- primitive role matches its typed registration;
- every effect target exists in the same application descriptor;
- every Workflow definition and Activity type is retained as required;
- all declared limits are nonzero and within runtime maxima.

Return the existing `Registry`; do not create another dispatcher.

**Verify**: permutation/property tests produce identical bytes, while duplicate
IDs, missing targets, role mismatches, and changed migration bytes fail closed.

### Step 4: Add capability-safe application handles

Implement `ApplicationHandle<A>` around an already-started `CellClient`, tenant,
application identity, and compiled topology. Typed accessors may construct the
existing `KvNamespace`, `BlobNamespace`, `QueueNamespace`, `CronNamespace`, and
`WorkflowNamespace`. Do not expose raw authority, runtime, replicas, paths,
credentials, arbitrary operation IDs, or an unbounded SQLite connection.

Provisioning is not owned here. If a target has not been provisioned, return
the existing explicit unavailable/not-started result.

**Verify**: positive typed-client tests compile; API surface review and rustdoc
show no internal runtime escape hatch.

### Step 5: Build a full-primitive reference application

Create a small handwritten application fixture with stable IDs and one module
per primitive role. It must compile one canonical descriptor and expose typed
clients for all primitives. Include a deterministic Workflow definition, one
Activity type, one Effect destination, and a Queue dead-letter target.

The test may use the current in-process runtime fixture; node/fleet lifecycle is
deferred to plan 023. Exercise at least one successful typed invocation per
primitive plus one invalid registration for each cross-capability relationship.

**Verify**: author API tests pass without importing server modules.

### Step 6: Reconcile docs to code

Rewrite the application framework status and examples so every non-compiling
snippet is marked proposed and every supported snippet is compiled/tested.
Publish the semantic exclusions and exact current limits. Remove names and
claims not delivered by this plan; do not retain future aliases.

**Verify**: docs validator passes and all unimplemented examples remain
explicitly fenced as `ignore`/proposed.

### Step 7: Run affected broad gates

Run full runtime tests, author crate tests, architecture check, Clippy, format,
doc validation, and `git diff --check`. Review `Cargo.lock` and confirm no
unapproved dependency was added.

## Done criteria

- [x] An application compiles to deterministic registry and topology bytes independent of registration order.
- [x] Stable IDs, migrations, operation limits, effect targets, definitions, and activities fail closed on mismatch.
- [x] Typed handles expose every primitive without exposing runtime internals.
- [x] The reference application registers and invokes all primitives through public author APIs.
- [x] Semantic exclusions and bounded limits are executable tests, not prose-only claims.
- [x] No node/provider/server policy moved into low-level crates.
- [x] Workspace, runtime, architecture, Clippy, format, and docs gates pass.

## STOP conditions

- A stable application descriptor cannot be produced without inventing a second registry.
- Supporting a primitive requires exposing raw transactions or arbitrary operation IDs.
- A new external dependency, proc macro, or serialization format becomes necessary without approval.
- Plans 018–021 have not stabilized the semantics this contract would freeze.

## Maintenance notes

This is the application-author contract. Plan 023 owns lifecycle and operator
composition. Future code generation may target this API only after handwritten
fixtures prove the descriptor and typed clients; it must never become a second
source of stable IDs or migration ordering.
