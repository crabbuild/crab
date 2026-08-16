---
name: crab-workflow
description: Build, run, inspect, reproduce, and maintain Crab content-addressed workflows and experiments. Use whenever a user mentions `crab.yaml`, workflow stages, `crab run` or `repro`, stage caches, lockfiles, journals, DAGs, parameters, metrics, plots, experiments, queues, or workflow cache push.
compatibility: Crab CLI with the workflow feature enabled and a repository-local workflow definition.
---

# Crab workflow

Own the workflow graph and its content-addressed execution state. Treat stage
identity, dependency order, lockfiles, journals, and cache entries as one
contract rather than independent convenience features.

## Command scope

`run`, `repro`, `stage`, `freeze`, `unfreeze`, `exp`, `queue`, `workflow`,
`params`, `metrics`, `plots`, `status --workflow`, `migrate from-dvc`, and
workflow-cache operations under `optimize`.

Route ordinary file hydration of workflow outputs to `crab-large-files` and
remote Git/ref synchronization to `crab-git-sync`.

## Workflow model

1. Discover the applicable `crab.yaml` or `*.workflow.yaml`, project config,
   lockfile mode, and selected targets. Do not silently choose a different
   nested workflow.
2. Compute or inspect stage identity from command, dependencies, parameters,
   environment, and declared outputs as implemented by the workflow crate.
3. Use the lockfile to resolve dependency versions and the journal to explain
   in-flight, completed, failed, or stale runs. A stale stage is not the same
   as a missing output.
4. Run only the selected DAG closure. Preserve cancellation and journal
   transitions if a stage fails; do not mark a cache hit optimistically.
5. Verify outputs, metadata, and cache state. If remote cache is involved,
   prove the remote entry or use `crab workflow push-cache --all` as the
   documented batch path.

## Command guidance

- `crab run` is the canonical executor; `crab repro` is its DVC-compatible
  spelling. Keep their semantics aligned.
- `stage add/list` edits or inspects declarations; `freeze/unfreeze` changes
  execution eligibility, not the stage definition.
- `status --workflow`, `workflow status`, `workflow dag`, and `workflow
  journal` are inspection paths. Use `--why`, dependency expansion, cloud
  comparison, and journal inspection to explain state instead of guessing.
- `workflow lockfile resolve/split` is for lockfile structure and merge
  conflicts. Read the selected lockfile mode before writing files.
- `exp` creates isolated experiment runs and metadata; `queue` controls batch
  execution workers. Keep experiment cleanup separate from repository data.
- `params`, `metrics`, and `plots` compare or render recorded workflow data;
  do not treat rendered output as a source-of-truth mutation unless the command
  explicitly says so.
- `migrate from-dvc` is a conversion boundary. Preview the generated workflow
  and preserve semantics that Crab does not support rather than dropping them.

## Verification

Use a tiny deterministic stage graph to prove cache miss → execution → cache
hit, dependency invalidation, lock/journal state, and failure recovery. For
remote cache or Crab refs, use the RustFS E2E skill. Read
`crates/crab-workflow/README.md` and the relevant source before changing the
stage identity or serialized metadata contract.

## Read first

- `crab/docs/guides/workflow.md`
- `crab/docs/guides/hermetic-workflows.md`
- `crab/docs/workflow/`
- `crab/docs/design/vs-dvc-workflow.md`
- `crab/src/cmd/{run,stage,freeze,exp,exp_queue,workflow,status_workflow,workflow_journal,workflow_lockfile,params,metrics}.rs`
- `crates/crab-workflow/README.md`
- `.codex/skills/crab-cli-core/references/contracts.md`
