# Plan 033: Re-organize Cell and LTX crate layout for Cellule extraction

> **Executor**: read root `AGENTS.md`, `crates/AGENTS.md`, and this whole file
> before editing. This plan changes file layout, visibility, and test
> organization. It must not change runtime behavior, persisted contracts, or
> LTX/Cell formats. Work from a checkout with its own `CARGO_TARGET_DIR`.
>
> **Drift check**: `git diff --stat 3ab2526492b..HEAD -- crates/crab-cell-runtime
> crates/crab-cell-app crates/crab-cell-host crates/crab-ltx`. If a file in the
> mapping tables moved, or a crate's test count changed, refresh the affected
> table before executing that stage.

## Status

- Priority: P1; effort: L; risk: MEDIUM (mechanical moves; public API shape is
  the only consumer-visible change, isolated in stage 6).
- Category: layout / test organization / extraction readiness.
- Depends on: none. Stages 1–5 are behavior-free and may land before any other
  Cell-runtime work. Stage 6 reshapes the public module tree and needs the
  consumer migration described there.
- Planned at: `3ab2526492b` (`#291`), 2026-09-22.
- Status: TODO.
- Decision recorded 2026-09-22: stage 6 uses **Option A**, the subsystem module
  tree in "Target module tree".
- Delivery: PR 1 = stages 1–5 (layout, no API change); PR 2 = stage 6 (module
  tree, consumer migration, prelude freeze). The Cellule synthesis is a
  separate change in the Cellule repository (see "Cellule handoff"); this plan
  stops at the Crab PRs.
- Status 2026-09-23: implemented as one branch
  (`codex/cell-ltx-layout-reorganization`) on user request instead of two PRs.
  Stages 1–3, 5, 6a–6c are complete; stage 4 is complete for `node`,
  `registry`, `crab-cell-host`, and `crab-ltx`'s environment, and remains for
  `actor.rs` and `qualification.rs`.

## Why this matters

`crab-cell-runtime`, `crab-cell-app`, `crab-cell-host`, and `crab-ltx` are the
source for the `cellule-*` workspace. Cellule is the future upstream: after its
first release Crab consumes published `cellule-*` crates and these four crates
are deleted. Every layout decision made now is inherited by a published
framework, so the cost of keeping the current ad-hoc layout is permanent while
the cost of fixing it is one mechanical PR.

The current layout has five test placements in one crate, integration test
files that shadow source-module names, test-only source reached through
`#[path]` includes, and duplicated fixtures that have already diverged:

- `crates/crab-cell-runtime` holds 41 inline `#[cfg(test)] mod tests` blocks,
  10 `src/<module>/tests.rs` modules, one `#[path]`-mounted `src/effects_tests.rs`,
  a `#[cfg(test)] mod coordination_sim` in `lib.rs`, and 17 `tests/*.rs`
  binaries. `src/client.rs`, `src/client/tests.rs`, and `tests/client.rs` all
  exist.
- `crates/crab-ltx` splits unit tests the same three ways (`src/*` inline,
  `src/format_tests.rs`, `tests/*.rs`).
- `crates/crab-cell-runtime/src/process_store.rs` is reached by `#[path]` from
  `src/bin/cell_movement_probe.rs:8`, `tests/actor.rs:12`, and
  `crates/crab-cell-app/tests/reference_application.rs:38`; the last one forces
  `crab-cell-app` to carry `fs4`/`async-trait`/`object_store` dev-dependencies
  for a file it does not own.
- Two more `#[path]` attributes are pure noise: `src/worker.rs:1353` points at
  the `worker/tests.rs` that plain `mod tests;` already resolves, and
  `src/effects.rs:14` renames `effects_tests.rs` instead of placing it under
  `effects/`.
- `fence_session` exists three times: `tests/support/mod.rs:9`,
  `tests/actor.rs:378`, `tests/workflow_api.rs:33`, with different fixture
  constants.
- Documentation already drifted: `crates/crab-cell-runtime/docs/delivery.md:28`
  points at `tests/effects.rs`, which does not exist.
- `crates/crab-cell-runtime/AGENTS.md` and `crates/crab-ltx/AGENTS.md` are the
  only missing crate guides among these four; all six Cellule crates have one.

## Non-goals

- No behavior change, no new feature, no error-variant change, no dependency
  addition beyond moving an existing dev-dependency edge.
- No change to persisted or exchanged formats: `crab.*.v1` hash domains, CRB1
  bundle footer keys, LTX file layout, Cell object paths, descriptors, schema
  versions, and peer wire messages are untouched.
- `crab/tests` (98 top-level test binaries) and `crab-http-server` tests are
  out of scope; record them as separate follow-up work.
- Stage 6 changes Rust module paths only. It must not rename persisted
  identifiers, wire fields, hash domains, or schema objects.
- `crab-ltx`'s upstream attribution (`UPSTREAM.md`, `LICENSE`,
  `LICENSE.pierrec-lz4`, adapted-file headers) must keep working; do not
  restructure files that upstream review diffs against Celld without noting it
  in `UPSTREAM.md`.
- The Cellule-side rename, hardening preservation, and release work.

## Baseline inventory (2026-09-22, `3ab2526492b`)

| Crate | Rust files | Lines | Tests | `tests/` binaries | In-src test locations |
| --- | --- | --- | --- | --- | --- |
| crab-cell-runtime | 100 | 80,785 | 473 | 17 | 53 |
| crab-ltx | 44 | 21,602 | 115 | 6 | 15 |
| crab-cell-app | 7 | 4,479 | 20 | 2 (+4 `#[path]` modules) | 1 (inline in `src/lib.rs`) |
| crab-cell-host | 1 | 2,459 | 33 | 0 | 1 (inline in `src/lib.rs`) |

Counts cover `src/` and `tests/` only. `crab-ltx/perf/celld` and
`crab-ltx/perf/crab` are separate Cargo projects with their own lockfiles and
are excluded; their six test attributes are not part of this plan.

Static reachability estimate used throughout this plan (name-based, see
"How the disposition column was measured"): of the in-src tests,
`crab-cell-runtime` has 59 whose references are all reachable from the crate
root and 263 that touch at least one item the root does not re-export;
`crab-ltx` has 0 and 52. `crab-cell-app` (7 tests) and `crab-cell-host`
(33 tests) reference only public items, so both can move to `tests/` with no
visibility change at all.

## Target layout

The rule: **`src/` is production code, `tests/` is the test surface, and the
few tests that cannot leave `src/` are enumerated.** Rust integration tests are
separate crates, so a test can only leave `src/` when everything it calls is
reachable from the crate root; `pub(crate)` items cannot be re-exported
(`E0364`).

| Bucket | Where | Rule |
| --- | --- | --- |
| Public-contract tests | `tests/<suite>/…` | Default. Everything a framework user can call. |
| Private-mechanics tests | `src/<file>.rs` bottom `#[cfg(test)] mod tests`, or `src/<module>/tests.rs` | Only when the test needs a private/`pub(crate)` item. Every occurrence is listed in the crate allow-list. |
| Cross-crate and end-to-end | `tests/<suite>/…` through one gated `test_support` module | The shim exposes only already-`pub` items plus named fixtures; it is `#[doc(hidden)]`, feature-gated, and not part of the compatibility promise. |

```text
crates/<crate>/
├── src/                      # production logic
│   ├── lib.rs                # module declarations and curated re-exports
│   ├── <topic>.rs
│   ├── <topic>/<submodule>.rs
│   └── test_support.rs       # #[cfg(any(test, feature = "test-support"))] pub mod test_support;
├── tests/
│   ├── support/mod.rs        # shared fixtures; subdirectory so it is not a target
│   └── <suite>/main.rs       # one auto-discovered binary per suite
│       └── <topic>.rs
├── docs/ qualification/ model/   # unchanged locations
├── AGENTS.md  README.md
└── tests-allow-list.txt      # in-src test locations, one path per line, with a reason
```

Conventions:

- Module files use `foo.rs` + `foo/`; no new `mod.rs` (Rust Book ch. 7.5). The
  40 existing `mod.rs` files elsewhere in the repo are not in scope.
- Suites are named for the capability, never for a source file: `tests/runtime/`,
  not `tests/actor.rs`.
- Test names stay behavioral sentences; regression cases go in a
  `regression.rs` module rather than an issue-number prefix.
- `tests/support/mod.rs` owns fixtures only. `fixture()`, `fixture_for(…)`,
  and `fence_session(…)` live there once, not per binary.
- Feature names: `replica` stays a capability; `process-test-support` becomes
  `test-support`, matching `cellule-store`'s existing feature.

## Target module tree

Stage 6's recorded decision replaces the 458-name flat root with owned
subsystem modules. Every current top-level module maps to exactly one target:

| Target module | Contains today | Ownership |
| --- | --- | --- |
| `identity` | `identity` | Cell, tenant, session, namespace, digest identities |
| `control` | `control`, `authority` | Control record, transitions, CAS authority |
| `codec` | `codec` | Bounded wire codec contracts used by module authors and product tests |
| `registry` | `registry`, `registry::descriptor` | Application contracts: modules, commands, queries, migrations |
| `cell` | `actor` (+`actor::handle`), `executor`, `worker`, `catalog`, `schema`, `application` | One Cell: activation, admission, execution, catalog, schema |
| `client` | `client` | Typed client, prepared commands, state streams |
| `primitives` | `sql`, `kv`, `blob`, `queue`, `cron`, `workflow`, `effects`, `activity_pool`, `maintenance` | Application-facing distributed primitives |
| `publication` | `publication` | Exact-root LTX publication |
| `follower` | `follower` | Follower store, lanes, tail pages |
| `node` | `node`, `node_lease`, `node_log`, `node_log_recovery`, `node_log_shipper`, `node_log_state`, `node_log_transport`, `node_durability` | Node identity, signed advertisements, node log, durability |
| `recovery` | `recovery_manifest`, `recovery_artifacts`, `release`, `release_progress`, `backup`, `retention` | Recovery artifacts, release control, pins, retention |
| `fleet` | `placement`, `pressure`, `resource`, `eviction`, `scheduler`, `telemetry` | Fleet placement, pressure, admission accounting, scheduling |
| `peer` | `peer`, `peer::dispatch`, `peer::transport` | Authenticated peer protocol (`protobuf` stays private) |
| `qualification` | `qualification`, `cluster_qualification` | Qualification harness, workloads, receipts |
| `ltx` | re-exports of `crab_ltx::{CaptureTiming, CellObjectKind, CellReplica, CellStorageLayout, DiskBudget, Host, Limits, LtxPhase, LtxReadOrigin, LtxRequestOutcome, ScratchMonitor}` | LTX façade, see below |
| private | `coordination`, `coordination::sim`, `error`, `test_support` | Pure kernel, error type, test shim |
| root prelude | — | `Error`, `Result`, and the entry points `crab-cell-app`, `crab-cell-host`, and the two `src/bin` targets import; frozen in `crates/crab-cell-runtime/api-prelude.txt` |

The `ltx` façade is deliberate: `crab-cell-host` (and later `cellule-host`) may
only depend on `cellule-app` and `cellule-runtime`, and Cellule's
`scripts/check-boundaries.py` enforces that. The runtime therefore keeps
re-exporting the LTX surface its own API exposes — today as the flat
`Host as ReplicaHost` / `Limits as ReplicaLimits` aliases — and the aliases are
dropped in favor of the single `crab_cell_runtime::ltx::{Host, Limits, …}`
path. No crate gains or loses a dependency edge.

`crab-ltx` is already module-shaped (`environment`, `bundle`, `replica`,
`writable_vfs`, `node_frame`, plus root re-exports) and gets no file moves
beyond stages 3–4. Stage 6 gives it the same treatment as the runtime: a frozen
root list in `crates/crab-ltx/api-prelude.txt`, explicit `pub mod` ownership for
the `capture`/`db`/`error`/`types` surfaces that today are re-exported flat, and
the same checker rule. `pub use rusqlite;` stays: embedders need the exact
`rusqlite` version this crate compiles against.

Rules for the tree:

- A module is public only when a consumer outside the crate needs it; otherwise
  it stays `pub(crate)` and is never re-exported.
- The prelude is a reviewed list, not a convenience dumping ground. Growth
  requires updating `api-prelude.txt` in the same commit.
- `coordination` stays `pub(crate)`; Cellule's `scripts/check-boundaries.py`
  asserts that kernel shape.
- Names are Rust `snake_case` and describe the domain, not the layer.

## Stage 1 — Make test support a module

Files: `crates/crab-cell-runtime/src/process_store.rs` →
`crates/crab-cell-runtime/src/test_support.rs` (content unchanged);
`crates/crab-cell-runtime/src/lib.rs`; `crates/crab-cell-runtime/Cargo.toml`;
`crates/crab-cell-runtime/src/bin/cell_movement_probe.rs`;
`crates/crab-cell-runtime/tests/actor.rs`;
`crates/crab-cell-app/Cargo.toml`;
`crates/crab-cell-app/tests/reference_application.rs`.

Steps:

1. Move `process_store.rs` to `test_support.rs` unchanged, and declare it:
   `#[cfg(any(test, feature = "test-support"))] pub mod test_support;`.
2. Rename the feature `process-test-support` → `test-support`; keep
   `[[bin]] cell_movement_probe required-features = ["test-support"]`.
3. Delete the three `#[path]` attributes that include `process_store.rs`;
   callers use `crab_cell_runtime::test_support::FilesystemCasStore`.
4. Enable the feature for `crab-cell-app`'s test targets with a
   `[dev-dependencies]` entry
   (`crab-cell-runtime = { workspace = true, features = ["test-support"] }`)
   and drop the dev-dependencies that existed only for the borrowed file
   (`fs4`; verify `async-trait`/`object_store` are still needed).
5. Delete the `#[path]` modules in `crab-cell-app/tests/reference_application.rs`
   only when stage 2 creates the suite directory; stage 1 keeps them compiling
   with the module path.

Gate: `cargo check -p crab-cell-runtime` must not compile `test_support`;
`cargo test -p crab-cell-runtime --features test-support --locked` and
`cargo test -p crab-cell-app --locked` pass. The three `process_store.rs`
includes are gone; the remaining `#[path]` attributes in
`crates/crab-cell-runtime/src/{worker.rs,effects.rs}` and
`crates/crab-cell-app/tests/reference_application.rs` are removed in stage 2.

## Stage 2 — Suite scaffolding and one shared harness

Create the suite roots and move existing binaries into them **without changing
test bodies**:

| Current | Target |
| --- | --- |
| `tests/actor.rs` | `tests/runtime/lifecycle.rs` |
| `tests/migration.rs` | `tests/runtime/migration.rs` |
| `tests/publication.rs` | `tests/runtime/publication.rs` |
| `tests/workers.rs` | `tests/runtime/workers.rs` |
| `tests/scheduler.rs` | `tests/runtime/scheduler.rs` |
| `tests/catalog.rs` | `tests/runtime/catalog.rs` |
| `tests/sql.rs` | `tests/primitives/sql.rs` |
| `tests/kv.rs` | `tests/primitives/kv.rs` |
| `tests/blob_cron.rs` | `tests/primitives/blob.rs` + `tests/primitives/cron.rs` |
| `tests/queue.rs` | `tests/primitives/queue.rs` |
| `tests/workflow.rs` | `tests/primitives/workflow.rs` |
| `tests/workflow_api.rs` | `tests/primitives/workflow_api.rs` |
| `tests/client.rs` | `tests/protocol/client.rs` |
| `tests/codec.rs` | `tests/contracts/codec.rs` |
| `tests/registry.rs` | `tests/contracts/registry.rs` |
| `tests/qualification_preflight.rs` | `tests/qualification/preflight.rs` |
| `tests/qualification_receipt.rs` | `tests/qualification/receipt.rs` |
| `tests/support/mod.rs` | `tests/support/runtime.rs` (`mod.rs` re-exports) |
| `crab-cell-app/tests/reference_application.rs` + 4 `#[path]` modules | `crab-cell-app/tests/reference_application/main.rs` + sibling modules |
| `crab-cell-app/tests/contract_validation.rs` | `crab-cell-app/tests/contracts/main.rs` |
| `crab-ltx/tests/{cell_roots,cell_parallel_restore,replication}.rs` | `crab-ltx/tests/cell/{roots,restore,replication}.rs` |
| `crab-ltx/tests/{crash,node_frame}.rs` | `crab-ltx/tests/ltx/crash.rs`, `crab-ltx/tests/ltx/node_frame.rs` |
| `crab-ltx/tests/host_hooks.rs` | `crab-ltx/tests/host/hooks.rs` |

Then:

1. Move the three `fence_session` copies and the `Fixture`/fault-store helpers
   from `tests/actor.rs` into `tests/support/`. Delete the copies; the actor
   suite no longer defines its own helpers.
2. Give each suite a `main.rs` listing its modules; do not use `#[path]`.
3. Keep test names byte-identical so review can diff `--list` output.

Gate: `cargo test -p crab-cell-runtime --features test-support -- --list`,
`cargo test -p crab-cell-app --locked -- --list`, and
`cargo test -p crab-ltx --features replica --locked -- --list` produce the same
test names as the baseline (record both counts in the PR description).
`rg -n '#\[path' crates/crab-cell-runtime crates/crab-cell-app crates/crab-ltx`
returns nothing. `crab-cell-host` is unchanged in this stage.

## Stage 3 — Move public-contract tests out of `src/`

For each in-src test location, attempt the move; a move is allowed only when the
test compiles against the crate root. When it needs a private item, either keep
it in `src/` (add to `tests-allow-list.txt` naming the private item) or expose
it through `test_support` when it is already `pub` behind a private module.
Never widen an item from `pub(crate)` to `pub` for a test.

Disposition by evidence (counts are the static estimate above):

| Location | Tests | Disposition |
| --- | --- | --- |
| `src/{application,cluster_qualification,control,identity,node_lease,node_log_transport,placement,pressure,recovery_artifacts,recovery_manifest,release,release_progress,schema,telemetry}.rs` | 59 | Move to `tests/contracts/`, `tests/runtime/`, or `tests/fleet/` per topic |
| `src/blob/api.rs`, `src/{sql,kv,queue,cron,effects}/api.rs`, `src/workflow/api.rs`, `src/workflow/activity_api.rs` | 14 | Move when the envelope types are root-reachable; otherwise allow-list |
| `src/node/tests.rs` and the inline modules in `src/{node_log,node_log_recovery,node_log_shipper,node_durability}.rs` | 68 | Attempt `tests/fleet/`; allow-list the private table/limit constants |
| `src/{follower,queue,retention,backup,worker,blob,activity_pool}/tests.rs` | 42 | Attempt the matching suite; allow-list or expose via `test_support` |
| `src/client/tests.rs` | 4 | Keep in `src/`: tests the private transport/encoding seam |
| `src/executor.rs`, `src/{actor,publication}.rs`, `src/registry/descriptor.rs` | 6 | Keep in `src/`: private state/projection constants |
| `src/coordination.rs` | 32 | Keep in `src/`: `pub(crate)` kernel required by `scripts/check-boundaries.py` |
| `src/coordination_sim.rs` | 11 | Move to `src/coordination/sim.rs`, declared by `coordination.rs` |
| `src/effects_tests.rs` | 8 | Move to `src/effects/tests.rs`; try `tests/primitives/effects.rs` first |
| `src/bin/qualification_receipt.rs` | 11 | Keep in the binary; a `bin` target cannot be imported by `tests/` |
| `crab-cell-app/src/lib.rs` | 7 | Move to `crab-cell-app/tests/application/main.rs`; no visibility change |
| `crab-cell-host/src/lib.rs` | 33 | Move to `crab-cell-host/tests/node/main.rs`; no visibility change |
| `crab-ltx/src/format_tests.rs` | 7 | Move to `src/ltx/format_tests.rs`, declared by `ltx.rs` |
| Other `crab-ltx/src/**` unit tests | 45 | Keep in `src/`: they test private codec/page/VFS mechanics |

Gate: the same `--list` parity as stage 2 for `crab-cell-runtime`,
`crab-cell-app`, `crab-cell-host`, and `crab-ltx`, plus
`cargo clippy --all-targets --locked -- -D warnings` per crate with its feature
set (`--features test-support` for runtime/app, none for host, `--features
replica` for ltx). The allow-list must contain exactly the remaining in-src
locations; the stage-5 checker fails on any unlisted occurrence.

## Stage 4 — Split oversized files

Split target (existing seams only, `pub use` in `lib.rs` unchanged). Files land
directly in their "Target module tree" location so stage 6 does not move them a
second time — for example `node.rs` splits into `node/`, not into a temporary
`node_*` set:

- `crates/crab-cell-runtime/src/actor.rs` (4,801) →
  `actor/{admission,task,message,transfer}.rs`; `handle_task` (~705 lines) and
  `handle_message` (~425 lines) move out with an `ActorState` bundle instead of
  10-parameter free functions.
- `crates/crab-cell-runtime/src/qualification.rs` (5,557) →
  `qualification/{profile,case,workload,receipt,matrix,executor}.rs`.
- `crates/crab-cell-runtime/src/node.rs` (2,933) →
  `node/{advertisement,directory,capacity,failure_domain}.rs`.
- `crates/crab-cell-runtime/src/registry.rs` (2,132) →
  `registry/{builder,module,operation}.rs` (joins `registry/descriptor.rs`).
- `crates/crab-ltx/src/environment.rs` (2,192, 117 `cfg(feature = "replica")`
  gates) → `environment/{host,resources,directory_cache,executor,telemetry}.rs`,
  with `#[cfg(feature = "replica")]` on module declarations instead of items.
- `crates/crab-cell-host/src/lib.rs` (2,459) →
  `{builder,node,durability,task_group,status}.rs`.

Gate: per-crate tests unchanged, `cargo fmt --all --check`, and
`git diff --numstat` reviewed — non-test production lines should not grow.

## Stage 5 — Policy, guides, and the layout checker

1. Add `crates/crab-cell-runtime/AGENTS.md` and `crates/crab-ltx/AGENTS.md`
   (+ `CLAUDE.md` symlink) following plan 001's shape: purpose, read-first
   routes, common changes, invariants, features, verification, the test-bucket
   rule, and the allow-list rationale.
2. Add `tests-allow-list.txt` per crate (path + one-line reason) and a README
   "test map" table mapping each contract to its suite.
3. Add `crab/scripts/check-cell-ltx-layout.py` and a Makefile target:
   - no `#[path]` under these four crates;
   - every `#[cfg(test)]` module in `src/` is listed in `tests-allow-list.txt`;
   - no `tests/<name>.rs` alongside `src/<name>.rs`;
   - every suite directory has `main.rs` and at least one `#[test]`;
   - every `test-support`-gated target appears in the CI workflow with the
     feature enabled (a `required-features` target that no job runs is a bug).
4. Update `crates/crab-cell-runtime/docs/delivery.md`'s implementation map to
   the new suite paths (it currently cites the non-existent `tests/effects.rs`).
5. Wire the checker and `cargo test -p crab-cell-runtime --features
   test-support --locked` into `.github/workflows/cell-runtime-qualification-contract.yml`.

Gate: checker green; CI green on the PR head.

## Stage 6 — Public module tree (Option A, decided 2026-09-22)

`crab-cell-runtime/src/lib.rs` re-exports 458 names from 47 statements while
every module stays private, so consumers cannot tell which module owns a type —
and Cellule would publish that shape. Stage 6 makes "Target module tree" the
real API in three commits on a second PR, after stages 1–5 have landed.

### 6a. Introduce the tree without breaking consumers

- Move each file to its target directory (`src/primitives/`, `src/node/`,
  `src/recovery/`, `src/fleet/`, `src/cell/`, …) and declare the modules; keep
  the existing flat `pub use` list unchanged in this commit.
- Split `src/lib.rs` into module declarations plus `api-prelude.rs` holding the
  root re-exports.
- Gate: `cargo check --workspace --all-targets --locked` passes with **zero**
  consumer edits. If a consumer edit is required, the tree is wrong — fix it
  here rather than in 6b.

### 6b. Migrate consumers to module paths

Reference counts at planning time:

| Consumer | `crab_cell_runtime::` refs | Files | Notes |
| --- | --- | --- | --- |
| `crab-http-server` | 1,096 | 68 | `src/cells/router.rs` 103, `src/cells/repository.rs` 92, `src/metrics.rs` 90 |
| `crab-cell-app` | 51 | `src/lib.rs` 8, tests 43 | 4 `crab_ltx::` refs, all in tests (dev-dependency) |
| `crab-cell-host` | 51 | `src/lib.rs` | single file |
| `crab` (CLI) | 0 | — | untouched |

- Rewrite imports to the narrowest module path
  (`crab_cell_runtime::primitives::kv::KvModule`,
  `crab_cell_runtime::node::log::DurabilityGate`, …).
- Replace the `ReplicaHost`/`ReplicaLimits` aliases with
  `crab_cell_runtime::ltx::{Host, Limits}` and the `crab_ltx` re-export block
  with `crab_cell_runtime::ltx::…`; do not add a `crab-ltx` dependency to
  `crab-cell-host`.
- Production code must not name `crab_ltx::` directly; test targets may keep
  the dev-dependency path where the boundary checker exempts dev-dependencies.
- Do not add root re-exports to avoid edits. A name with no sensible module
  path is a tree defect; fix it in 6a and re-run.
- Keep import rewrites mechanical: one commit per consumer crate, no logic
  edits, `git diff --numstat` reviewed per commit.

### 6c. Delete the flat surface and freeze the prelude

- Reduce the root to the frozen prelude and add
  `crates/crab-cell-runtime/api-prelude.txt` listing every remaining root
  re-export, one name per line.
- Extend the stage-5 checker: root `pub use` names must equal
  `api-prelude.txt` exactly, and no `pub(crate)` item may appear in the
  prelude.
- Update `crates/crab-cell-runtime/README.md` (create it; the crate has none),
  `crates/crab-cell-runtime/docs/rust-api.md`, and both crate guides with the
  module map and one import example per public module.

Gate: `cargo check --workspace --all-targets --locked`,
`cargo test --workspace --locked`,
`RUSTDOCFLAGS='-D warnings' cargo doc -p crab-cell-runtime -p crab-cell-app
-p crab-cell-host --no-deps --locked`, the layout checker, and a diff review
confirming that no persisted identifier, hash domain, or schema name changed.

## Verification

Every compilation uses a checkout-specific target directory, for example:

```sh
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-layout \
  cargo test -p crab-cell-runtime --features test-support --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-layout \
  cargo test -p crab-cell-app -p crab-cell-host --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-layout \
  cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-layout \
  cargo clippy -p crab-cell-runtime -p crab-cell-app --all-targets \
  --features test-support --locked -- -D warnings
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-layout \
  cargo clippy -p crab-cell-host --all-targets --locked -- -D warnings
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-layout \
  cargo clippy -p crab-ltx --all-targets --features replica --locked -- -D warnings
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-layout \
  cargo fmt --all --check
RUSTDOCFLAGS='-D warnings' \
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-cell-layout \
  cargo doc -p crab-cell-runtime -p crab-cell-app -p crab-cell-host --no-deps --locked
python3 crab/scripts/check-cell-ltx-layout.py
git diff --check
```

Record in the PR: baseline vs final test counts per crate (473/115/20/33),
`cargo test -- --list` name parity, binary-count reduction, and the
`git diff --numstat` summary. For stage 6, also record the root re-export count
before (47 statements / 458 names) and after the prelude freeze. If the mounted
volume is unavailable, stop and report rather than using a local `target/`.

## Done criteria

- [ ] No `#[path]` attributes in the four crates; `process_store` is a gated
      `test_support` module and `crab-cell-app` carries no file-include dev-deps.
- [ ] `crab-cell-app` and `crab-cell-host` have zero `#[cfg(test)]` modules in
      `src/`.
- [ ] `crab-cell-runtime` and `crab-ltx` remaining in-src tests are exactly the
      allow-list, each with a named private item.
- [ ] Suites are capability-named with `main.rs` roots, one shared
      `tests/support/`, and no duplicated `fence_session`/`fixture` helpers.
- [ ] Oversized files are split at existing seams with no public API change.
- [ ] Same test names and counts as the baseline for every crate.
- [ ] Both crate guides, the allow-lists, the test map, and `docs/delivery.md`
      match the tree; the layout checker and CI pass.
- [ ] Stage 6: every public item lives in its `Target module tree` module, both
      crates' root surfaces equal their `api-prelude.txt`, consumers import
      module paths instead of aliases, and strict rustdoc passes for
      `crab-cell-runtime`, `crab-cell-app`, and `crab-cell-host`.
- [ ] No persisted contract, format, error variant, or feature behavior changed.

## Stop conditions

- A move requires changing behavior, widening a `pub(crate)` item to `pub`, or
  touching a persisted contract: stop that move, record it as follow-up.
- A test name or count drops without an explicit, reviewed deletion: stop and
  reconcile against the baseline list.
- `crab-ltx`'s upstream-adapted files cannot be relocated without breaking the
  `UPSTREAM.md` review story: leave them and record the exception.
- The external Cargo target volume is unavailable: stop and report.
- Stage 6 requires a persisted identifier, hash domain, or schema name to
  change: stop and split the decision into its own reviewed change.

## Cellule handoff

After the Crab PR merges, the Cellule synthesis is a rename-mapped copy, not an
overwrite: `crab_cell_runtime` → `cellule_runtime`, `crab_cell_app` →
`cellule_app`, `crab_cell_host` → `cellule_host`, `crab_ltx` → `cellule_ltx`,
`crab-storage` → `cellule-store`, `crab.*.v1` strings unchanged. Preserve the
hardening Cellule already carries (ambiguous peer and migration replies as
unknown outcomes, bounded host shutdown wait, typed workflow activity events)
with a three-way merge, then update `docs/synthesis.md` with the mapped
revision and any new deliberate adaptation.

Mirror the layout rules in Cellule's `scripts/check-boundaries.py` (or a sibling
`check-layout.py`) so the two repositories cannot drift back to the current
shape, and keep `crab-cell-runtime`'s two missing crate guides as the template
for Cellule's per-crate guides.

Stage 6's module tree is inherited verbatim: `cellule-runtime` gets the same
`identity`/`control`/`registry`/`cell`/`client`/`primitives`/`publication`/
`follower`/`node`/`recovery`/`fleet`/`peer`/`qualification` layout, the same
private kernel, and the same frozen prelude rule (`api-prelude.txt` becomes
`cellule-runtime`'s list). `cellule-ltx` keeps the crate-local unit tests
described in stage 3.

## How the disposition column was measured

For each in-src test file: collect every capitalized identifier it references,
subtract names it defines locally, intersect with names declared anywhere in
the crate's `src/`, and subtract names reachable from `src/lib.rs` (`pub use`
targets and `pub mod` names). The remainder approximates "needs a private item".
The estimate over-counts generic names (`Future`, `Item`, `Key`, `Output`,
`Input`, `TABLE`, `ID`) and test-local `static` definitions, which is why the
`DESCRIPTOR` hits for `crab-cell-app` and `crab-cell-host` resolve to zero
private references. The executor confirms each file by attempting the move —
the checker, not this table, is the source of truth after stage 5.

## Maintenance

Update this plan's status to TODO, IN PROGRESS, DONE, or BLOCKED with the exact
failed gate as stages land. Once stage 5 is green, `tests-allow-list.txt` and
the crate guides own the rules; this plan becomes history.

## Implementation record (2026-09-23)

Landed on `codex/cell-ltx-layout-reorganization`:

- Stage 1: `src/test_support.rs` behind `test-support`; four `#[path]` includes
  deleted; `crab-cell-app` and `crab-http-server` enable the feature through
  dev-dependencies; `fs4` dropped from `crab-cell-app`.
- Stage 2: six runtime suites (`runtime`, `primitives`, `protocol`,
  `contracts`, `fleet`, `qualification`), plus `cell`/`ltx`/`host` in
  `crab-ltx`, `reference_application`/`contracts` in `crab-cell-app`, and
  `node` in `crab-cell-host`; one canonical `fence_session`; 473 runtime tests
  preserved.
- Stage 3: application, authority, node-log transport, pressure, cron API,
  workflow activity codec, backup pin, and release-progress tests moved to
  `tests/`; `release_progress` now uses public accessors. Everything that
  asserts private state stays in `src/` and is listed in the allow-lists.
- Stage 5: crate guides and `CLAUDE.md` symlinks for the two crates that lacked
  them, crate README, allow-lists, `crab/scripts/check-cell-ltx-layout.py`
  (also a `cell-ltx-layout-check` make target), CI steps, and the refreshed
  evidence map in `docs/delivery.md`.
- Stage 6: subsystem module tree with 15 public modules; consumers import
  module paths; the root keeps a 59-name prelude frozen in
  `crates/crab-cell-runtime/api-prelude.txt`; `crab_ltx` types are reached
  through `crab_cell_runtime::ltx`.

Deviations from the plan text, all deliberate:

- Suite roots are `tests/<suite>.rs` with modules in `tests/<suite>/` instead of
  `tests/<suite>/main.rs`. Crate-root file semantics would otherwise force a
  `#[path]` attribute for the shared `tests/support/` harness; the chosen form
  keeps every suite `#[path]`-free.
- Root re-exports stay in `lib.rs` because `pub use` inside a submodule would
  change every path; `api-prelude.txt` is the frozen list the checker compares
  against.
- Fixtures used by exactly one suite stay in that suite; only the shared
  `fence_session` lives in `tests/support/`.
- `tests/blob_cron.rs` was not split: its single owner-loss test covers Blob and
  Cron in one scenario.
- `crab-cell-app` keeps its application-builder tests in `src/` (allow-listed):
  they drive the private registry validation path.
- Stage 4 still owes `src/cell/actor.rs` and `src/qualification.rs` splits;
  both compile and are covered by the suites above.
