# crab-workflow

`crab-workflow` is Crab's workflow language, DAG planner, lockfile, status,
cache, and execution runtime. It turns a strict `crab.yaml` document into
validated stages and deterministic plans, then supplies the state and
execution contracts used by the product CLI.

## Why it exists

Reproducible data work needs more than running shell commands. Dependencies
must produce a deterministic graph, outputs must not collide, stage hashes
must account for inputs and policy, and users need a durable explanation of
why a stage is up to date or outdated. This crate makes those decisions
explicit and shareable across local runs, experiments, remote cache, and
status reporting.

## Architecture

```text
crab.yaml
   │ strict parse, template expansion, foreach/matrix
   ▼
Workflow + Stage / Dep / Out contracts
   │ path inference, duplicate-output and cycle checks
   ▼
Graph (deterministic topological order)
   │ stage hashing and persisted resolution
   ▼
crab.lock + stage cache + run journal
   │
   ├── status: up-to-date / outdated / not-run / frozen
   ├── scheduler and executor
   └── experiments, queue, DVC migration, watch mode
```

The parser rejects unknown keys and invalid stage names early. `Graph::build`
rejects duplicate outputs, undefined explicit stage outputs, and cycles before
anything executes. Lockfiles use schema version 1 and the `crab.stage.v1`
hash algorithm, save atomically, and preserve deterministic stage/input
resolution. Status helpers expose stable exit codes for scripts.

Feature gates keep optional integrations narrow: `watch` adds filesystem
watching, `gix-facade` adds the Git facade, `testing` exposes test helpers, and
`crash-injection` enables failure testing.

## Usage

Parse a workflow and inspect its execution order:

```rust
use crab_workflow::{parse_yaml, Graph};

let workflow = parse_yaml(
    r#"
stages:
  prepare:
    cmd: "python prepare.py"
    outs: ["data/prepared.txt"]
  train:
    cmd: "python train.py"
    deps: ["data/prepared.txt"]
    outs: ["model.bin"]
"#,
)?;

let graph = Graph::build(&workflow.stages)?;
assert_eq!(graph.toposort().len(), 2);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Use `crab-workflow` for parsing, planning, hashing, and state contracts. The
product command owns user-facing repository discovery and chooses when to
invoke the executor, scheduler, or status renderer.

## Boundaries

- [`crab-types`](../crab-types/README.md) owns shared stage hashes and
  cross-crate serialized contracts.
- [`crab-storage`](../crab-storage/README.md) owns remote object access; the
  workflow cache uses it but does not redefine storage semantics.
- [`crab-git`](../crab-git/README.md) supplies Git object/ref mechanics; this
  crate owns stage dependency semantics and execution state.
