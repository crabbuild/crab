# crab-cell-runtime

Root `AGENTS.md`, `crates/AGENTS.md`, and `docs/README.md` apply.

## Purpose and ownership

Embedded SQLite Cell runtime: identities, control/CAS authority, the single-Cell
actor and executor, schema installation, publication, follower durability,
fleet placement, and qualification receipts. HTTP, authentication, provider
construction, and product policy stay in `crab-http-server`.

## Read first

1. `src/lib.rs` — module declarations and the frozen root prelude.
2. `src/cell/actor.rs` — the actor root. `actor/task.rs` and
   `actor/requests.rs` drive the loop and the request paths, `actor/tasks.rs`
   and `actor/lifecycle.rs` handle finished tasks and cell scheduling,
   `actor/{handle,state,runtime}.rs` own the handle, projections, and runtime
   administration, and `actor/admission.rs` fences all of them.
3. `src/cell/executor.rs` and `src/cell/worker.rs` with `src/cell/worker/run.rs`
   — command execution and the bounded SQL worker pool.
4. `src/publication.rs` and `src/recovery/manifest.rs` — exact-root publication
   and recovery artifacts.
5. `src/coordination.rs` — the pure `pub(crate)` coordination kernel, with the
   deterministic simulator in `src/coordination/sim.rs`.
6. `docs/runtime.md` and `docs/delivery.md` — request path and evidence map.

## Module map

Subsystem roots keep the shared contract and helpers; the named child modules
own one concern each. Module files sit beside their root (`foo.rs` + `foo/`).

- Cell: `cell/{actor.rs,catalog.rs,application.rs,executor.rs,schema.rs,worker.rs}`,
  `cell/actor/{admission,handle,lifecycle,requests,runtime,state,task,tasks}.rs`,
  `cell/worker/run.rs`.
- Control and clients: `control.rs` + `control/authority.rs`, `client.rs`,
  `peer.rs` + `peer/{dispatch,protobuf,transport}.rs`.
- Durability and followers: `follower.rs` + `follower/records.rs`,
  `node/{advertisement,capacity,durability,lease,log,log_shipper,log_state,log_transport}.rs`,
  `node/directory.rs` + `node/directory/{advertisement,log,recovery}.rs`,
  `node/log_recovery.rs` + `node/log_recovery/witness.rs`.
- Fleet and recovery: `fleet/{scheduler,placement,pressure,eviction,resource,telemetry}.rs`,
  `recovery/{manifest,release,release_progress,artifacts}.rs`,
  `recovery/backup/restore.rs`, `recovery/retention.rs`.
- Primitives: `primitives/<name>.rs` with the wire codecs in
  `primitives/<name>/api.rs`; Effects adds `supervisor.rs` and Workflow adds
  `activity.rs`, `activity_api.rs`, `activity_codec.rs`, and `maintenance.rs`.
- Registry and qualification: `registry/{builder,descriptor,handlers,schemas}.rs`,
  `qualification/{profile,receipt,workload,cluster}.rs`,
  `qualification/receipt/{matrix,runner}.rs`, `qualification/tests/`.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Add a primitive operation | `src/primitives/<name>.rs` | `src/registry/schemas.rs`, `tests/primitives/` |
| Add primitive maintenance | `src/fleet/scheduler.rs` | that primitive's `TABLE` constant, `tests/runtime/scheduler.rs` |
| Wire or defer a policy seam | `src/cell/actor.rs` | `crab/scripts/check-policy-entry-points.py` |
| Change admission or lifecycle | `src/cell/actor/admission.rs`, `src/cell/actor/lifecycle.rs` | `src/coordination.rs`, `tests/runtime/lifecycle.rs` |
| Change publication | `src/publication.rs` | `src/recovery/`, `tests/runtime/publication.rs` |
| Change node log or durability | `src/node/` | `src/follower.rs`, `tests/fleet/` |
| Change placement or pressure | `src/fleet/` | `tests/fleet/`, `docs/canonical-ltx-scaling.md` |
| Change pressure sampling or shedding | `src/cell/actor/task.rs` | `src/cell/actor/runtime.rs`, `src/fleet/pressure.rs`, `src/fleet/telemetry.rs`, `tests/fleet/pressure.rs` |

## Layout and tests

- `src/` is production code. Integration tests live in `tests/` as one binary
  per suite: `runtime`, `primitives`, `protocol`, `contracts`, `fleet`,
  `qualification`. Suite modules live in the matching directory.
- `tests/support/` holds the shared harness; suites declare `mod support;` and
  refer to `crate::support::…`. Never add a `#[path]` attribute.
- Tests that assert crate-private behavior stay in their module and are listed
  in `tests-allow-list.txt` with a reason. New in-src tests must be added there;
  prefer moving the behavior behind the public API when that is honest.
  Each entry must name a file that still holds tests or test modules, so a moved
  or emptied test location cannot leave a stale entry behind.
- `api-prelude.txt` is the frozen root surface. Adding a root re-export means
  editing both `src/lib.rs` and that file in the same commit.
- Run `python3 crab/scripts/check-cell-ltx-layout.py` after layout changes.

## Invariants

- One fenced writer per Cell; a successful response follows durable publication
  or a durable follower proof (`src/cell/actor.rs`, `src/publication.rs`).
- Recovery verifies the authority-pinned root and every referenced object
  (`src/recovery/manifest.rs`).
- Staged xorbs flush before any bundle publication.
- Every acquired lock is released on success, error, cancellation, and timeout.
- A node sheds settled Cells only on sustained evidence: the actor samples its own
  reservation ledger on its tick, the classifier hands a tier to the bounded
  eviction path only after a full dwell window above the enter threshold, and
  that tier is what the node reports through
  `CellTelemetry::pressure_state` (`src/cell/actor/task.rs`,
  `src/fleet/pressure.rs`).
- `src/coordination.rs` stays sans-I/O: no `async`, no clock, no storage
  (`crab/scripts/check-cell-ltx-layout.py` enforces it).

## Features and platform

- `test-support` enables `src/test_support.rs` and the `cell_movement_probe`
  binary. Integration suites that need the process fixture run with
  `--features test-support`.
- `crab-ltx` is always consumed with its `replica` feature from this crate.

## Verification

```sh
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> \
  cargo test -p crab-cell-runtime --features test-support --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/<checkout> \
  cargo clippy -p crab-cell-runtime --all-targets --features test-support --locked -- -D warnings
python3 crab/scripts/check-cell-ltx-layout.py
```

Use a target directory unique to the checkout; never share it between
worktrees. Protected qualification receipts come from the workflows under
`.github/workflows/cell-runtime-*.yml`, not from local runs.

## Related documentation

`docs/README.md`, `docs/runtime.md`, `docs/delivery.md`,
`docs/canonical-ltx-scaling.md`, `qualification/README.md`, and
`../../advisor-plans/033-cell-ltx-layout-reorganization.md`.
