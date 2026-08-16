# Phase 003: Remove every public-to-private dependency edge

> **Executor instructions**: This phase changes test ownership without changing
> runtime behavior. Public packages must be testable with public fixtures only.
> Run all verification commands. Stop instead of copying private server internals
> into public tests. Update Phase 003 in `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat 98b5c1e8..HEAD -- crab/Cargo.toml crates/crab-cache-store/Cargo.toml crates/crab-cache-store/src/lib.rs crates/crab-cache crates/crab-auth crab/scripts/check-architecture-gates.py`
> Re-search all manifests if these paths or the package inventory changed.

## Status

- **Priority**: P0
- **Effort**: L (two to four days)
- **Risk**: MED — replacing a full server fixture with a weak mock can reduce
  contract coverage.
- **Depends on**: Phase 002
- **Category**: tests / migration
- **Planned at**: commit `98b5c1e8`, 2026-08-14

## Why this matters

Cargo dev dependencies are still dependencies when contributors run tests.
Today two public candidates require the private cache server to compile test
targets. A public repository that omits the server would therefore be
incomplete. The correct split is public client-contract tests plus private
server conformance tests, not a public copy of private server internals.

## Current state

- `crab/Cargo.toml:414` declares `crab-cache-server` as a dev dependency, but
  `rg` finds no Rust import of `crab_cache_server` under `crab/src` or
  `crab/tests`; this edge is expected to be removable.
- `crates/crab-cache-store/Cargo.toml:29` declares `crab-cache-server` as a dev
  dependency.
- `crates/crab-cache-store/src/lib.rs:1664` contains an inline test module.
  Lines 1671–1685 import private server storage, database, eviction, metrics,
  origin, configuration, and router internals. Those are server integration
  tests living under the wrong owner.
- `crates/crab-cache/Cargo.toml:13` already has an `axum` dev dependency and
  owns remote-client DTO and route contract tests.
- The architecture decision at
  `crab/docs/architecture/multi-crate-transition.md:84` explicitly rejects new
  wire-only protocol crates; shared cache wire contracts remain in
  `crab-cache`.

Target dependency shape:

```text
public crab-cache-store tests -> public Axum contract fixture -> crab-cache DTOs
private crab-cache-server tests -> public crab-cache DTOs + private server state
public package manifests -X-> crab-cache-server / crab-auth-server
```

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Find manifest leaks | `rg -n 'crab-(auth|cache)-server' --glob Cargo.toml Cargo.toml crab crates crab-sdk crab-py` | matches only root/private package definitions; no public package dependency table |
| Public cache tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 cargo test -p crab-cache --all-features -p crab-cache-store --all-features --locked` | exit 0 |
| CLI tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 cargo test -p crab --locked` | exit 0 |
| Private conformance | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 cargo test -p crab-cache-server --locked` | exit 0 |
| Boundary gate | `cd crab && CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 make architecture-check` | exit 0 and zero allowed public→private fixture edges |

## Scope

**In scope**:

- Remove the unused `crab-cache-server` dev dependency from `crab/Cargo.toml`.
- Remove the server dev dependency from
  `crates/crab-cache-store/Cargo.toml`.
- Extract tests that exercise public remote-cache client behavior into a public
  integration test using a minimal Axum HTTP fixture.
- Move tests whose assertions are about server DB, eviction, origin policy,
  metrics, or router internals to `crates/crab-cache-server/tests/`.
- Add public wire samples under `crates/crab-cache/tests/fixtures/` only if a
  serialized request/response compatibility assertion needs them.
- Tighten the architecture checker from “documented dev fixture edges allowed”
  to “zero public dependency edges to private packages.”

**Out of scope**:

- Moving production DTOs to a new crate.
- Reimplementing server persistence, eviction, or routing in public test code.
- Changing cache HTTP routes, status codes, payloads, retry behavior, or auth.
- Removing user-facing strings that mention `crab-cache-server` from doctor
  diagnostics.
- Splitting repositories; Phase 005 owns export.

## Steps

### Step 1: Prove and remove the unused CLI dev edge

Search `crab/src`, `crab/tests`, examples, benches, and build scripts for both
the package and Rust crate names. If there is no use, delete only the manifest
entry and regenerate `Cargo.lock` with Cargo.

**Verify**:
`CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-56c1 cargo test -p crab --locked`
exits 0 and `cargo tree -p crab --edges dev | rg crab-cache-server` returns no
match.

### Step 2: Classify the cache-store tests by assertion owner

For every remote-client test currently using server internals, write a table in
the PR description or `docs/open-source/evidence/003-test-ownership.md`:
test name, behavior asserted, public-client or private-server owner, new path.
Client-owned tests assert outbound method/path/headers/body and handling of
responses/errors. Server-owned tests assert persistence, eviction, metrics,
origin policy, database files, or router state.

**Verify**: every import at `crates/crab-cache-store/src/lib.rs:1671`–`:1685`
has a destination and no test is silently deleted.

### Step 3: Build the public client contract fixture

Create `crates/crab-cache-store/tests/remote_client_contract.rs` (or the nearest
existing integration-test location) with an Axum listener that implements only
the routes exercised by `crab-cache-store`. Construct requests/responses using
public `crab-cache` DTOs/constants. Record received calls in test-local state.
Do not import or mirror private persistence types.

Cover at least: cache hit, cache miss/fallthrough, corrupt/hash-mismatch
response, authorization failure, retryable server failure if the public client
contract promises it, and cancellation/timeout if currently promised.

**Verify**: public cache/store tests pass with the private server package removed
temporarily from a generated workspace manifest.

### Step 4: Relocate server behavior tests

Move server-owned cases into `crates/crab-cache-server/tests/` or its existing
unit modules. Preserve meaningful coverage and use the real private router/state.
Tests may use `crab-cache` public DTOs because the private server legitimately
depends downward on public contracts.

**Verify**: `cargo test -p crab-cache-server --locked` passes and the before/after
test-ownership table has no unaccounted case.

### Step 5: Enforce the zero-edge rule

Update `check-architecture-gates.py` so any normal, build, or dev dependency
from a `public-core` package to a `private-platform` or `private-product`
package fails. Keep the inverse dependency legal.

**Verify**: add a negative checker fixture for a public dev dependency; the
checker test passes and `make architecture-check` reports zero exceptions.

## Test plan

- Public client fixture tests assert observable HTTP contract, not private
  implementation calls.
- Private conformance tests assert the real server produces/consumes the same
  public DTOs.
- Existing public and server test counts are recorded before/after; a lower
  count requires a one-to-one explanation.
- Run tests with `--all-features` for the two public cache crates.
- Run the full architecture gate after all moves.

## Done criteria

- [ ] No public Cargo package has any dependency edge to
      `crab-auth-server` or `crab-cache-server`.
- [ ] `cargo tree` proves zero public reverse consumers of both server packages.
- [ ] Public remote-client tests compile and run without server source present.
- [ ] Private server integration coverage is preserved under the server owner.
- [ ] No new protocol crate or duplicated production DTO was created.
- [ ] Public cache/store, CLI, private server, and architecture commands all
      exit 0.
- [ ] Phase 003 status is `DONE`.

## STOP conditions

Stop and report if:

- A supposedly public-client assertion requires private persistence or eviction
  state to be meaningful.
- Removing a dev edge reveals a production module imported only through a test
  feature.
- The public fixture would need to copy a private route handler or database
  implementation.
- Existing tests reveal an undocumented wire behavior; define and approve the
  contract before encoding it.
- Any runtime behavior change is required.

## Handoff artifact

Provide `docs/open-source/evidence/003-test-ownership.md`, the successful test
commands, and the architecture output proving zero public-to-private dependency
exceptions. Phase 005 uses this as repository-seam proof.

## Maintenance notes

Future server features land with two tests when they alter the wire contract:
a public client-contract test owned by `crab-cache`/`crab-auth`, and a private
server conformance test. Implementation-only server behavior stays private.
