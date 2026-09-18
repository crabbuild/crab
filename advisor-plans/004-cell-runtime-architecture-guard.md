# Canonical Cell runtime architecture guard and baseline

Status: DONE — source/dependency guard, characterization coverage, Cargo checks, and lint/docs gates pass on the external target volume
Priority: P1
Effort: M
Risk: Low
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`

## Executor instructions

Implement this plan in its own `codex/004-cell-runtime-architecture-guard`
branch and one focused pull request. Read root `AGENTS.md`, `crates/AGENTS.md`,
the design authority, and every file named below before editing. Use
`apply_patch`; preserve unrelated work. Do not add runtime behavior, a new
configuration knob, or a compatibility shim.

Before any Cargo command, verify `$HOME/Workspace` is mounted and writable.
Use a target directory unique to the checkout, for example
`$HOME/Workspace/crabbuild-target/crab-004-cell-guard`. Stop rather than build
into the repository if that volume is unavailable.

## Drift check

Before implementation:

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crab/scripts/check-architecture-gates.py \
  crab/scripts/test_check_architecture_gates.py \
  crates/crab-http-server/Cargo.toml \
  crates/crab-http-server/src \
  crates/crab-cell-runtime/src \
  crates/crab-cell-runtime/tests
```

If production server code has gained a direct `crab-ltx` dependency/import,
or another canonical Cell composition owner has been introduced, stop and
reconcile the design before implementing the guard.

## Why this plan exists

The target architecture has one production composition path:
`crab-http-server -> crab-cell-runtime -> crab-ltx`. Today this is convention,
not an enforced boundary. `crab-http-server` correctly declares `crab-ltx`
only as a dev-dependency, but future code can still import it directly and
create a second owner for publication, fencing, or recovery. The later plans
also refactor safety-critical behavior; characterization tests are required so
the refactor cannot redefine current acknowledgements or fencing.

## Current state and evidence

- `crates/crab-http-server/Cargo.toml` has `crab-cell-runtime` in normal
  dependencies and `crab-ltx` in dev-dependencies for test fixtures.
- `crates/crab-http-server/src/cells/router.rs` routes production operations
  through `CellRuntime` and `CellReplica`; test modules use `crab-ltx` helpers.
- `crab/scripts/check-architecture-gates.py` already owns cross-crate scope and
  dependency checks.
- `crab/scripts/test_check_architecture_gates.py` supplies temporary-tree
  positive and negative fixtures for those checks.
- `crates/crab-cell-runtime/tests/publication.rs` covers lost CAS response,
  takeover fencing, and renewal rebasing. `tests/actor.rs` covers actor
  activation, command publication, and source-loss recovery.

The gate must distinguish normal dependencies and production imports from
dev-dependencies and `#[cfg(test)]` code. A repository-wide string ban would
reject legitimate fixtures and is not acceptable.

## Scope

- Extend the existing architecture checker with a named Cell composition gate.
- Reject `crab-ltx` as a normal/build dependency of `crab-http-server`.
- Reject direct `crab_ltx::` use in production server modules while admitting
  explicitly test-gated code.
- Add fixture tests proving both rejection and admitted test usage.
- Add characterization coverage for the current acknowledgement, fence,
  release, and ambiguous-CAS behavior if an existing test does not already
  assert the externally visible outcome.
- Update the architecture documentation and design delivery ledger.

## Out of scope

- Moving existing runtime behavior.
- Removing the server's `crab-ltx` dev-dependency.
- Extracting the coordination kernel.
- Changing a serialized control, catalog, node, or LTX shape.
- Adding CI independent of the existing architecture workflow.

## Implementation steps

### 1. Establish the exact admitted and forbidden surfaces

Search all direct references and classify each as production, unit test,
integration test, or manifest dependency:

```bash
rg -n "crab[-_]ltx" crates/crab-http-server
cargo metadata --format-version 1 --locked > /tmp/crab-004-metadata.json
```

Document the classification in the checker tests. If a production reference
already exists, stop: this plan must not grandfather it without a design
decision.

### 2. Add a dependency-kind guard

Add a focused checker function that reads Cargo metadata and fails when the
`crab-http-server` package directly depends on `crab-ltx` with normal or build
kind. Dev kind is admitted. Include the package, dependency kind, and expected
owner in the diagnostic.

Verify with temporary metadata fixtures:

- normal dependency fails;
- build dependency fails;
- dev dependency passes;
- transitive dependency through `crab-cell-runtime` passes.

### 3. Add a production-import guard

Use the existing architecture checker's source-scanning conventions. Scan
`crates/crab-http-server/src` and reject direct `crab_ltx::` paths outside
test-only regions/files. Do not implement an imprecise replacement that strips
all text after the first `#[cfg(test)]`. Fixture tests must include production
code before and after a test module and a nested inline test module.

If the checker cannot reliably classify inline Rust test regions without a
new parser dependency, use the narrower enforceable rule: production modules
must not contain direct references, and test helpers requiring them must live
in named test modules/files admitted by an exact path allowlist. Keep the
allowlist line/path scoped like existing architecture fixtures.

### 4. Lock current runtime behavior

Review existing actor/publication tests before adding coverage. Add only
missing characterization cases, table-driven where possible, for:

- response release after exact-root publication;
- response release after valid follower durability proof while the actor stays
  retained until exact publication;
- a different CAS winner fences and does not acknowledge;
- shutdown drains accepted work before authority release;
- lost update response adopts only the exact expected successor.

These tests describe the current contract; they must pass before and after any
production change. Do not duplicate assertions already proved at the same
boundary.

### 5. Connect documentation and the existing gate entry point

Register the new checker in `check-architecture-gates.py`'s existing main
entry point so `.github/workflows/architecture.yml` runs it through
`make architecture-check`. Update the design delivery table to point at the
enforced gate and characterization test names.

## Verification

```bash
python3 -m unittest crab/scripts/test_check_architecture_gates.py
make -C crab architecture-check

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-004-cell-guard \
  cargo test -p crab-cell-runtime --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-004-cell-guard \
  cargo test -p crab-http-server --locked

CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-004-cell-guard \
  cargo clippy -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings

cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
git diff --check
```

Expected results: every command exits zero; fixture mutations demonstrate that
normal/build dependencies and production imports fail with the new diagnostic;
legitimate dev/test usage passes.

## Acceptance criteria

- [x] The server cannot add a direct production `crab-ltx` dependency without
      failing the existing architecture workflow.
- [x] Direct production `crab_ltx::` use in server source fails the gate.
- [x] Current dev-dependency and test-only fixtures remain admitted by an exact,
      tested rule rather than a broad exemption.
- [x] Existing acknowledgement, fence, release, and ambiguous-CAS behavior has
      named characterization proof with no production behavior change.
- [x] No new dependency, feature, environment variable, stored format, or
      runtime branch is introduced.
- [x] Documentation names `crab-cell-runtime` as the sole production owner.
- [x] All verification commands above pass.

## Stop conditions

- A production server caller demonstrably requires direct `crab-ltx` ownership.
- Reliable source classification would require a new parsing dependency; stop
  for approval rather than add it silently.
- A characterization test exposes an existing correctness failure. Report the
  failure separately; do not redefine the test to match unsafe behavior.

## Maintenance note

Keep this gate structural. It should enforce ownership, not enumerate every
runtime type. When test fixtures move, update their exact admission and its
negative sibling fixture in the same change.
