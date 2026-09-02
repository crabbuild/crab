---
name: crab-workflow
description: Build, run, inspect, reproduce, and maintain Crab content-addressed workflows, experiments, external data sources, and immutable artifacts. Use for workflow files, stages, run/repro, caches, lockfiles, journals, DAGs, data, artifacts, parameters, metrics, plots, experiments, queues, and cache push.
---

# Crab workflow

Treat the workflow graph, stage identity, lockfile, journal, and cache as one
execution contract. A cache hit is valid only when the identity and outputs
match the requested stage.

## Workflow model

1. Discover the intended workflow definition, project settings, lockfile mode,
   and selected targets. Do not silently choose a nested or alternate file.
2. Stage identity includes the declared command, dependencies, parameters,
   relevant environment, and declared outputs. Use the canonical identity
   implementation; do not recreate it in a wrapper or script.
3. The DAG closure determines execution order. Lockfiles resolve dependency
   versions and journals distinguish pending, running, completed, failed, and
   stale runs.
4. Execute only selected stages and their required dependencies. Preserve
   cancellation and journal transitions when a command fails.
5. Verify every declared output and the recorded metadata before reporting a
   successful run or cache hit.

## Command scope

- `run` is the canonical executor; `repro` is its compatible spelling.
- `stage add/list` edits or inspects declarations.
- `freeze/unfreeze` controls eligibility, not stage definition.
- `status --workflow`, `workflow status`, `workflow dag`, and workflow journal
  commands explain graph state. Use dependency expansion and why-style output
  rather than guessing from timestamps.
- `workflow lockfile resolve/split` manages lockfile structure and merge
  conflicts.
- `exp` creates and inspects isolated experiment runs; `queue` manages batch
  workers and cleanup. Each run uses a temporary worktree; inspect `show` or
  `diff` before `apply` or `promote`; save, rename, transfer, removal, cleanup,
  and GC are separate metadata lifecycles.
- `data list/status/import/import-url/import-db/update` manages materialized
  external inputs plus credential-free source descriptors. Imports require a
  new safe target; update is transactional and has a read-only dry run.
- `artifacts list/show/get/version create/promote/history` manages declared
  workflow outputs. Versions and payloads are immutable and content-addressed;
  promotion is a compare-and-swap stage-label move. Retrieval verifies bytes
  and never overwrites an existing destination.
- `params`, `metrics`, and `plots` read or compare recorded workflow data.
- `migrate from-dvc` converts pipeline declarations. Preview generated output
  and preserve unsupported semantics instead of dropping them.
- Workflow cache push publishes missing terminal stage entries; verify remote
  identity before treating the cache as durable.

## Safety and concurrency

- Never mark a stage cached before outputs and metadata are durable.
- Keep lock acquisition and journal transitions paired on all exit paths.
- Do not delete experiment metadata or temporary worktrees that another queue
  worker may still own.
- Separate workflow output hydration from workflow execution; a pointer output
  is not a materialized output.
- A remote artifact registry is canonical when a Crab remote exists; local
  registry state is a cache/recovery mirror. Do not delete old remote artifact
  versions: automatic remote artifact GC is not yet a supported lifecycle.
- Keep environment capture deterministic and redact secrets from identity,
  logs, and reports.

## Verification

Use a tiny deterministic graph to prove cache miss → execution → cache hit.
Change one dependency or parameter and prove invalidation. Exercise a failed
stage and cancellation, then inspect the journal for a recoverable state. For
remote cache behavior, publish one entry, use a fresh checkout, and verify the
same stage identity and output bytes.
