# Crab Workflow Layer

Content-addressed caching and deterministic replay for arbitrary commands,
built on top of Crab's existing xorb storage and ref CAS.

This guide covers the workflow layer end-to-end: what it is, when to reach
for it, the file formats, every subcommand, and the operational behavior
you need to trust it in CI. If you have ever wanted `make` to be honest
about its dependencies or wanted a DVC-shaped pipeline without a separate
server, this is the layer.

## Table of Contents

- [Crab Workflow Layer](#crab-workflow-layer)
  - [Table of Contents](#table-of-contents)
  - [When to use it](#when-to-use-it)
  - [Mental model](#mental-model)
  - [Quickstart: single stage](#quickstart-single-stage)
    - [Plan-only](#plan-only)
    - [Bypass the cache](#bypass-the-cache)
  - [Quickstart: multi-stage DAG](#quickstart-multi-stage-dag)
  - [The `crab.yaml` schema](#the-crabyaml-schema)
    - [Top-level keys](#top-level-keys)
    - [Stage keys](#stage-keys)
    - [`cmd` forms](#cmd-forms)
    - [Stage name rules](#stage-name-rules)
    - [Dep types](#dep-types)
  - [Splitting workflows across files](#splitting-workflows-across-files)
    - [When to split](#when-to-split)
    - [File layout](#file-layout)
    - [Enabling recursive discovery](#enabling-recursive-discovery)
    - [Single vs split lockfiles](#single-vs-split-lockfiles)
    - [Cross-file stage deps](#cross-file-stage-deps)
    - [Prefix collisions](#prefix-collisions)
    - [Migrating from single to split](#migrating-from-single-to-split)
    - [Example: two-team ML repo](#example-two-team-ml-repo)
  - [The lockfile](#the-lockfile)
    - [Canonical form](#canonical-form)
    - [Orphan entries](#orphan-entries)
    - [Merge conflicts](#merge-conflicts)
  - [Command reference](#command-reference)
    - [`crab run`](#crab-run)
      - [Synopsis](#synopsis)
      - [Key options](#key-options)
      - [Examples](#examples)
    - [`crab status --workflow`](#crab-status---workflow)
    - [`crab workflow dag`](#crab-workflow-dag)
    - [`crab params`](#crab-params)
    - [`crab metrics`](#crab-metrics)
    - [`crab exp`](#crab-exp)
    - [`crab workflow journal`](#crab-workflow-journal)
    - [`crab workflow lockfile resolve`](#crab-workflow-lockfile-resolve)
    - [`crab workflow lockfile split`](#crab-workflow-lockfile-split)
    - [`crab workflow push-cache`](#crab-workflow-push-cache)
  - [Stage lifecycle and crash recovery](#stage-lifecycle-and-crash-recovery)
    - [The 13 states](#the-13-states)
    - [Resume](#resume)
    - [Manual recovery](#manual-recovery)
    - [The commit point](#the-commit-point)
  - [Cache semantics](#cache-semantics)
    - [Where the stage cache lives](#where-the-stage-cache-lives)
    - [Is my stage cache uploaded to the remote?](#is-my-stage-cache-uploaded-to-the-remote)
    - [Hit path (atomic)](#hit-path-atomic)
    - [Miss path (direct write)](#miss-path-direct-write)
    - [Overwrite rules](#overwrite-rules)
    - [`--cache-only`](#--cache-only)
    - [`--cache-push` and replication](#--cache-push-and-replication)
    - [GC integration](#gc-integration)
  - [Hermeticity and environment](#hermeticity-and-environment)
    - [`hermetic: true`](#hermetic-true)
    - [Params in `cmd`](#params-in-cmd)
  - [Retries and timeouts](#retries-and-timeouts)
  - [Side effects](#side-effects)
  - [Experiments](#experiments)
    - [Running](#running)
    - [Comparing and promoting](#comparing-and-promoting)
    - [Sort order](#sort-order)
    - [GC](#gc)
    - [Dedup](#dedup)
  - [CI recipes](#ci-recipes)
    - [Reproduce a commit's outputs](#reproduce-a-commits-outputs)
    - [Post params/metrics diff on PRs](#post-paramsmetrics-diff-on-prs)
    - [Run with cache push](#run-with-cache-push)
    - [Parallel CI jobs](#parallel-ci-jobs)
  - [Exit codes](#exit-codes)
  - [Structured output](#structured-output)
    - [`crab run --json`](#crab-run---json)
    - [`crab run --jsonl`](#crab-run---jsonl)
  - [Troubleshooting](#troubleshooting)
    - ["Stage keeps missing the cache on identical inputs"](#stage-keeps-missing-the-cache-on-identical-inputs)
    - ["crab run exits with 5 (lock conflict)"](#crab-run-exits-with-5-lock-conflict)
    - ["Journal says Running but no process is running"](#journal-says-running-but-no-process-is-running)
    - ["Merge conflict in crab.lock"](#merge-conflict-in-crablock)
    - ["Cache hit won't overwrite my file"](#cache-hit-wont-overwrite-my-file)
    - ["Side-effect stage didn't fire on CI"](#side-effect-stage-didnt-fire-on-ci)
  - [Limits and gotchas](#limits-and-gotchas)
  - [Related commands](#related-commands)

## When to use it

Reach for the workflow layer when any of these apply:

- A command takes more than a few seconds and its inputs change less often
  than you re-run it.
- CI re-runs the same build, preprocessing, or analysis every push and you
  want reproducible skipping, not a best-effort tool cache.
- You want parameter and metric diffs in a PR review, not "ok looks fine
  in the screenshots."
- You iterate on parameters (learning rates, thresholds, window sizes)
  and want to compare runs without polluting branch history.
- You need another machine to reproduce byte-identical outputs from a
  commit without re-running expensive stages.

If you just need to store large files in git, `crab add` and
`crab hydrate` are enough. The workflow layer is additive — a repo
that ignores it keeps working exactly as before.

## Mental model

A **stage** is a deterministic function:

```
(deps, params, env, cmd) → outs
```

Crab hashes the left-hand side into a `stage_hash`. If that hash
already points to a cached entry, Crab materializes the cached outputs,
metrics, and plots, then skips execution. If not, it runs the command,
hashes those artifacts, stores them as xorbs, writes a cache entry, and
records the hashes in the lockfile.

Four things flow through the system:

- `crab.yaml` and/or `*.workflow.yaml` — what stages exist (your
  declaration, **git-tracked**). See
  [Splitting workflows across files](#splitting-workflows-across-files)
  for the multi-file layout.
- `crab.lock` and/or `*.workflow.lock` — what ran, what it produced
  (auto-generated, **git-tracked**, canonically serialized).
- Stage cache — content-addressed entries plus the output bytes they
  point to. Managed entirely by Crab, **never tracked by git**.
  Locally they live under `.crab/cache/stages/` (gitignored).
  Uploaded to the configured object store only when you opt in via
  `crab run --cache-push` or `crab workflow push-cache`.
  See [Where the stage cache lives](#where-the-stage-cache-lives).
- Run journal — `.crab/workflow/runs/<run_id>/journal.db`, SQLite,
  not git-tracked, not uploaded. Drives crash recovery on the
  machine where the run happened.

Everything else is plumbing around those four artifacts.

## Quickstart: single stage

You do not need a `crab.yaml` to try it. Start with one command:

```bash
crab run \
    --name clean \
    --deps data/raw.csv \
    --deps src/clean.py \
    --outs data/clean.parquet \
    -- python src/clean.py --in data/raw.csv --out data/clean.parquet
```

First run: Crab hashes the deps, runs `python`, hashes the output,
stores it as a xorb, writes `crab.lock`.

```bash
crab run \
    --name clean \
    --deps data/raw.csv \
    --deps src/clean.py \
    --outs data/clean.parquet \
    -- python src/clean.py --in data/raw.csv --out data/clean.parquet
```

Second run: Crab recognizes the `stage_hash`, skips `python`, and
reports a cache hit in under a hundred milliseconds. Touch `src/clean.py`
and the third run is a cache miss again.

### Author the stage

```bash
crab stage add -n clean \
    -d data/raw.csv \
    -d src/clean.py \
    -p model.lr,model.epochs \
    -o data/clean.parquet \
    --desc "Prepare cleaned training data" \
    python src/clean.py --in data/raw.csv --out data/clean.parquet

crab stage add -n train \
    -d data/clean.parquet \
    -m metrics.json \
    --plots plots/roc.csv \
    -o models/model.pkl \
    python src/train.py --metrics metrics.json --plot plots/roc.csv
```

`crab stage add` is the DVC-style authoring helper for `crab.yaml`. It accepts
the common `dvc stage add` flags for deps, params, cached and non-cached outs,
metrics, plots, `wdir`, descriptions, overwrite with `--force`, and `--run`.
Crab validates the workflow graph before saving so duplicate outputs and cycles
are rejected at authoring time. Use `-O /shared/model.pkl`,
`-O file:///shared/model.pkl`, or `-O s3://bucket/model.pkl` for DVC-style
external outputs that should be tracked in `crab.lock` but not cached.

### Plan-only

```bash
crab run --name clean --deps data/raw.csv --outs data/clean.parquet \
    --dry-run -- python src/clean.py
crab run --dry
crab exp run --dry -S model.lr=0.001
```

Inline runs print the computed stage hash and cache-hit/miss decision.
`crab.yaml` DAG runs print the selected stage commands as a
`workflow.dag_plan` JSON/JSONL event, or as text log lines. Dry runs exit
without executing commands, writing `crab.lock`, or persisting experiment
metadata.

### Bypass the cache

```bash
crab run --force ...
crab run --force-downstream train
```

`--force` bypasses run-cache lookup for stages selected by the invocation.
`--force-downstream` keeps normal cache behavior until a stage executes, then
forces its downstream consumers to execute too. Fresh outputs are still written
as cache entries, so the next unforced run can reuse them.
`--no-commit` is the DVC-compatible exploration mode: stages execute and
`crab.lock` is updated, but fresh run-cache entries and output xorbs are not
written.

## Quickstart: multi-stage DAG

Drop a `crab.yaml` at the repo root:

```yaml
stages:
  clean:
    cmd: "python src/clean.py --in data/raw.csv --out data/clean.parquet"
    deps:
      - data/raw.csv
      - src/clean.py
    outs:
      - data/clean.parquet

  train:
    cmd:
      argv: ["python", "src/train.py"]
    deps:
      - data/clean.parquet
      - src/train.py
    params:
      - model.lr
      - model.epochs
    outs:
      - models/model.pkl
    metrics:
      - metrics/train.json
    env:
      - CUDA_VISIBLE_DEVICES
    retry:
      max_attempts: 3
      on_signals: [9]        # OOM-kill retry
    timeout: 6h

  report:
    cmd: "python src/report.py"
    deps:
      - data/clean.parquet
      - models/model.pkl
    outs:
      - reports/summary.html
    plots:
      - metrics/roc.csv
```

Run it:

```bash
crab run
```

Crab parses `crab.yaml`, infers edges from matching paths
(`data/clean.parquet` connects `clean` → `train` and `clean` → `report`),
topologically orders the stages, and executes only the stale ones.

Run a single stage by name:

```bash
crab run train
```

Crab verifies upstream stages are up to date against `crab.lock` and
runs only `train` if they are.

## The `crab.yaml` schema

Minimal, opinionated. No Jinja, no includes, no tags. DVC-style `${...}`
substitution is supported from top-level `vars:` and params files, including
YAML, JSON, TOML, and Python literal params files referenced from `vars:`. `vars`
entries can import whole params files or selected keys with either
`params.yaml:clean,feats` or `params.yaml: [clean, feats]`; nested maps merge
recursively across vars sources. When `params.yaml` exists beside `crab.yaml`,
Crab loads it first for template resolution even if it is not listed under
top-level `params:`.
`foreach:` and `matrix:` entries may also use `${...}` references to params-file
lists or dictionaries, matching DVC sweep definitions.

### Top-level keys

| Key | Type | Description |
|-----|------|-------------|
| `artifacts` | map | Preserved and validated as non-executable catalog metadata; local `crab artifacts` lifecycle commands operate on immutable versions and CAS labels |
| `params` | list of paths | Param files to expose to `crab params` and additional template resolution |
| `metrics` | list of paths | Metric files to expose to `crab metrics` |
| `plots` | list of paths or structured plot configs | Plot files, directories, or DVC-style plot IDs for `crab plots show` and `crab plots diff`; accepts multi-source `x`/`y` mappings and renders CSV, TSV, JSON/YAML object arrays, including nested arrays, plus JPEG/GIF/PNG/SVG images |
| `defaults` | map | Default `env`, `retry`, `timeout` for every stage |
| `stages` | map | Named stages; the core content |

Structured plot entries can name axes and titles:

```yaml
plots:
  - metrics/loss.csv:
      x: epoch
      y: [train_loss, val_loss]
      x_label: Epoch
      y_label: Loss
      title: Training loss
  - metrics/raw.csv:
      x: "0"
      y: ["2"]
      no_header: true
      x_label: Epoch
      y_label: Accuracy
  - train_val_test:
      x: epoch
      y:
        metrics/train.csv: [train_loss, val_loss]
        metrics/test.csv: test_loss
      title: Train vs test loss
  - confusion:
      x:
        plots/actual.csv: actual_class
      y:
        plots/preds.csv: predicted_class
      template: confusion
```

Render the current plot data as a terminal preview, Vega-Lite spec, or
browser-ready HTML. Image plot targets are embedded directly in HTML reports:

```bash
crab plots show
crab plots show --show-vega --out plots.vl.json
crab plots show --format html --output plots.html
crab plots show train_val_test --show-vega --out train-val-test.vl.json
crab plots show confusion --format html --output confusion.html
crab plots show metrics/loss.csv --x epoch --y val_loss --format html --open
crab plots show metrics/raw.csv --no-header -x 0 -y 2 --x-label Epoch --y-label Accuracy --show-vega --out raw.vl.json
crab plots show metrics/loss.csv --html-template .dvc/plots/mypage.html --out plots.html
crab plots show plots/confusion.svg --format html --output image-plots.html
crab plots show plots/images --format html --output image-dashboard.html
crab plots templates
mkdir -p .crab/plots
crab plots templates smooth > .crab/plots/my-smooth.json
```

Passing target files plots those files directly, even if they are not declared
in `crab.yaml`. Passing a declared DVC-style plot ID, such as
`train_val_test`, expands to every source file under that plot. DVC mappings
where `x` and `y` point to different files are paired by row index, so
confusion-style plots can keep actual labels and predicted labels in separate
CSV/JSON/YAML files. `--x`, repeated `--y`, `--no-header`, `--x-label`,
`--y-label`, `--title`, `--template`, and `--html-template` override the
declared plot metadata for that invocation. `--open` writes HTML to
`crab_plots/index.html` when no `--output` path is provided.
`crab metrics plot` is the same renderer under the metrics command group.

`--template` accepts built-in names (`linear`, `simple`, `scatter`, `smooth`,
`confusion`, `confusion_normalized`, `bar_horizontal`,
`bar_horizontal_sorted`), explicit JSON template paths, or names resolved from
`.crab/plots/<name>.json` and `.dvc/plots/<name>.json`. Use
`crab plots templates` to list built-in and local templates, or
`crab plots templates <name>` to print a Vega-Lite JSON template that can be
customized. DVC-style Vega-Lite anchors such as
`<DVC_METRIC_DATA>`, `<DVC_METRIC_X>`, and `<DVC_METRIC_Y>` are replaced during
rendering.

`--html-template` is a separate DVC-compatible wrapper around the generated
plots. The HTML file must contain a `{plot_divs}` marker, which Crab replaces
with the rendered plot containers and embed script. This is useful for offline
reports that load Vega, Vega-Lite, and Vega-Embed from local script files.

CSV, TSV, JSON, and YAML plot directories are expanded into one chart per data
file. JPEG, GIF, PNG, and SVG files are rendered as image panels; image
directories are expanded recursively. Vega output is only available for
data-series targets.

When migrating DVC projects that use DVCLive, modern DVCLive-generated
`params:`, `metrics:`, and `plots:` declarations work as ordinary Crab
declarations. Legacy stage-level `live:` sections are converted to a cached
DVCLive directory output plus `<live-dir>/metrics.json`; explicit DVCLive
`plots:` entries are preserved when they already exist.

Compare two refs as an overlaid plot report:

```bash
crab plots diff --format html --output plots.html
crab plots diff --targets metrics/loss.csv --show-vega --out plots.vl.json -- main candidate experiment
crab plots diff --baseline main --target candidate --format html --output plots.html
```

Without refs, `crab plots diff` compares `HEAD` against the current workspace.
With `--baseline <ref>` and no `--target`, it compares that ref against the
workspace. DVC-style `--targets <path>... -- <rev> [rev ...]` syntax is also
accepted for multi-revision overlays.

### Stage keys

| Key | Type | Required | Description |
|-----|------|:--------:|-------------|
| `cmd` | string, list, or `argv: [...]` | Yes | Command to execute (see below) |
| `deps` | list | No | Paths, stage outs, URLs, `crab://`, `git://`, OCI refs |
| `outs` | list of paths, DVC path-key maps, or `{path, kind, cache, push, persist, max_bytes}` | No | Files or directories this stage produces |
| `params` | list of dotted keys | No | Scalar param keys read at run time |
| `env` | `inherit` \| `allowlist: [...]` \| `empty` \| list of vars | No | Env policy (see [Hermeticity](#hermeticity-and-environment)) |
| `metrics` | list of paths or DVC path-key maps | No | Metric files produced by this stage; structured entries accept DVC output settings such as `cache`, `persist`, and `push`, and metric hashes are recorded in `crab.lock` |
| `plots` | list of paths or structured plot configs | No | Plot files produced by this stage; accepts DVC multi-source `x`/`y` mappings, stores the referenced source paths, and records plot hashes in `crab.lock` |
| `nondeterministic` / `always_changed` | bool | No | Always execute this stage instead of replaying run cache |
| `hermetic` | bool | No | Fail if command reads undeclared paths (Phase 5) |
| `side_effects` | bool | No | Command has external side effects (API calls, notifications) |
| `on_cache_hit` | string or argv | No | Only with `side_effects: true`; fired on hit |
| `retry` | map | No | See [Retries and timeouts](#retries-and-timeouts) |
| `timeout` | duration (`30s`, `6h`) | No | SIGTERM → SIGKILL escalation |
| `persist` | bool | No | Do not delete declared outs before running |

When `wdir` is set, the stage command runs in `repo_root/wdir`, and relative
`deps`, `outs`, and stage-local params are interpreted from that directory.
Bare stage param refs default to `wdir/params.yaml`, and file-scoped stage
param refs such as `custom.yaml: [model.lr]` resolve to `wdir/custom.yaml`.
`crab.lock` stores path-bearing entries with the `wdir/` prefix so the lockfile
remains repo-relative and unambiguous.

`always_changed: true` is accepted as a DVC-compatible alias for Crab's native
`nondeterministic: true`. Either form makes the stage stale in workflow status,
skips local and remote run-cache lookup, and executes every run. Stages with no
declared deps and no declared outs are also treated as always changed, matching
DVC `repro`.

`frozen: true` skips a stage even when its inputs change and even when
`crab run --force` is used. Use `crab freeze <stage>` and
`crab unfreeze <stage>` to toggle the field from the command line; DVC-style
path-qualified targets such as `models/dvc.yaml:train` are accepted.

Output settings can use Crab's explicit `path:` map or DVC's path-key form:

```yaml
outs:
  - path: models/model.pkl
    cache: true
  - reports/metrics.json:
      cache: false
      persist: true
```

Output-level `cache: false` matches DVC's run-cache rule: if any declared output
disables caching, Crab still hashes the output into `crab.lock`, but it does not
read, write, pull, or push a stage-cache entry for that stage. Output-level
`push: false` keeps local cache reuse but suppresses remote stage-cache
publication for the whole stage because remote entries are stage-complete.
Output-level `persist: true` keeps that output in place before the command runs.
Crab accepts DVC output `desc:` metadata. Output-level `remote:` names route
artifact bytes through matching workflow remotes while the stage manifest and
remote cache ref stay on the repository's Crab remote:

Absolute local output paths, `file://` output URLs, HTTP(S) output URLs, and
object-store output URLs such as `s3://`, `gs://`, `az://`, and `azure://` are
treated as DVC-style external outputs. They are hashed into `crab.lock` for
change detection but must be non-cached (`cache: false`); raw external outs
default to `cache: false` and `push: false`.

```toml
[workflow.remotes.models]
url = "crab://ml-cache/models"
```

```yaml
outs:
  - models/model.pkl:
      remote: models
```

### `cmd` forms

Three forms are accepted:

```yaml
# argv form: executed directly, no shell. Safer.
cmd:
  argv: ["python", "src/train.py", "--epochs", "50"]

# shell form: runs via `sh -c "<string>"`. Works with pipes and redirects.
cmd: "python src/train.py --epochs 50 | tee logs/train.log"

# DVC-compatible shell list: runs each command in order and stops on failure.
cmd:
  - mkdir -p logs
  - python src/train.py --epochs 50
  - python src/evaluate.py
```

The list form runs each entry in a fresh shell with the same working directory
and environment, stopping at the first non-zero exit. `argv`, `shell`, and shell
lists hash differently on purpose. `argv(["python","a"])`,
`shell("python a")`, and `shells(["python a"])` are not interchangeable and
Crab refuses to pretend they are.

### Stage name rules

Names must match `^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$`. ASCII letters, digits,
underscore, hyphen; start with letter or underscore; 1–64 chars.
Unicode names, slashes, colons, and whitespace are rejected at parse
time with `WorkflowStageNameInvalid`.

### Dep types

```yaml
deps:
  - data/raw.csv                                    # path
  - "https://example.com/latest.csv"                # DVC-style URL string
  - "remote://datasets/raw.csv"                     # alias via [workflow.remotes.datasets]
  - stage_out:                                      # another stage's output
      stage: clean
      out: data/clean.parquet
  - url:
      url: "https://example.com/x.tar"
      digest: "b3:abc..."                           # pinned URL
  - crab:
      repo: "bucket/repo"
      rev: "v1.0"
      path: "models/base.pkl"                       # cross-repo
  - git:
      url: "https://github.com/org/repo"
      rev: "main"
      path: "src/util.py"                           # external git
  - oci:
      reference: "ghcr.io/org/img"
      digest: "sha256:..."                          # OCI image (Phase 5)
```

DVC-style URL strings in `deps:` are accepted for `http://`, `https://`,
`s3://`, `s3a://`, `gs://`, `az://`, `azure://`, `abfs://`, `abfss://`,
`adl://`, `file://`, `ssh://`, `sftp://`, `hdfs://`, `webhdfs://`, and
`remote://` schemes. HTTP(S), `file://`, S3, GCS, and Azure URL deps are
fetched/read and hashed as live external deps. `remote://name/path` aliases
expand through `[workflow.remotes.<name>].url`; aliases are live-hashed when
the expanded URL uses one of those supported backends. Pinned URL deps with a
`b3:<64-hex>` digest participate in the stage hash without network access only
after provider preflight. SSH, SFTP, HDFS, WebHDFS, WebDAV, Drive, and OSS
schemes are rejected before execution until a live provider is compiled and
qualified; a digest does not bypass that capability check.
Directory deps are allowed. Crab hashes them as a canonical tree
manifest (sorted paths, NFC normalization, `.gitignore`-filtered).
Symlinks, FIFOs, device files, and sockets are rejected.

## Splitting workflows across files

Small repos live happily with a single `crab.yaml` at the root. Larger
repos — monorepos, ML training pipelines, teams that split concerns —
can spread stages across multiple `*.workflow.yaml` files, each
alongside its own lockfile.

### When to split

Reach for split workflows when any of these apply:

- Two or more teams edit the workflow independently and collide on
  `crab.lock` in every PR.
- The workflow file has grown past a few hundred lines and navigation
  is painful.
- You run workflows in parallel CI jobs and want independent lockfile
  state per job.

Single-file users: the default mode is unchanged. You do not need to
opt in, and nothing about the section below affects a `crab.yaml`
+ `crab.lock` repo.

### File layout

```
repo/
├── crab.yaml              # shared defaults, params, root stages (optional)
├── crab.lock              # lock for root-level stages only
├── train.workflow.yaml      # training pipeline
├── train.workflow.lock      # lock for train.* stages
├── eval.workflow.yaml       # evaluation pipeline
├── eval.workflow.lock       # lock for eval.* stages
└── pipelines/
    ├── deploy.workflow.yaml # nested: prefix becomes pipelines.deploy.*
    └── deploy.workflow.lock
```

File-name rules:

- `crab.yaml` — the root file. Stages inside it keep their
  declared names with no prefix.
- `<name>.workflow.yaml` — a named workflow file. Every stage inside
  is prefixed with `<name>.` when merged with other files. So a stage
  named `preprocess` inside `train.workflow.yaml` becomes
  `train.preprocess` in the merged DAG.
- `<dir>/crab.yaml` — a nested workflow. Stages are prefixed with
  the directory path joined by dots.
- `<dir>/<name>.workflow.yaml` — prefix combines both, e.g.
  `pipelines/deploy.workflow.yaml` yields `pipelines.deploy.*`.

### Enabling recursive discovery

Split layouts require recursive discovery so Crab walks the tree
for yaml files. Either set it per invocation:

```bash
crab run --recursive
```

or flip it in config for all workflow commands:

```bash
crab config set workflow.enabled true
crab config set workflow.discover recursive
```

which writes:

```toml
# .crab/local.toml
[workflow]
enabled = true
discover = "recursive"
```

Root-only discovery (the default) rejects `*.workflow.yaml` files at
parse time with `WorkflowDiscoveryAmbiguous` — this is deliberate so
partial migrations cannot silently run only a subset of stages.

### Single vs split lockfiles

Workflow YAML files and lockfiles are configured independently. You
choose each based on what your repo needs:

| Lockfile mode | Layout | When to pick it |
|---|---|---|
| `single` (default) | One `crab.lock` at the repo root with every stage | Small repos, one team, simple PRs |
| `split` | One `<name>.workflow.lock` next to each `<name>.workflow.yaml` | Multi-team repos, frequent merge conflicts |

Flip the mode in config:

```bash
crab config set workflow.lockfile split
```

which writes:

```toml
[workflow]
enabled = true
discover = "recursive"
lockfile = "split"
```

or override per-invocation via env:

```bash
CRAB_WORKFLOW_LOCKFILE=split crab run
```

Split mode writes lockfiles atomically per workflow file, so two CI
jobs running `train.workflow.yaml` and `eval.workflow.yaml`
concurrently never touch the same lockfile.

### Cross-file stage deps

A stage in `eval.workflow.yaml` can depend on the output of a stage
in `train.workflow.yaml` via a plain path dep:

```yaml
# eval.workflow.yaml
stages:
  evaluate:
    cmd: "python scripts/eval.py"
    deps:
      - models/model.pkl      # produced by train.train
    outs:
      - reports/metrics.json
```

The DAG builder stitches the edge by matching `deps` paths against
`outs` paths across every merged workflow file. No special
cross-file syntax is needed.

### Prefix collisions

Two sources cannot claim the same prefix. If you have both
`train/crab.yaml` (the nested-directory form) and
`train.workflow.yaml` (the filename form), Crab refuses the run
with a message listing both candidates — the fix is to rename or
delete one.

### Migrating from single to split

An existing `crab.lock` migrates to per-file lockfiles in one
command:

```bash
# Preview what would happen:
crab workflow lockfile split --dry-run

# Commit to the migration and flip config:
crab workflow lockfile split --update-config
```

`--update-config` appends (or edits) `[workflow] lockfile = "split"`
in `.crab/local.toml` so the next `crab run` uses the new layout
automatically. Without `--update-config`, the split files are written
but subsequent runs stay on `single` mode and will recreate
`crab.lock` alongside the new per-file lockfiles — the `--keep`
flag is the right companion in that case.

Key safety properties:

- Empty lockfile → migration is a no-op (nothing gets deleted).
- Stages with no provenance (e.g., declared in `crab.yaml` but
  never run after a rename) land in the root `crab.lock` bucket.
- Failed migration mid-write leaves the monolithic file in place so
  you can retry.
- `--keep` preserves the monolithic `crab.lock` after the split
  for mixed repos that declare some stages in `crab.yaml` and
  others in `*.workflow.yaml`.

After migration, all existing cache entries still resolve — stage
hashes are identical across both lockfile layouts, only the file
they live in changes.

### Example: two-team ML repo

```yaml
# crab.yaml — shared root defaults
defaults:
  env:
    - CUDA_VISIBLE_DEVICES
    - PYTHONPATH
  retry:
    max_attempts: 2
params:
  - params.yaml

stages:
  bootstrap:
    cmd: "./scripts/setup.sh"
    outs:
      - .venv/.done
```

```yaml
# train.workflow.yaml — data team
stages:
  preprocess:
    cmd: "python -m src.preprocess"
    deps:
      - data/raw.csv
      - src/preprocess.py
    outs:
      - data/clean.parquet

  train:
    cmd: "python -m src.train"
    deps:
      - data/clean.parquet
      - src/train.py
    params:
      - model.lr
      - model.epochs
    outs:
      - models/model.pkl
    metrics:
      - metrics/train.json
```

```yaml
# eval.workflow.yaml — evaluation team
stages:
  evaluate:
    cmd: "python -m src.evaluate"
    deps:
      - models/model.pkl           # produced by train.train
      - data/clean.parquet         # produced by train.preprocess
    outs:
      - reports/metrics.json

  report:
    cmd: "python -m src.report"
    deps:
      - reports/metrics.json
    outs:
      - reports/summary.html
```

A single `crab run --recursive` runs five stages across three files
in dependency order: `bootstrap` → `train.preprocess` → `train.train`
→ `eval.evaluate` → `eval.report`. Each team sees PR diffs only in
their own lockfile.

## The lockfile

The lockfile is committed alongside the workflow yaml. Two Crab builds
produce byte-identical lock files for equivalent inputs, so diffs in PRs
are meaningful.

The on-disk layout follows the lockfile mode set in config:

- `lockfile = "single"` (default): one `crab.lock` at the repo root
  containing every stage.
- `lockfile = "split"`: one `<name>.workflow.lock` per
  `<name>.workflow.yaml`, plus `crab.lock` for stages declared in
  the root `crab.yaml`. See
  [Splitting workflows across files](#splitting-workflows-across-files).

Byte format, merge behavior, and orphan handling are identical between
the two layouts — the only difference is how stages are partitioned
across files.

### Canonical form

- Block-style scalars only; no flow-style `{}` or `[]` at top level.
- Double-quoted strings everywhere (avoids YAML 1.1 `yes`/`no`/`3.14`
  coercion surprises).
- Sorted keys at every map level.
- UTF-8 NFC normalization on all strings.
- Hashes as `"b3:" + 64 lowercase hex`.
- Unix modes as quoted octal: `"0o644"`.
- No anchors, aliases, tags, or multi-document streams.

Example stage block:

```yaml
schema_version: 1
crab_hash_algo: "crab.stage.v3"
stages:
  report:
    stage_hash: "b3:abc123..."
    cmd:
      shell: "python src/report.py"
    deps:
      - path: "data/clean.parquet"
        hash: "b3:def456..."
        size: 2048576
      - path: "src/report.py"
        hash: "b3:789abc..."
        size: 1842
    params:
      threshold: 0.95
    outs:
      - path: "reports/summary.html"
        kind: "file"
        hash: "b3:fed321..."
        size: 54321
        mode: "0o644"
    metrics:
      - path: "reports/metrics.json"
        hash: "b3:111222..."
    executed_at: "2026-04-27T14:23:11.083Z"
    duration_ms: 12543
    host_fingerprint: "linux-x86_64-crab-0.8.0"
    attempts: 1
```

### Orphan entries

Stages that exist in `crab.lock` but not in `crab.yaml` are pruned
on the next atomic rewrite, with a warning. The run does not fail.

### Merge conflicts

When a `git merge` conflicts `crab.lock`, use:

```bash
crab workflow lockfile resolve               # recompute (default)
crab workflow lockfile resolve --ours
crab workflow lockfile resolve --theirs
crab workflow lockfile resolve --recompute
```

`--recompute` rehashes every conflicted stage from the working tree and
rewrites deterministically. The output is byte-identical regardless of
which side ran it, so two devs resolving the same conflict independently
produce the same lock file.

## Command reference

### `crab run` / `crab repro`

Execute a single stage or the DAG. `crab repro` is the DVC-compatible alias
for the same runner.

#### Synopsis

```
crab run [OPTIONS] [STAGE]
crab repro [OPTIONS] [STAGE]   # DVC-compatible alias for crab run
crab run [OPTIONS] --cmd <CMD> --name <NAME> [--deps <PATH-OR-URL>]... [--outs <PATH>]...
```

#### Key options

| Option | Description |
|--------|-------------|
| `--cmd <cmd>` | Single-stage mode without `crab.yaml` |
| `--name <n>` | Stage name for single-stage mode |
| `--deps <path-or-url>` | Declare a dep (repeatable); DVC-style HTTP(S), file, S3, GCS, and Azure URL deps are live-hashed |
| `--outs <path>` | Declare an out (repeatable) |
| `--params <key>` | Declare a param key (repeatable) |
| `--env <var>` | Allowlist an env var (repeatable) |
| `--force` | Ignore cache, re-execute |
| `--force-downstream` | After any stage executes, force downstream consumers to execute |
| `--dry-run` / `--dry` | Print inline stage hash or DAG plan, exit |
| `-i` / `--interactive` | Ask before executing each stage that would otherwise run |
| `--cache-only` | Replay from cache; fail on miss with exit 3 |
| `--no-run-cache` | Execute commands even if a matching run-cache entry exists |
| `--no-commit` | Execute and update lockfile without writing fresh cache entries |
| `--cache-push` | Upload new cache entries after run |
| `--pull-cache` | Consult remote on local miss |
| `--no-overwrite` | Cache hit must not overwrite existing files |
| `--keep-going` | On failure, keep running unrelated branches |
| `--ignore-errors` | Continue all remaining stages |
| `--explain-miss <stage>` | Field-by-field diff of why a stage missed |
| `--resume-trust-outputs` | Trust crashed-mid-run outputs on resume |
| `--abandon <run_id>` | Mark a stuck journal `Aborted` |
| `--no-wait` | Fail fast if scheduler lock is held |
| `--lock-timeout <sec>` | Wait up to N seconds for scheduler lock (default 600) |
| `--json` / `--jsonl` | Structured output |
| `--recursive` / `-R` | Discover nested `crab.yaml` files |
| `[STAGE...]` | Run target stage(s) plus upstream deps, matching `dvc repro <stage>`; accepts canonical dotted names and DVC-style `path/to/dvc.yaml:stage` aliases. Use `crab repro` when you want the DVC spelling; it is the same executor as `crab run` |
| `--single-item` / `-s` | Run only target stage(s) |
| `--downstream` | Run target stage(s) and downstream consumers |
| `--pipeline` / `-p` | Run the connected pipeline containing target stage(s) |
| `--all-pipelines` / `-P` | Discover and run every pipeline under the repo root |
| `--glob` | Treat positional targets as stage-name glob patterns |

List stages with the DVC-style `stage list` helper when you want a quick
inventory without opening `crab.yaml`:

```bash
crab stage list
crab stage list --all
crab stage list -R pipelines/
crab stage list --name-only models/dvc.yaml:train
```

Freeze stages with the DVC-style commands when you want `crab run`,
`crab status --workflow`, and `crab workflow status` to treat them as unchanged
until further notice:

```bash
crab freeze train
crab freeze models/dvc.yaml:train
crab unfreeze train
```

#### Examples

```bash
# Single stage, no yaml
crab run --name clean --deps data/raw.csv --outs data/clean.parquet \
    -- python src/clean.py

crab run --name fetch --deps s3://bucket/raw.csv --outs data/raw.csv \
    -- aws s3 cp s3://bucket/raw.csv data/raw.csv

# Full DAG
crab run

# Preview the DAG plan without executing or writing crab.lock
crab run --dry

# Target stage plus upstream dependencies
crab run train
crab repro train

# Only one stage, using existing lockfile/worktree deps
crab repro --single-item train

# Target plus downstream consumers
crab run --downstream preprocess
crab run --force-downstream train

# Ask before executing each stale stage
crab run -i train

# Entire connected pipeline containing a target
crab run --pipeline evaluate

# DVC-style target glob
crab run --glob 'train_*'

# Every nested crab.yaml under the repo root
crab run --all-pipelines
crab run -R pipelines.train
crab run -R pipelines/dvc.yaml:train

# Reproduce a commit exactly, fail if any stage is missing from cache
crab run --cache-only

# Ignore existing run-cache entries and execute commands
crab run --no-run-cache train

# Execute and update crab.lock without committing outputs to the cache
crab repro --no-commit train

# Run locally, then push the new cache entries
crab run --cache-push

# Keep going after failures on unrelated branches
crab run --keep-going

# Explain why a stage missed the cache
crab run --explain-miss train
```

### `crab status --workflow`

Per-stage state: up-to-date, stale, never-run, or in-flight.

```bash
crab status --workflow
crab status --workflow train
crab status --workflow --with-deps model.pt
crab status --workflow --why train       # field-by-field miss diff
crab status --workflow --json
```

Stale stages are further attributed: stale due to dep change, param
change, env change, cmd change, or missing outputs. `--why` produces the
same field-by-field breakdown `--explain-miss` does.

### `crab workflow dag`

Render the DAG.

```bash
crab workflow dag                         # ASCII stage DAG
crab workflow dag train                   # target plus upstream stages
crab workflow dag --full train            # whole connected pipeline
crab workflow dag -o                      # dependency/output DAG
crab workflow dag --mermaid               # Mermaid graph TD
crab workflow dag --md                    # fenced Markdown Mermaid block
crab workflow dag --dot                   # Graphviz DOT
crab workflow dag --collapse-foreach-matrix
crab workflow dag --json                  # workflow.dag envelope
```

### `crab params`

Read and diff parameters across refs. Works even without the stage
runner — if you have a `params.yaml`, this layer is useful on day one.

```bash
crab params show                          # HEAD
crab params show --ref feature-branch
crab params diff                          # HEAD vs workspace
crab params diff main                     # main vs workspace
crab params diff --targets conf/model.yaml params.yaml -- main feature-branch
crab params diff --deps                   # only stage dependency params
crab params diff --no-path --md
crab params diff --all --json             # include unchanged param keys
crab params diff main HEAD --format pr-comment
```

Without explicit targets, `params diff` reads `params.yaml`, workflow-level
params, and stage-level param dependencies declared in `crab.yaml`. Use
DVC-style `--targets <path>... --` before revisions for ad-hoc params files.
Params are compared by path and dotted key, so `model.lr` in two files remains
two distinct rows; `--no-path` hides the path column only in human-readable
output.

Supported param formats: YAML, JSON, TOML, Python. Keys flatten to dotted
notation (`model.lr`). Values are typed scalars (null, bool, int, float,
string, list, map).

### `crab metrics`

Same shape as `params`. Numeric diffs include absolute and percent
delta.

```bash
crab metrics show                         # workspace metrics
crab metrics show metrics/eval.json
crab metrics show -R metrics              # recursive directory target
crab metrics show -aT --md                # workspace + branches + tags
crab metrics show --all-commits --json
crab metrics diff                         # HEAD vs workspace
crab metrics diff main                    # main vs workspace
crab metrics diff --targets metrics.json -- main feature-branch
crab metrics diff -R --targets metrics -- main feature-branch
crab metrics diff main feature-branch --format pr-comment
crab metrics diff main feature-branch --md
crab metrics diff --no-path --precision 3
crab metrics diff --all --json            # include unchanged metric keys
```

Without explicit paths, `metrics show` and `metrics diff` read metrics declared
in `crab.yaml`, including stage metrics under their `wdir`, and fall back to
`metrics.json` when no workflow metrics are declared. `metrics show` accepts
DVC-style positional targets and history selectors (`-a/--all-branches`,
`-T/--all-tags`, `-A/--all-commits`). Use DVC-style
`--targets <path>... --` before diff revisions for ad-hoc metric files; add
`-R/--recursive` when a target is a directory. Metrics are compared by path and
key, so `accuracy` in two metric files remains two distinct rows.

### `crab exp`

Experiments are transient runs on a hidden ref namespace. See
[Experiments](#experiments) for the full model.

```bash
crab exp run --set-param model.lr=0.001 --set-param model.epochs=50
crab exp run --set-param model.lr=0.001 --name lr-sweep-1
crab exp run --set-param model.lr=0.001 --message "try smaller learning rate"
crab config set workflow.enabled true
crab config set hydra.enabled true
crab exp run -S train/model=efficientnet -S train.optimizer.lr=0.02
crab exp run --dry -S model.lr=0.001
crab exp run -R pipelines.train -S model.lr=0.001
crab exp run -i train -S model.lr=0.001
crab exp run --downstream train -S model.lr=0.001
crab exp run --force-downstream train -S model.lr=0.001
crab exp run -C secrets.env -S model.lr=0.001
crab exp run --queue -S model.lr=0.001,0.01 --name lr-sweep --message "lr grid"
crab exp run --run-all -j 2             # DVC-style alias for exp start
crab queue status                       # DVC-style queue management
crab queue logs <task>
crab queue kill --force <task>
crab queue stop --kill
crab queue remove --success
crab exp clean                           # DVC-style temp/queue housekeeping
crab exp show                            # recent experiments
crab exp show --all --num 10
crab exp show --md --sort-by model.lr --sort-order asc
crab exp show --md --only-changed --drop seed --keep model.lr
crab exp show --csv --precision 3
crab exp show <id>                       # full metadata for one experiment
crab exp ls --json
crab exp list --limit 10                 # DVC-style alias for exp ls
crab exp diff <id_a> <id_b>
crab exp diff <id_a> <id_b> --md --precision 3
crab exp promote <id> -b winning-experiment
crab exp branch <id> winning-experiment  # DVC-style alias for promote
crab exp save --name manual-snapshot --message "manual checkpoint"
crab exp save -R models/dvc.yaml --name model-only
crab exp rename <id> readable-name
crab exp apply <winner_id>
crab exp remove <id_or_name_a> <id_or_name_b>
crab exp remove --keep <winner_id_or_name>
crab exp remove --all --dry-run
crab exp remove -g origin <remote_id_or_name>
crab exp clean
crab exp gc --keep 50
crab exp gc --keep 50 --dry-run
```

### `crab workflow journal`

Inspect and manage the per-run journal under
`.crab/workflow/runs/`.

```bash
crab workflow journal ls
crab workflow journal ls --outcome failed --json
crab workflow journal show <run_id>          # full trajectory
crab workflow journal gc --keep 50           # default
```

### `crab workflow lockfile resolve`

Resolve a merge-conflicted `crab.lock`. See
[Merge conflicts](#merge-conflicts).

### `crab workflow lockfile split`

One-shot migration from a monolithic `crab.lock` to per-workflow
lockfiles (`<name>.workflow.lock`). See
[Migrating from single to split](#migrating-from-single-to-split) for
the full context.

```bash
crab workflow lockfile split --dry-run          # preview without writing
crab workflow lockfile split                    # migrate, leave config alone
crab workflow lockfile split --update-config    # migrate + flip config to split
crab workflow lockfile split --keep             # keep the monolithic file after split
crab workflow lockfile split --json             # structured output
```

Options:

| Option | Description |
|--------|-------------|
| `--dry-run` | Print the partition plan and exit. No files written. |
| `--keep` | Preserve `crab.lock` after writing the split files. Useful for mixed repos where some stages live in `crab.yaml`. |
| `--update-config` | Also set `[workflow] lockfile = "split"` in `.crab/local.toml` so subsequent runs use the new layout automatically. |
| `--json` | Emit the `workflow.lockfile_split` envelope instead of text. |

Runs succeed idempotently: if every partition is empty (no stages have
ever been cached), the command leaves the monolithic file alone so
no cache state is lost.

### `crab workflow push-cache`

Bulk-push historical stage/experiment refs that were not pushed at run
time.

```bash
crab workflow push-cache --all
crab workflow push-cache --since origin/main
```

`crab push` does NOT automatically replicate all historical workflow
refs — only refs created in the current push cycle (plus git heads and
tags). Use `--cache-push` on `crab run` for new entries or
`push-cache` for backfill.

## Stage lifecycle and crash recovery

Every stage moves through a strict state machine. Each transition is
durably recorded in SQLite (WAL mode, `synchronous=NORMAL`) before the
next state's work starts. This is what makes crash recovery tractable.

### The 13 states

```
NotRun → Resolving → Resolved → CacheChecked
                           │           │
                     (miss) │           │ (hit — fast path)
                           ▼           ▼
                        Running      (virtual transitions)
                           │
                           ▼
                        Produced → Hashed → Staged
                           │
                           ▼
                      EntryWritten  ◄── COMMIT POINT
                           │
                           ▼
                      RefPublished → LockfileUpdated → Committed

  Failed / Aborted: reachable from any non-terminal state
```

`EntryWritten` is the commit point. Before it, work is safe to redo.
After it, the stage is "done"; remaining transitions publish
already-committed local state.

`Aborted` is distinct from `Failed`: it means Crab itself got
SIGINT/SIGTERM'd. `Failed` means the user command exited non-zero,
signaled, or timed out. Both are terminal.

### Resume

When `crab run` starts, it scans `.crab/workflow/runs/*` for
non-terminal journals and decides per stage:

| Journal state | Filesystem check | Action |
|---|---|---|
| `Resolving` | — | Discard (no durable work) |
| `Resolved`, `CacheChecked` | — | Restart from `Resolved` |
| `Running` | pid alive | Re-attach / wait for child |
| `Running` | pid dead | Clean sidecars; restart unless `--resume-trust-outputs` + outs present |
| `Produced` | outs exist, hashes match | Resume from `Hashed` |
| `Produced` | otherwise | Restart |
| `Hashed` | outs exist, hashes match | Resume from `Staged` |
| `Hashed` | otherwise | Restart |
| `Staged` | staging segments present | Resume from `EntryWritten` |
| `EntryWritten` | — | Resume from `RefPublished` |
| `RefPublished` | — | Update lockfile only |
| `LockfileUpdated` | — | Mark `Committed`, orphan cleanup |
| `Failed`, `Aborted` | — | Skip unless `--force` |

A `SIGKILL` to `crab run` at any point leaves the repo in a state
where the next invocation resumes without re-executing any stage whose
journal reached `Hashed` or later. Stages that crashed mid-command
restart cleanly from `Resolved`.

### Manual recovery

```bash
crab run --abandon <run_id>           # mark a stuck journal Aborted
crab run --resume-trust-outputs       # trust outs when pid died mid-run
crab workflow journal show <run_id>   # inspect trajectory before deciding
```

### The commit point

Only work committed at `EntryWritten` or later affects other runs.
Partial outputs written during `Running` are the user command's
responsibility; Crab cleans them up on restart via orphan sidecar
sweep.

## Cache semantics

### Where the stage cache lives

The stage cache is **not tracked by git**. Crab manages it directly.
Two separate locations hold the same content-addressed state, and
movement between them is always explicit.

| Location | Holds | Lifetime |
|---|---|---|
| `.crab/cache/stages/` | Local manifests (JSON) keyed by stage hash | Lives on the machine that produced the run; gitignored |
| `.crab/cache/xorbs/` | Local output xorbs (content-addressed by blake3) | Same machine; gitignored |
| `.crab/workflow/runs/<run_id>/` | Crash-recovery journals (SQLite) | Same machine; gitignored |
| Remote `workflow/stages/<ab>/<hex>.json` | Manifests, sharded by first hash byte | Shared across the team via object storage |
| Remote `workflow/xorbs/<hash>.xorb` | Output bytes, content-addressed | Shared across the team |
| Remote `refs/crab/stages/<hex>` | Git-style refs pinning the manifest | Shared across the team |

The first `crab run` in a repo adds `.crab/workflow/` to
`.gitignore` automatically so journals never accidentally land in a
commit. The rest of the local cache is covered by the standard
`.crab/` ignore rule that `crab init` writes.

### Is my stage cache uploaded to the remote?

Only when you ask. Crab gives you three modes:

| Mode | Who runs it | What happens |
|---|---|---|
| `crab run` (default) | Local iteration | Cache is written to `.crab/cache/stages/` only. Nothing leaves the machine. |
| `crab run --cache-push` | CI builders, shared workstations | After each stage commits, upload xorbs + manifest, then CAS a ref at `refs/crab/stages/<hex>`. Safe under concurrent pushes (conditional PUT). |
| `crab workflow push-cache --all` | Backfill | Scan every local cache entry, upload whichever are not yet on the remote. Use after forgetting `--cache-push`, or when migrating from another tool. |

On a fresh clone, `crab run --cache-only` consults the remote refs
transparently: it reads the ref, fetches the manifest, and streams
just the xorb ranges needed to materialize the declared outs. No
separate hydrate step is required — the workflow layer is
integrated with the same object store the rest of Crab uses.

**`crab push` does NOT replicate the full stage cache.** It only
pushes refs created in the current push cycle (plus git heads and
tags). Stage-cache refs ride that pipe only if they were produced
with `--cache-push` or backfilled via `workflow push-cache`.

### Hit path (atomic)

Cache-hit materialization writes via
`.crab.tmp.<run_id>` sidecar + fsync + atomic rename:

1. Fetch entry bytes (local cache → else remote via ref lookup).
2. Fetch artifact bytes (local `.crab/cache/xorbs/` → else remote xorb ranges).
3. For each cached out, metric, or plot:
   - If existing file/directory hashes match the entry, no-op.
   - If uncommitted git changes, fail unless `--force`.
   - Atomic write via sidecar on the same filesystem.
   - Restore unix mode bits from the entry.
4. If `side_effects: true` + `on_cache_hit` set, fire the hook.

### Miss path (direct write)

The user command writes directly to declared paths. A crash mid-command
leaves partial files; the next run's orphan sweep deletes them before
re-execution. Atomic semantics apply only after execution completes
(hashing, staging, entry write).

### Overwrite rules

| Situation | Default | `--no-overwrite` | `--force` |
|-----------|---------|------------------|-----------|
| File matches cache | No-op | No-op | No-op |
| File differs, no git changes | Overwrite atomically | Fail | Overwrite |
| File has uncommitted changes | Fail | Fail | Overwrite |

### `--cache-only`

Intentionally does NOT revalidate the working tree. It recomputes the
stage hash from the lockfile's recorded state and materializes outs
from the cache. Fails with exit 3 on any miss.

Use this in CI to reproduce a commit's outputs exactly:

```bash
git checkout <commit>
crab run --cache-only
```

### `--cache-push` and replication

```bash
crab run --cache-push          # push new entries produced by this run
crab workflow push-cache --all # backfill historical entries
```

Entries live at `{repo_prefix}/workflow/stages/<2-char-shard>/<full-hex>.json`
with refs `refs/crab/stages/<hex>` pointing at them. Xorbs are
content-addressed; concurrent writers from different machines produce
byte-identical entries, and ref CAS ensures exactly one becomes
authoritative.

When an output declares `remote: <name>`, its artifact xorbs live under that
named `[workflow.remotes.<name>]` URL. The manifest and stage ref remain on the
repo remote so every clone has one cache-index authority.

### GC integration

Stage cache xorbs are pinned by the live-set walker through
`refs/crab/stages/*` and `refs/crab/exp/*`. Orphaned stage refs (no
experiment pointing, no live branch) become collectible on the next
`crab gc`, subject to the existing grace period.

```bash
crab exp clean                # remove stale tempdirs and queue housekeeping
crab exp gc --keep 50         # orphan old experiments
crab gc                       # collect unreferenced xorbs
```

## Hermeticity and environment

Environment policy is declared per stage via `env`:

```yaml
# Option 1: allowlist (recommended)
env:
  - CUDA_VISIBLE_DEVICES
  - PYTHONPATH

# Option 2: empty (strictest; only PATH, HOME, TMPDIR injected)
env: empty

# Option 3: inherit (full process env; EXCLUDED from hash)
env: inherit    # default; emits warn on first use per session
```

**Only listed variables participate in the stage hash.** With
`inherit` (the default), Crab warns once per stage per session
that cache hits may be spurious if environment varies.

### `hermetic: true`

Wraps the command in the hermetic sandbox. The current enforcing backend is
macOS `sandbox-exec`; unsupported platforms fail before launching the command.
Hermetic stages may read declared `deps`, write declared `outs`, and use the
per-stage sandbox temp directory. Undeclared repository reads or writes fail
with a structured hermetic violation that names the stage and path.

Hermetic policy versioning participates in the stage hash, so a sandbox policy
change cannot reuse an older hermetic cache entry.

### Params in `cmd`

Param values interpolated via `${model.lr}` into a `Cmd::Shell` string
are shell-injection prone. Crab tokenizes the shell string first,
substitutes after, and emits a `substitution_warning` field in
structured output whenever substitution occurred. `Cmd::Argv` avoids
this entirely.

## Retries and timeouts

```yaml
stages:
  train:
    cmd: "python src/train.py"
    retry:
      max_attempts: 3
      initial_backoff: 5s
      max_backoff: 60s
      backoff_multiplier: 2.0
      on_exit_codes: [6, 137]
      on_signals: [9]           # SIGKILL — OOM retry
      on_timeout: true
    timeout: 6h
```

Rules:

- Each attempt is its own row in `stage_runs` with a distinct `attempt`
  number.
- Only a successful attempt's outputs become the cache entry.
- `on_signals` matches the **child's** termination signal (OOM-killer
  kills the child with 9). If Crab itself is SIGKILL'd, the journal
  resume path handles it, not the retry policy.
- `timeout` escalates: SIGTERM → `graceful_shutdown_timeout` (default
  10s) → SIGKILL. Each escalation is journaled.
- Storage-layer retries (xorb upload, ref CAS) happen inside each
  attempt via the existing `storage::retry` policy.
- `--force` bypasses run-cache lookup for selected stages and writes a fresh
  cache entry on success.
- `--force-downstream` bypasses run-cache lookup for descendants after an
  upstream stage has executed in the same DAG run.

## Side effects

```yaml
notify:
  cmd: "./notify.sh"
  deps: [reports/summary.html]
  side_effects: true
  on_cache_hit: "./notify.sh --resend"
```

- **Cache miss**: `cmd` runs as normal. `on_cache_hit` does NOT fire.
- **Cache hit**: outputs materialized; `on_cache_hit` fires once,
  synchronously, before `Committed`.
- **Retries**: each retry attempt fires the main `cmd` again;
  `on_cache_hit` is never invoked during retry attempts.
- **`on_cache_hit` non-zero exit**: stage transitions to `Failed`. The
  cache entry remains; retrying later hits and re-fires the hook.

Every cache hit on a `side_effects: true` stage emits a visible warning
and sets `side_effects_skipped: true` in structured output. If you want
the hook to NOT fire on every hit, remove `on_cache_hit`.

## Experiments

Experiments are transient parameter sweeps recorded under
`refs/crab/exp/<uuid>` without polluting branch history.

### Running

```bash
crab exp run --set-param model.lr=0.001 --set-param model.epochs=50
crab exp run --set-param model.lr=0.001 --name lr-sweep-1
crab exp run --set-param model.lr=0.001 --message "try smaller learning rate"
crab config set workflow.enabled true
crab config set hydra.enabled true
crab exp run -S train/model=efficientnet -S train.optimizer.lr=0.02
crab exp run --dry -S model.lr=0.001
crab exp run -R pipelines.train -S model.lr=0.001
crab exp run -i train -S model.lr=0.001
crab exp run --downstream train -S model.lr=0.001
crab exp run --force-downstream train -S model.lr=0.001
crab exp run -C secrets.env -S model.lr=0.001
crab exp run --queue -S model.lr=0.001,0.01 --name lr-sweep --message "lr grid"
crab exp run --run-all -j 2
crab queue status
crab queue logs <task>
crab queue kill --force <task>
crab queue stop --kill
crab queue remove --success
crab exp clean
```

Behavior:

1. Create a tmpdir worktree at
   `.crab/workflow/exp/<exp_id>/` (NEVER `/tmp`, NEVER the main
   worktree).
2. Copy any `-C/--copy-paths` repo-relative files or directories from the
   main workspace into the experiment worktree. This is the DVC-compatible
   path for ignored or untracked inputs such as local secrets, credentials,
   or small fixtures that are needed by a stage but should not be committed.
3. Compose Hydra config groups into `params.yaml` when
   `crab config set hydra.enabled true` has enabled `[hydra]` in
   `.crab/local.toml`.
4. Apply `--set-param` overrides, or the shorter `--set` alias, by
   writing them to the tmpdir's params files on disk. Overrides
   participate in `stage_hash` exactly as on-disk params would — no
   out-of-band channel.
5. Execute the selected target set against the tmpdir. Positional targets and
   `--single-item`, `--downstream`, `--pipeline`, `--all-pipelines`, and
   `--glob` use the same selection rules as `crab run`.
   `-i`/`--interactive` prompts before each selected stage that would execute;
   declining skips that stage for the current run and leaves existing
   workspace/lockfile state in place.
   `--dry`/`--dry-run` prints the selected stage plan and stops here without
   executing commands or recording experiment metadata.
6. Record `ExperimentMetadata`: `exp_id`, `base_commit`,
   `queue_commit`, `name`, `message`, `status`, `param_overrides`,
   `stages`, `metrics`, `cli_args`, and `host_fingerprint`.

When Hydra is enabled, Crab reads `conf/config.yaml` by default, follows its
`defaults:` list, writes the composed YAML to `params.yaml` in the experiment
worktree, then applies remaining scalar overrides. Group overrides such as
`-S train/model=efficientnet` update the defaults-list selection; scalar
overrides such as `-S train.optimizer.lr=0.02`, `+key=value`, `++key=value`,
and `~key` mutate the composed params file. Use
`crab config set hydra.config_dir settings` and
`crab config set hydra.config_name experiment.yaml` when your Hydra root is not
`conf/config.yaml`. Config-group files may use Hydra package directives such
as `# @package _global_`, `# @package _group_`, and
`# @package _group_._name_`; package overrides in the defaults list take
priority over file directives. During composition, Crab resolves nested
OmegaConf-style interpolations, including relative node references like
`${.sibling}` and `${..parent_sibling}`, plus the safe built-in resolver subset
used by common DVC Hydra projects: `${join:${dir},${file}}` and
`${oc.env:VAR,default}`, `${oc.select:key,default}`,
`${oc.decode:${oc.env:VAR}}`, `${oc.create:...}`,
`${oc.deprecated:key[,message]}`, `${oc.dict.keys:path}`, and
`${oc.dict.values:path}`.

`crab exp run --queue` is the DVC-style spelling for adding a queued
experiment without running it immediately. It accepts `-S/--set-param`, and
comma-separated choices, `choice(...)`, or Hydra-compatible stop-exclusive
`range(...)` values enqueue one experiment per Cartesian combination.
Non-queued `crab exp run -S ...` accepts scalar values only; sweep expressions
are rejected so heavy grids cannot accidentally run as one literal override.
Queued experiments also persist target selection flags, so
`crab exp run --queue --downstream train -S model.lr=0.001,0.01`
replays the same downstream target set when workers start the queue.
`--force-downstream` is stored with the queue entry as well. `-C/--copy-paths`
is stored per queue entry and replayed by workers, so
`crab exp run --queue -C secrets.env -S model.lr=0.001,0.01`
copies `secrets.env` into each queued experiment worktree before running.
`--name` and `-m/--message` are stored with each queue entry and persisted
when that task runs.
`crab exp run --run-all -j N` is the DVC-style shortcut for processing the
pending queue with `N` workers. The native spellings `crab exp queue -S ...`
and `crab exp start --jobs N` remain available. DVC-style
`crab queue start/status/stop/remove` aliases manage the same queue. Use
`crab queue logs <task>` to read a running or completed task's stage output;
`--follow` tails a running task until it completes.
Use `crab queue kill <task>` to interrupt a running task, or
`crab queue kill --force <task>` for immediate termination. Use
`crab queue stop --kill` to write the stop signal and force-kill current
running tasks, leaving pending tasks for a later start.
`crab queue remove --success` deletes completed queue task records and their
task logs but does not delete the experiment metadata; use `crab exp remove`
for that. Use `crab exp clean` after a crashed or force-killed queue worker to
remove stale experiment tmpdirs, active-run markers, kill requests, and orphaned
queue logs without touching saved experiment metadata.

### Comparing and promoting

```bash
crab exp show
crab exp show --all --num 10
crab exp show -aT --all-commits --rev HEAD --num -1 --no-pager --hide-failed
crab exp show --md --sort-by model.lr --sort-order asc
crab exp show --md --only-changed --drop seed --keep model.lr
crab exp show --csv --precision 3
crab exp show <id>
crab exp diff <id_a> <id_b>
crab exp diff <id_a> <id_b> --all --no-path
crab exp diff <id_a> <id_b> --md --precision 3 --param-deps
crab exp promote <id> -b winning-model
crab exp branch <id> winning-model
crab exp apply <winner_id>
crab exp save --name manual-snapshot --message "manual checkpoint"
crab exp save -R models/dvc.yaml --name model-only
crab exp rename <id> renamed-snapshot
crab exp push <id>
crab exp push --all
crab exp pull <id>
crab exp pull --all
crab exp remove <id_or_name_a> <id_or_name_b>
crab exp remove --keep <winner_id_or_name>
crab exp remove --all --dry-run
crab exp remove -g origin <remote_id_or_name>
```

Experiment IDs can be abbreviated to any unambiguous prefix in `show <id>`,
`diff`, `promote`, `apply`, `push`, and `remove`; `remove` also accepts exact
experiment names. `exp pull` accepts full remote experiment ids, exact remote
experiment names, or unambiguous remote prefixes, so the short prefix copied
from a shared table is enough.
`exp list` is a DVC-style alias for `exp ls`, and `exp branch` is a
DVC-style alias for `exp promote`.
List-mode `exp show` can render text, Markdown, CSV, or JSON. `--sort-by`
accepts built-in columns (`id`, `name`, `message`, `started_at`, `base_commit`,
`status`, `stages`) and captured param/metric keys such as `model.lr` or
`metrics.json:accuracy`.
`--only-changed` keeps only captured param/metric keys that vary across the
listed experiments. `--drop REGEX` removes matching param/metric keys from
the list output, and `--keep REGEX` keeps matching keys even when
`--only-changed` or `--drop` would remove them.
DVC history selectors `-a/--all-branches`, `-T/--all-tags`,
`-A/--all-commits`, `--rev COMMIT`, and `-n/--num N` are accepted; Crab's
local experiment metadata list is already independent of Git ref grouping, so
the branch/tag/rev selectors do not widen the scan. Negative `--num` values
mean "no local limit", matching DVC's "all first-parent commits" intent.
`--hide-failed` filters persisted experiments whose metadata status is
`failed`. `--hide-queued` and `--hide-workspace` are accepted no-ops because
queued tasks live under `crab queue status` and Crab does not synthesize a
workspace row in `exp show`. `--no-pager`, `--param-deps`, `--sha`, and
`--force` are accepted for DVC script compatibility; Crab prints directly,
lists captured experiment param overrides, includes base commits in the table,
and reads the local metadata files without an internal show-cache.
`exp diff --all` includes unchanged params, stage hashes, and metrics in
addition to added, removed, and changed values. `--no-path` hides `file:`
prefixes in text and Markdown output while JSON keeps canonical keys.
`--param-deps` is accepted for DVC script compatibility; Crab experiment
metadata stores the params that were actually overridden for the experiment,
so there is no broader params table to narrow. `exp diff --md` renders a
Markdown table report for PR comments, and `--precision N` controls numeric
metric formatting in text and Markdown output.
`exp rename <id> <name>` updates a local experiment label without changing
the immutable experiment id or its workspace snapshot. Duplicate labels are
rejected unless `--force` is passed. If the experiment was already shared,
run `crab exp push --force <id>` to publish the renamed metadata.
`exp apply <id>` overlays the captured experiment workspace snapshot onto
the current workspace, overwriting conflicting files and removing files that
the experiment deleted from its base commit. Files that were already present
in the workspace but did not exist in the experiment are left alone.
`exp save` captures the current workspace as an experiment without running
the DAG and persists `--name` plus `-m/--message` in the metadata. DVC-style
`-R/--recursive`, `-f/--force`, and positional targets are accepted: targets
such as `models/dvc.yaml`, `models/dvc.yaml:train`, or `models/` limit the
stage hashes and declared metrics recorded in the experiment metadata. Crab
still snapshots the whole workspace except `.git` and `.crab` for apply, so
DVC-style `-I/--include-untracked` paths are accepted as compatibility hints
rather than required for untracked files.
`exp remove` deletes local experiment metadata by full id, exact name, or
unambiguous prefix. Pending queued experiments can be removed with the same
command by their queued id prefix, matching DVC's queued-id behavior; use
`exp remove --queue` to clear all pending queued experiments without touching
completed experiment metadata. `exp remove --rev COMMIT` selects experiments
whose recorded base commit matches that baseline, and `--num N` selects
experiments from the last `N` first-parent commits starting at `--rev` or
`HEAD`; negative `--num` means every first-parent commit. `exp remove --keep`
inverts local id, name, or history selectors and keeps the selected experiments
while removing the rest. `--dry-run` previews the removal set. DVC-style
`-g/--git-remote REMOTE` removes experiments from a Crab remote name or a
direct `crab://` URL using the same object-store experiment refs that
`exp push`, `exp pull`, and `exp list` use.

`promote` and its DVC-style `branch` alias create a real branch containing
the captured experiment snapshot without switching to it. If no branch name is
supplied, Crab derives one from the experiment name or id. Experiment outputs
remain available through the stage cache while the metadata is present.

### Sharing

`crab exp push` and `crab exp pull` use the configured `crab://` object-store
remote; they do not take a separate Git remote argument. A pushed experiment
includes:

- the experiment metadata at `workflow/exp/<id>/meta.json`;
- a metadata ref at `refs/crab/exp-meta/<id>`;
- the referenced stage hash list at `workflow/exp/<id>/stage-refs.json`;
- the captured apply snapshot, including files, empty directories, symlinks,
  modes, and paths deleted by the experiment.

By default, `exp push` skips an experiment id that already exists remotely and
`exp pull` skips an experiment id that already exists locally. Pass `--force`
to replace the existing copy. Use `--all` (or the DVC-style `--all-commits`
alias) to push or pull every experiment visible in that scope.

Pulled experiments can be inspected with `crab exp show`, compared with
`crab exp diff`, and applied with `crab exp apply` without rerunning the
workflow. Stage-cache reuse remains explicit: use `crab workflow push-cache`
to publish local stage cache entries and `crab run --pull-cache` to hydrate
them before reruns.

### Sort order

`exp show` orders by UUIDv7 embedded timestamp, not wall-clock. Clock
skew across machines does not reorder experiments.

### GC

```bash
crab exp clean
crab exp gc --keep 100                 # default
crab exp gc --dry-run
```

Deletes old experiment refs. Orphaned stage refs (no experiment
pointing, no live branch) become collectible by the next `crab gc`.

### Dedup

Two experiments differing only in one param share 99%+ of stored bytes
in S3. Upstream stages (whose hash didn't change) hit the cache; only
downstream stages re-execute.

## CI recipes

### Reproduce a commit's outputs

```yaml
# .github/workflows/reproduce.yml (excerpt)
- name: Clone
  run: git clone crab://${{ secrets.BUCKET }}/${{ github.repository }} .

- name: Reproduce outputs
  run: crab run --cache-only
```

Exit 3 on any cache miss, so CI fails loudly if the commit wasn't
properly pushed with `--cache-push`.

### Post params/metrics diff on PRs

```yaml
- name: Params diff
  run: |
    crab params diff origin/${{ github.base_ref }} HEAD --format pr-comment > params.md
    crab metrics diff origin/${{ github.base_ref }} HEAD --format pr-comment > metrics.md

- name: Comment on PR
  uses: marocchino/sticky-pull-request-comment@v2
  with:
    path: params.md
```

### Run with cache push

```yaml
- run: crab run --cache-push --json > run.json
```

Structured output lands in `run.json`; use it to drive downstream
reporting without scraping stdout.

### Parallel CI jobs

The scheduler lock at `.crab/workflow/.lock` serializes `crab run`
per repo. Launch one CI job per repo; if you need parallelism within a
job, wait for Phase 5 (parallel DAG execution).

## Exit codes

| Code | Meaning |
|-----:|---------|
| 0 | Success |
| 1 | Generic failure |
| 2 | User-input / config error (bad args, malformed yaml, bad stage name) |
| 3 | Cache miss on `--cache-only` |
| 4 | Integrity error (dep hash mismatch, corrupted cache entry) |
| 5 | Lock conflict (scheduler lock held, `--no-wait`) |
| 130 | SIGINT |
| 143 | SIGTERM |

No other codes are introduced by workflow commands.

## Structured output

All workflow commands respect `--json` and `--jsonl` and emit through
the same `core/output` envelope as the rest of Crab. See
[Structured Output](structured-output.md) for the envelope contract.

### `crab run --json`

```json
{
  "schema": "workflow.run",
  "version": "1.0",
  "timestamp": "2026-04-27T14:23:23.626Z",
  "data": {
    "run_id": "0191ff00-7c8f-7fff-8fff-aaaaaaaaaaaa",
    "stages": [
      {
        "name": "clean",
        "state": "Committed",
        "source": "Cache",
        "stage_hash": "b3:abc123...",
        "duration_ms": 12,
        "attempts": 1,
        "outs": [
          {"path": "data/clean.parquet", "hash": "b3:def456...", "size": 2048576}
        ]
      },
      {
        "name": "train",
        "state": "Committed",
        "source": "Execution",
        "stage_hash": "b3:fed321...",
        "duration_ms": 87543,
        "attempts": 1,
        "outs": [
          {"path": "models/model.pkl", "hash": "b3:111222...", "size": 52428800}
        ]
      }
    ],
    "outcome": "success"
  }
}
```

### `crab run --jsonl`

Streaming events, one per line:

```
{"schema":"workflow.stage.started","version":"1.0","timestamp":"...","data":{"run_id":"...","stage":"clean","stage_hash":"b3:..."}}
{"schema":"workflow.stage.cache_hit","version":"1.0","timestamp":"...","data":{"stage":"clean","source":"Local"}}
{"schema":"workflow.stage.started","version":"1.0","timestamp":"...","data":{"run_id":"...","stage":"train","stage_hash":"b3:..."}}
{"schema":"workflow.stage.cache_miss","version":"1.0","timestamp":"...","data":{"stage":"train","reason":"dep_hash_changed","field":"data/clean.parquet"}}
{"schema":"workflow.stage.committed","version":"1.0","timestamp":"...","data":{"stage":"train","duration_ms":87543,"attempts":1}}
{"schema":"workflow.run","version":"1.0","timestamp":"...","data":{"run_id":"...","outcome":"success"}}
```

Event schemas: `workflow.run.started`, `workflow.stage.started`,
`workflow.stage.cache_hit`, `workflow.stage.cache_miss`,
`workflow.stage.committed`, `workflow.stage.failed`,
`workflow.journal.transition`, `workflow.journal.resumed`,
`workflow.exp.run.started`.

## Troubleshooting

### "Stage keeps missing the cache on identical inputs"

Almost always `env: inherit` leakage. Run:

```bash
crab run --explain-miss <stage>
```

You'll get a field-by-field diff of every input-hash component. The
culprit is usually an env var that changes between runs (`PWD`,
`SSH_AUTH_SOCK`, `TERM`, a timestamp-derived var). Switch to an
`allowlist` with only the vars that matter.

### "crab run exits with 5 (lock conflict)"

Another `crab run` is active in the same repo. The lock file at
`.crab/workflow/.lock` records the holder's pid. Options:

- Wait (default 600s).
- `--no-wait` to fail fast.
- `--lock-timeout 30` to bound the wait.

If the holder pid is dead, the lock is stale; delete the file manually.

### "Journal says Running but no process is running"

```bash
crab workflow journal show <run_id>   # inspect
crab run --abandon <run_id>           # mark Aborted
crab run                              # resume others cleanly
```

Or, if you trust the on-disk outputs:

```bash
crab run --resume-trust-outputs
```

### "Merge conflict in crab.lock"

```bash
crab workflow lockfile resolve        # default: recompute
```

Two devs resolving independently produce byte-identical results.

### "Cache hit won't overwrite my file"

You have uncommitted git changes at the target path. Either commit,
stash, or pass `--force`.

### "Side-effect stage didn't fire on CI"

It was a cache hit. Either:

- Add `on_cache_hit: "..."` to reproduce the side effect on hit.
- Remove `side_effects: true` and accept it in the hash (change any
  input to force a miss).
- Use `--force` to bypass cache for that run.

## Limits and gotchas

- **Stage count**: default cap 10 000 outs per stage
  (`max_outs_per_stage`), 1 TiB per out (`max_out_bytes`). Override in
  `.crab/local.toml` under `[workflow]`.
- **Symlinks/FIFOs/devices**: rejected as deps and outs with
  `StageDepMalformed` / `StageOutMalformed`. Regular files and
  directories only.
- **Cross-filesystem directory renames**: not atomic. Crab falls
  back to per-file atomic writes and logs a warning. Writing outs to
  a different mount (e.g., NFS) weakens the guarantee.
- **Monorepos / nested `crab.yaml` and `*.workflow.yaml`**: default
  discovery is root-only; opt into `--recursive` or
  `discover = "recursive"` to pick up nested workflow files. See
  [Splitting workflows across files](#splitting-workflows-across-files).
  Files outside that opt-in surface as `WorkflowDiscoveryAmbiguous`.
- **`--cache-only` vs drift**: does NOT re-hash the working tree.
  The intent is "reproduce the last recorded run fast," not "verify
  current state matches the lock." Use `crab run` (no flag) for
  verification.
- **Remote execution**: not implemented. Use `--remote-url` and you
  get `StageRemoteExecutionUnsupported`, NOT silent local fallthrough.
- **URL deps**: pinned URL deps with `digest: "b3:<64-hex>"` are hashed from
  the declared digest after provider preflight. DVC-style URL string deps are
  parsed as URL deps. Digest-less `http://`, `https://`, `file://`, S3, GCS,
  Azure, and `remote://` aliases backed by those URLs are fetched/read and
  hashed. SSH, SFTP, HDFS, WebHDFS, WebDAV, Drive, and OSS deps fail with a
  typed provider-capability error before a stage starts.
- **External outs**: absolute paths, `file://`, HTTP(S), S3, GCS, and Azure
  output URLs, plus `remote://` aliases backed by those URLs, are tracked when
  non-cached. Cached absolute and external URL outputs are rejected to prevent
  cache restore outside the workspace. SSH and HDFS output URLs still return
  `StageRemoteExecutionUnsupported`.
- **Feature flag**: fresh configs enable the layer by default. Set
  `crab config set workflow.enabled false` for an explicit opt-out; disabled
  commands respond with `WorkflowDisabled`. Stock commands are unaffected.
- **Env `inherit` footgun**: the default. Silent spurious cache hits
  are possible if env varies. Consider `env: allowlist` in any
  pipeline you care about.
- **Experiment ref sprawl**: default `exp gc --keep 100`. Run it
  regularly if you sweep often.
- **Journal disk use**: `.crab/workflow/runs/<run_id>/` grows with
  unfinished runs. `crab workflow journal gc --keep 50` is the
  pressure valve.

## Related commands

- [`crab add`](crab-add.md) — stage files before a run produces them.
- [`crab hydrate`](crab-hydrate.md) — materialize dep pointers before a stage runs.
- [`crab status`](crab-status.md) — working-tree state; pair with `--workflow` for stage state.
- [`crab gc`](crab-gc.md) — collect unreferenced xorbs, including stale stage entries.
- [`crab push`](crab-import.md) — replicates workflow refs created in the current push cycle.
- [Structured Output](structured-output.md) — envelope contract for `--json` / `--jsonl`.
- [Errors](crab-errors.md) — exit codes and error code lookup.
