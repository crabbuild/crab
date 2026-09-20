# Plan 023: Put production Cell lifecycle behind one node host

> **Executor instructions**: Introduce the facade alongside the current server
> composition, switch the server, then delete the old path in the same delivery
> sequence. Never leave two production schedulers, authorities, publishers, or
> durability owners. Run every gate and stop if ownership cannot be made unique.
>
> **Drift check (run first)**:
> `git diff --stat 892720ce6a6..HEAD -- crates/crab-http-server/src/server.rs crates/crab-http-server/src/cells.rs crates/crab-http-server/src/cells crates/crab-cell-runtime/src crates/crab-ltx/src Cargo.toml Cargo.lock`

## Status

- **Priority**: P0
- **Effort**: XL
- **Risk**: HIGH — this refactors the production composition and shutdown boundary
- **Depends on**: plan 022
- **Category**: architecture / lifecycle / DX
- **Planned at**: commit `892720ce6a6`, 2026-09-19
- **Implementation status**: provider-neutral host owns serving and offline-maintenance runtime construction, exposes canonical `start`/`status` lifecycle calls, retains the signal/catalog/projection/durability/follower/heartbeat/lease/release/scheduler loops in its bounded task group, retains the production catalog, capacity report, router, peer receiver, follower store, node-log transport, publisher, and release store behind typed host-owned component slots, requires every declared production component before readiness, cancels node admission before reverse provider-facility drains, serializes deadline-aware shutdown/drain callers, and the typed full-primitive reference application now proves source-directory loss, owner fencing, exact-root takeover, and continued post-takeover operations; full operator-facility construction ownership and multi-process public-host qualification remain open

## Why this matters

`crab-http-server` currently assembles runtime, follower storage, publisher,
resolver, peer transports, router, scheduler, durability recruiter/rotator,
recovery, retention, and shutdown tasks manually. The low-level pieces are
tested, but there is no single API that validates a complete node before
readiness or guarantees ordered drain/shutdown. This prevents the runtime from
being safely adopted out of the box and makes composition changes easy to apply
on only one path.

This plan adds one reusable top-level `crab-cell-host` crate. It composes
existing owners; it does not reimplement them. `crab-http-server` supplies
provider construction, peer authentication, HTTP policy, and the compiled
application, then consumes the host.

## Current state

- `crates/crab-http-server/src/server.rs:880-963` manually creates the runtime,
  follower store, publisher, resolver, peer transport/receiver, repository
  router, and scheduler.
- `crates/crab-http-server/src/server.rs:1049+` starts catalog refresh,
  projection sweep, durability recruitment/rotation, follower collection, and
  node heartbeat tasks separately.
- `crates/crab-http-server/src/cells.rs:176-182` builds the only production
  registry and registers `RepositoryModule`.
- `crates/crab-cell-runtime` already owns actor/runtime, authority, catalog,
  release, routing contracts, scheduler mechanics, durability, follower,
  backup, retention, resource ledger, and typed supervisors.
- Plan 022 provides a canonical application descriptor and handle. Do not
  duplicate that registry/topology representation in the host.

## Target ownership

`CellNode` owns exactly one instance of:

- compiled application registry/topology;
- `CellRuntime` and resource ledger;
- catalog, authority, release, migration, and backup controls;
- follower store, node log durability, recovery, and collection;
- publisher, resolver, authenticated peer dispatcher/round trip;
- Cell router and scheduler supervisors;
- effect and activity supervisors;
- placement, pressure, drain, retention, telemetry, and shutdown tasks.

The product boundary still owns:

- object-store/provider construction and credentials;
- TLS/signing identity acquisition and external authentication;
- HTTP/RPC routes and user authorization;
- process configuration parsing and user-facing error mapping.

Queue consumers remain application workers using `QueueNamespace::claim`; the
generic scheduler performs Queue maintenance only. The host must not invent a
consumer callback or treat ready Queue messages as maintenance.

## Commands you will need

| Purpose | Command | Expected on success |
| --- | --- | --- |
| Frontend prerequisite | `npm ci --prefix packages/repository && npm run build --prefix packages/repository` | exit 0 |
| Host tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-023-host cargo test -p crab-cell-host --locked` | exit 0 |
| Runtime tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-023-host cargo test -p crab-cell-runtime --release --locked` | exit 0 |
| Server tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-023-host cargo test -p crab-http-server --locked --lib` | exit 0 |
| LTX tests | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-023-host cargo test -p crab-ltx --features replica --locked` | exit 0 |
| Architecture | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-023-host make -C crab architecture-check` | exit 0; manual production composition guard passes |
| Quality | `CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-023-host cargo clippy -p crab-cell-host -p crab-cell-runtime -p crab-http-server --all-targets --locked -- -D warnings && cargo fmt --all -- --check` | exit 0 |

## Scope

**In scope**:

- New top-level `crates/crab-cell-host/` crate, guide, and symlink
- Narrow runtime visibility changes required for composition
- `crab-http-server` migration to the host and deletion of old assembly
- Deterministic single-process and three-process host fixtures
- Lifecycle/status metrics needed to prove readiness and shutdown
- Workspace manifest/lockfile changes caused by the new crate

**Out of scope**:

- Provider credential parsing, HTTP authentication, or user authorization
- New storage layouts, LTX formats, authority algorithms, or wire protocols
- Dynamic application loading or multi-language handlers
- Automatic Queue consumption
- Kubernetes operators/controllers
- Compatibility shims preserving the old server composition
- Claiming production readiness before plan 024

## Git workflow

- Branch: `codex/023-cell-node-host`
- Use dependency-ordered commits: host shell/tests; ownership moves; server
  switch; old-path deletion; architecture guard.
- Example: `refactor(http-server): compose cells through CellNode`
- Do not push/open a PR unless instructed.

## Steps

### Step 1: Build an ownership map and shutdown order test

Before moving code, map each current runtime facility to its constructor,
background task, cancellation token, drain condition, and final join. Add an
integration test around the current server fixture that records lifecycle
events and asserts the required order:

1. stop external admission;
2. stop new placement/migration/maintenance acquisition;
3. drain commands, effects, activities, publication, and node-log shipping;
4. release/fence Cells as appropriate;
5. flush/close SQLite and LTX owners;
6. stop peer listeners and background tasks;
7. return all resource reservations to baseline.

**Verify**: the test captures current behavior and exposes any missing join as
a failing assertion rather than relying on process exit.

### Step 2: Create the host crate and fail-closed builder

Create `crab-cell-host` as a top-level composition crate depending on
`crab-cell-app`, `crab-cell-runtime`, `crab-ltx`, and the existing abstract
storage/transport contracts. Add no provider SDK, HTTP framework, or auth
implementation.

Implement `CellNodeBuilder` with required inputs for application, identity,
storage/layout, durable local directory, resources, authenticated cluster
transport/signing, durability policy, and telemetry. `build()` validates all
inputs and returns no partially configured node. There is no single-node
durability fallback unless explicitly selected as a named development profile.

**Verify**: builder-negative tests reject every missing/incompatible facility;
dependency tree contains no server/provider construction layer.

### Step 3: Move composition without changing mechanics

Move the production assembly sequence from `crab-http-server/src/server.rs`
behind `CellNode`. Prefer moving code intact, then narrowing APIs. Reuse the
same catalog, authority, runtime, publisher, peer, scheduler, durability,
placement, backup, and retention implementations.

Expose only application handles and explicit operator methods:

- `start`/readiness;
- provision one declared Cell target;
- prepare/activate release;
- status/capacity;
- backup/verify/restore and bounded retention;
- drain with deadline;
- shutdown and await all tasks.

Do not expose raw actors, database paths, authority mutation, or scheduler task
handles to application code.

**Verify**: host unit/integration tests pass while the server still uses its old
path; compare release/application digests and initial roots between both paths.

### Step 4: Add a representative local cluster

Provide a test-only `TestCluster<A>` that starts three independent `CellNode`
instances with the same production registry/router/scheduler path, isolated
directories, and deterministic fault hooks. It must support owner kill/fence,
source-directory loss, follower loss, lost CAS response, duplicate effect,
Activity retry, disk admission failure, rolling-compatible release, and clean
drain.

Use the full-primitive application from plan 022. Prove provision → typed
mutation → visible read, then owner loss → exact-root recovery → continued
mutation for each namespace role.

**Verify**: tests use only public author/host APIs; no direct actor/database
mutation is permitted outside fixture fault injection.

### Step 5: Switch `crab-http-server` to the host

Make server startup construct one `CellNode`, obtain the repository application
handle/router adapter, and start HTTP only after host readiness succeeds. Route
existing release, backup, capacity, migration, peer, and shutdown operations
through the host.

Preserve authentication, authorization, request limits, error mapping, and
provider construction in the server. Do not change repository API behavior.

**Verify**: full server library suite and existing Compose qualification pass;
all existing repository operations advance the same authoritative roots.

### Step 6: Delete manual production assembly

Remove the old constructors/task startup from `server.rs` and any wrappers made
obsolete by the host. Add an architecture check that fails if production server
code directly constructs the runtime owners now owned by `CellNode`.

Search for duplicate ownership:

```bash
rg -n "CellRuntime::|FollowerStore::open|RepositoryCellScheduler::new|NodeLogRecovery::|NodeDurability::" \
  crates/crab-http-server/src crates/crab-cell-host/src
```

Every remaining server match must be a test fixture or host adapter with a
documented reason; production constructors live once.

**Verify**: architecture check fails when a deliberate forbidden constructor is
temporarily inserted, then passes after removal.

### Step 7: Prove cancellation and resource cleanup

At each startup phase, inject failure and assert earlier resources/tasks close.
Cancel during hydration, publication, effect delivery, Activity execution,
node-log recovery, migration, backup, and drain. Assert runtime ledger usage,
file descriptors, temp files, and task counts return to baseline.

**Verify**: host lifecycle tests pass repeatedly with Tokio multi-thread flavor;
no timeout is treated as success.

### Step 8: Run broad proof

Run frontend prerequisite, all host/runtime/LTX/server tests, architecture,
Clippy, format, docs validation, and `git diff --check`. Review non-test LOC;
the new host must delete comparable manual composition complexity from server.

## Done criteria

- [ ] One `CellNode` owns every listed production runtime facility and task.
- [x] Server readiness occurs only after complete host validation/startup.
- [x] Server drain/shutdown awaits work and returns resource ledgers to baseline.
- [x] Node admission is cancelled before provider-facility drains, with a deadline regression test.
- [x] Full-primitive application provisions and survives owner/source loss through public APIs.
- [x] `crab-http-server` contains no parallel production composition path.
- [x] Existing repository HTTP/auth/provider behavior remains unchanged.
- [x] Host, runtime, LTX, server, architecture, Clippy, format, and cleanup gates pass.

## STOP conditions

- An owner cannot be moved without duplicating it across old and new paths.
- Shutdown cannot await a background task or prove resource release.
- The host would need provider credentials or user authorization policy.
- A server behavior change is required beyond adapting to the host.
- The full-primitive descriptor from plan 022 is not stable.

## Maintenance notes

New runtime-wide processes must be owned and joined by `CellNode` before they
can enter production. Product adapters may configure and call the host, but
must not recreate its scheduler, authority, durability, or resource ledgers.
