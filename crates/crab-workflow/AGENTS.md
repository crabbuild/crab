# crab-workflow

Root `AGENTS.md` and `crates/AGENTS.md` apply. Read
`crates/crab-workflow/README.md` for crate usage. Paths below are repository-root-relative.

## Purpose and ownership

Owns workflow parsing, DAG planning, stage execution, cache/state persistence, and experiments. Product commands choose invocation and user-facing policy; the executor is one stage attempt, not the whole scheduler.

## Read first

1. `crates/crab-workflow/src/lib.rs` — public workflow and persisted contracts.
2. `crates/crab-workflow/src/yaml.rs` — `parse / validate_semantics`: strict schema parsing and expansion.
3. `crates/crab-workflow/src/graph.rs` — `Graph::build / toposort`: dependency inference and deterministic plan.
4. `crates/crab-workflow/src/scheduler.rs` — `DagScheduler`: concurrency and dependency admission.
5. `crates/crab-workflow/src/executor.rs` — `run_local`: one attempt and journal transitions.

Trace one path: `crab/src/cmd/run.rs` → `parse_at` in `crates/crab-workflow/src/yaml.rs`
→ `Graph::build` in `crates/crab-workflow/src/graph.rs` before scheduler
execution. Parsing and graph construction are sequential caller-owned steps.

## Common changes

| Task | Start here | Also inspect |
| --- | --- | --- |
| Workflow grammar | `crates/crab-workflow/src/yaml.rs` | `crab/src/cmd/run.rs` |
| Execution ordering | `crates/crab-workflow/src/scheduler.rs` | `crates/crab-workflow/src/executor.rs` |
| Persisted state/resume | `crates/crab-workflow/src/lockfile.rs` | `crates/crab-workflow/src/resume.rs` |

## Invariants

- Experiment metadata readers share `ExperimentMetadata::verify_identity` for
  requested ID and canonical hash checks. Storage prefixes remain caller-owned;
  `from_json` owns schema decoding and requested-ID checks. Listings own their
  warning/skip policy; live-set collection must fail on unknown metadata roots.
  Source: `crates/crab-workflow/src/experiment.rs` and `crab/src/cmd/exp.rs`.

- Scheduler contention is fs4's `Ok(false)` outcome, not an I/O failure.
  The guard owns the advisory lock; PID files are only diagnostics.
  `SchedulerLock::acquire` blocks its calling thread while waiting.
  Source: `crates/crab-workflow/src/scheduler_lock.rs`.

- Unknown schema keys must fail visibly; parse/expand before planning so typos cannot silently alter execution.
  Source: `crates/crab-workflow/src/yaml.rs`.
- Stage and default retry policies share range checks with semantic validation;
  reject invalid values during parsing so execution cannot bypass the checks.
  Source: `crates/crab-workflow/src/yaml.rs`.
- Reject duplicate output ownership and cycles before execution; preserve deterministic topological decisions.
  Source: `crates/crab-workflow/src/graph.rs`.
- Keep per-attempt journal transitions separate from retry and DAG scheduling; inspect resume and journal consumers before changing durable state.
  Source: `crates/crab-workflow/src/executor.rs`.

## Features and platform

Empty default. `watch` enables notify; `gix-facade` enables gix; `testing` exposes test helpers; `crash-injection` enables fault-injection paths. Use exact feature slices for changes; do not enable crash injection incidentally.

## Verification

Inline yaml tests cover unknown fields and invalid paths; graph tests include cycle/output conflicts and property checks. Execution tests can spawn real local child processes and mutate temporary fixtures.

Run from repository root. The target below is the example for worktree `089c`;
replace it with a unique directory for your checkout. Before compilation, verify
`$HOME/Workspace` resolves to the mounted workspace volume and the target is
writable. Stop if unavailable; never fall back to a local target directory.

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-workflow --locked --lib yaml
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-089c" cargo test -p crab-workflow --locked --lib graph::tests
```

These are focused checks, not full runtime qualification. Use affected-consumer
checks and dedicated CI for broader behavior; existing interface/behavior slices
are in `crab/scripts/check-crate-interface-builds.py` and
`crab/scripts/check-crate-behavior.py`.

## Related documentation

- `crates/crab-workflow/README.md` — usage and detailed contracts.
- `crates/crab-workflow/Cargo.toml` — dependency and feature authority.

Read `crates/crab-workflow/src/journal.rs`, `crates/crab-workflow/src/resume.rs`, and `crates/crab-workflow/src/lockfile.rs` together for persistence changes.

Update this guide when entry points, ownership, invariants, features, or test
routes change. Keep detailed API preconditions in rustdoc rather than copying
them into a second specification.
