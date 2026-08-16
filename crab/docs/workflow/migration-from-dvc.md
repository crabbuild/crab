# Migrating from DVC to Crab

This guide covers converting an existing DVC pipeline (`dvc.yaml`) to
Crab's workflow format (`crab.yaml`). The automated migration tool
handles most of the work; this document explains what it does, what it
can't do, and what you need to finish by hand.

## Table of Contents

- [Overview](#overview)
- [Running the migration](#running-the-migration)
- [Conversion rules](#conversion-rules)
- [Unsupported features](#unsupported-features)
- [Manual steps after migration](#manual-steps-after-migration)
- [Example: 5-stage DVC pipeline](#example-5-stage-dvc-pipeline)

## Overview

Crab's workflow engine covers DVC's core pipeline execution concepts and
extends them in several areas. Most DVC concepts map directly:

- Stages, deps, outs, params, metrics, plots — same semantics.
- `foreach` and `matrix` — same syntax.
- `vars:` and `${...}` templating — same syntax.
- `wdir:` and `frozen:` — direct equivalents; `dvc freeze` and
  `dvc unfreeze` map to `crab freeze` and `crab unfreeze`.

The migration tool reads your `dvc.yaml`, applies conversion rules, emits
a valid `crab.yaml`, and prints a report of anything that needs manual
attention.

## Running the migration

```bash
# From the repo root (where dvc.yaml lives):
crab migrate from-dvc

# From a different directory:
crab migrate from-dvc --dir path/to/project

# Print to stdout instead of writing crab.yaml:
crab migrate from-dvc --stdout
```

The command:
1. Locates `dvc.yaml` in the target directory.
2. Parses the DVC pipeline definition.
3. Converts each stage to crab format.
4. Writes `crab.yaml` (or prints to stdout).
5. Prints a migration report with stage count and warnings.

After migration, validate the result:

```bash
crab run --validate
```

## Conversion rules

| DVC field | Crab equivalent | Notes |
|-----------|-------------------|-------|
| `cmd:` (string) | `cmd:` (string) | Direct copy |
| `cmd:` (list) | `cmd:` (list) | Accepted directly; each entry runs in order in a fresh shell and migration preserves the list |
| `deps:` | `deps:` | Direct copy |
| `outs:` | `outs:` | Subfields mapped (see below); absolute local paths and `file://` outs are tracked when non-cached |
| Top-level `params:` | Top-level `params:` | Direct copy for YAML, JSON, TOML, and Python params files; root `params.yaml` is still loaded for `${...}` templates when present; `crab params diff` uses declared params by default and accepts DVC-style `--targets <path>... -- [a_rev] [b_rev]`, `--all`, `--deps`, `--json`, `--md`, and `--no-path` |
| Stage `params:` | Stage `params:` | Direct copy, including DVC-style `custom.yaml: [key]` and `custom.yaml:` forms; stage-local params remain path-scoped in `crab params diff` output |
| `metrics:` | `metrics:` | Direct copy, including stage-level DVC path-key output settings such as `cache`, `persist`, and `push`; produced metric hashes are recorded in `crab.lock`; `crab metrics show` uses declared metrics by default and accepts DVC-style targets, `-R`, `-a`, `-T`, `--all-commits`, `--json`, and `--md`; `crab metrics diff` accepts DVC-style `--targets <path>... -- [a_rev] [b_rev]`, `--all`, `--json`, and `--md` |
| `plots:` | `plots:` | Plot source paths, arbitrary plot IDs, DVC multi-source `x`/`y` mappings, cross-file `x`/`y` sources, headerless data, axis labels, custom HTML wrappers, and basic metadata are preserved; produced stage plot hashes are recorded in `crab.lock`, and plots can be rendered with `crab plots show --show-vega`, `--no-header`, `--x-label`, `--y-label`, `--html-template`, `--format html`, `--open`, or `crab plots diff` |
| `live:` | `outs:` + `metrics:` | Legacy DVCLive sections become a cached directory output plus `<live-dir>/metrics.json`; modern DVCLive-generated `params:`, `metrics:`, and `plots:` pass through normally |
| `artifacts:` | `artifacts:` | Direct copy as model/artifact metadata; ignored by workflow execution |
| `wdir:` | `wdir:` | Direct copy; stage-local `params.yaml` and file-scoped stage params remain relative to `wdir` |
| `frozen:` | `frozen:` | Direct copy; `crab freeze <stage>` and `crab unfreeze <stage>` toggle it after migration |
| `always_changed:` | `always_changed:` or `nondeterministic:` | Directly accepted; migration emits the native Crab spelling |
| `foreach:` | `foreach:` | Syntax match, including `${...}` list/dict values from params files |
| `matrix:` | `matrix:` | Syntax match, including `${...}` dimensions and composite dict/list values |
| `vars:` | `vars:` | Direct copy, including YAML, JSON, TOML, and Python literal params files, selected-key imports, and recursive map merging used as template sources |
| `${...}` | `${...}` | Same syntax; default `params.yaml` values are available without adding top-level `params:` |
| `desc:` | `desc:` | Direct copy |
| `meta:` | `meta:` | Direct copy |

### Output subfields

Crab accepts both DVC path-key output syntax and Crab's explicit `path:` map:

```yaml
outs:
  - model.pkl:
      cache: false
      persist: true
  - path: reports/metrics.json
    cache: true
```

| DVC output field | Crab equivalent |
|------------------|-------------------|
| `cache: true/false` | `cache: true/false`; `false` disables run-cache reads/writes for the stage |
| `persist: true` | `persist: true` |
| `push: false` | `push: false`; local cache is retained, remote stage-cache publication is skipped |
| `checkpoint: true` | `persist: true`; old DVC checkpoint outputs are kept across stage runs |
| `remote: <name>` | Routes artifact bytes through `[workflow.remotes.<name>]`; stage manifests and refs stay on the configured Crab repo remote |

### Command list conversion

DVC allows `cmd:` as a list of strings. Crab accepts that form directly and
runs each command in order in a fresh shell with the same working directory and
environment, stopping after the first failure. The migration tool preserves the
list form so shell state such as `cd` or `export` does not leak between entries:

```yaml
# DVC
cmd:
  - mkdir -p output
  - python build.py
  - python validate.py

# Crab (after migration)
cmd:
  - mkdir -p output
  - python build.py
  - python validate.py
```

### `always_changed` → `nondeterministic`

DVC's `always_changed: true` becomes `nondeterministic: true` when using the
migration tool, and Crab also accepts `always_changed: true` directly in
`crab.yaml`. Same semantics: the stage is reported stale, skips local and remote
run-cache lookup, and always re-executes regardless of input hashes.

### Legacy DVCLive `live:`

Old DVC projects may contain a stage-level `live:` section instead of the
standard DVCLive-generated `params:`, `metrics:`, and `plots:` declarations.
The migration tool converts common `live:` forms such as `live: dvclive`,
`live: {path: dvclive}`, and `live: {dvclive: {summary: true, html: true}}`
to a cached directory output at the live directory plus a stage metric at
`<live-dir>/metrics.json` and a stage plot target at `<live-dir>/plots`.
Crab plot rendering expands directories recursively, so logged metric series,
sklearn plots, images, and other DVCLive plot files under that directory are
picked up when they exist. If your DVC YAML already has standard `plots:`
entries from DVCLive, those are preserved and Crab will hash/render them
normally.

## Unsupported features

These DVC features have no crab equivalent. The migration tool emits
warnings but continues converting the rest of the pipeline.

| DVC feature | Workaround |
|-------------|------------|
| SSH/HDFS external deps | Keep DVC-style HTTP(S), `file://`, S3, GCS, Azure, or `remote://` aliases backed by those URLs in `deps:` for live change detection, use a pinned URL dep such as `{ url: { url: "ssh://...", digest: "b3:<64-hex>" } }` when you want network-free hashing for an unsupported backend, or use Crab's cross-repo deps when a configured resolver is available. SSH, HDFS, and WebHDFS deps still need a digest until those resolvers land. |
| SSH/HDFS external outs | Absolute local paths, `file://`, HTTP(S), S3, GCS, Azure, and `remote://` aliases backed by those URLs are supported when non-cached. SSH, HDFS, and WebHDFS output URLs remain a migration warning until those resolvers land. |
| Hydra custom Python resolvers/plugins | Crab composes YAML defaults-list config groups directly, honors package directives such as `# @package _global_`, and resolves nested `${...}`, relative `${.sibling}` / `${..parent}` references, `${join:...}`, `${oc.env:VAR,default}`, `${oc.select:key,default}`, `${oc.decode:...}`, `${oc.create:...}`, `${oc.deprecated:key[,message]}`, `${oc.dict.keys:path}`, and `${oc.dict.values:path}` expressions. Move arbitrary Python resolver/plugin logic into the stage command when it cannot be expressed with that safe subset or static YAML defaults. |

Crab renders current plot data from CSV, TSV, JSON object arrays, YAML object
arrays, and JPEG/GIF/PNG/SVG images declared in `plots:` or passed as ad-hoc
targets. Directories are expanded recursively, so a migrated DVC image plot
directory becomes a self-contained HTML image dashboard. For hierarchical
JSON/YAML files, Crab plots the first nested array of objects, matching DVC's
common metrics shape. Use
`crab plots show --format html --output plots.html` for a browser-ready
report, `--show-vega` for the underlying Vega-Lite spec, or
`crab plots show metrics/loss.csv --x epoch --y val_loss --format html --open`
to render and open a target file directly. DVC-style arbitrary plot IDs such as
`train_val_test` can be passed to `crab plots show train_val_test` and expand
to the source files declared under that plot. DVC plots whose `x` and `y`
mappings point to different files, such as actual labels in one file and
predicted labels in another, are paired by row index. Headerless CSV/TSV
targets use DVC-style zero-based columns with `--no-header`, `-x 0`, repeated
`-y`, and optional `--x-label`/`--y-label` display names. Use
`--html-template .dvc/plots/mypage.html` with templates containing a
`{plot_divs}` marker for DVC-style custom HTML wrappers. Use
`crab plots diff --format html --output plots.html` for the DVC-style `HEAD`
vs workspace overlay. `crab plots diff --targets metrics/loss.csv -- main
candidate experiment` compares explicit refs with DVC-style target/revision syntax, and
`crab plots diff --baseline main --target candidate` is the equivalent explicit
Crab spelling. `crab metrics plot` is the same rendering engine under the
metrics command group. DVC-style Vega-Lite JSON templates can be used by path,
by built-in name, or by local names from `.crab/plots` and `.dvc/plots` when
they use standard `<DVC_METRIC_...>` anchors. Use `crab plots templates` to
list built-in/local templates, or `crab plots templates <name>` to dump a
Vega-Lite JSON template for customization. Non-standard Vega template logic
still needs manual review after migration.

Metrics comparison scripts can usually keep DVC's revision and output shape:
`dvc metrics show` maps to `crab metrics show` for workspace metrics,
`dvc metrics show -aT --md` maps directly for branch/tag Markdown output, and
`dvc metrics show -R metrics --all-commits --json` maps directly for recursive
history JSON.
`dvc metrics diff` maps to `crab metrics diff` for `HEAD` vs workspace,
`dvc metrics diff main` maps to `crab metrics diff main` for main vs
workspace, and `dvc metrics diff --targets metrics.json -- main candidate`
maps directly. `--all`, `--json`, `--md`, `--no-path`, and `--precision <n>`
are accepted, `-R/--recursive` searches directory targets, and same-named
metrics in different files remain path-scoped.

Parameter comparison scripts can also keep DVC's shape:
`dvc params diff` maps to `crab params diff` for `HEAD` vs workspace,
`dvc params diff main` maps to `crab params diff main` for main vs workspace,
and `dvc params diff --targets params.yaml conf/model.yaml -- main candidate`
maps directly. `--all`, `--deps`, `--json`, `--md`, and `--no-path` are
accepted, and same-named params in different files remain path-scoped.

Python params files are migrated and tracked for hashing/status when they
contain literal top-level assignments, class constants, or `self.*`
assignments inside `__init__`. Experiment overrides can rewrite YAML, JSON,
TOML, and those same Python literal params, including nested literal dict/list
values. For DVC projects with Hydra enabled, run
`crab config set hydra.enabled true`; Crab composes
`conf/config.yaml` and its defaults-list config groups into `params.yaml`
inside each experiment worktree before applying `--set-param` scalar
overrides. Hydra package directives such as
`# @package _global_`, `# @package _group_`, and `# @package _group_._name_`
are honored for config-group files; explicit package overrides in the
defaults list still take priority. Nested `${...}` interpolation, relative
`${.sibling}` / `${..parent}` references, `${join:...}`,
`${oc.env:VAR,default}`, `${oc.select:key,default}`, `${oc.decode:...}`,
`${oc.create:...}`, `${oc.deprecated:key[,message]}`, `${oc.dict.keys:path}`,
and `${oc.dict.values:path}` resolver expressions are resolved during
composition. `${oc.decode:...}` converts string/env values into YAML scalars,
lists, or maps. Custom params files can be targeted with
`crab exp run --set-param custom.yaml:model.lr=0.01`,
`crab exp run --set-param params.py:TrainConfig.layers=12`, and
`crab exp run --set-param custom.yaml:~model.dropout`. Dynamic Python expressions
remain read-only and should be moved to a literal params file if you want
`--set`/`--set-param` to mutate them. Hydra sweep forms such as
comma-separated choices, `choice(...)`, and `range(...)` are queue-only, just
like DVC: use `crab exp run --queue -S model.lr=0.001,0.01`.

DVC target-selection commands map directly to `crab repro`, `crab run`, and
`crab exp run`. `dvc repro train` can stay `crab repro train` and runs
`train` plus upstream dependencies through the same executor as `crab run`.
Use `crab repro --single-item train` for DVC's `--single-item`,
`crab repro --downstream train` for `--downstream`,
`crab repro --pipeline train` for `--pipeline`,
`crab repro --all-pipelines` for `--all-pipelines`, and
`crab repro --glob 'train_*'` for `--glob`. DVC's `-R/--recursive` spelling is
available as `crab repro -R <target>`, `crab run -R <target>`, or
`crab exp run -R <target>` for nested workflow discovery. DVC's
`--force-downstream` maps to `crab repro --force-downstream <target>`,
`crab run --force-downstream <target>`, or
`crab exp run --force-downstream <target>` and is preserved by queued
experiments. DVC's `-i/--interactive` maps to
`crab repro -i <target>`, `crab run -i <target>`, and
`crab exp run -i <target>`; Crab asks before each selected stage that would
execute and treats a declined stage as an intentional skip for that run.
Path-qualified DVC targets such as `models/dvc.yaml:train` are accepted as
aliases for Crab's canonical nested name `models.train`; the same form works
with `--glob`, for example `crab repro --glob 'models/dvc.yaml:train-*'`.
DVC's `--dry` spelling is accepted as an alias for `crab repro --dry`,
`crab run --dry-run`, and `crab exp run --dry`. Dry-run workflow DAGs print
the selected stage plan without executing commands or writing `crab.lock`;
dry-run experiments do not persist experiment metadata.
`crab repro --no-run-cache` and `crab run --no-run-cache` execute stage
commands even when a matching run-cache entry exists.
Use `crab status --workflow [targets...]` as the DVC-style replacement for
`dvc status [targets...]` when you want pipeline freshness; preserve `--json`,
`-R/--recursive`, and `-d/--with-deps` where used. `--why <stage>` is Crab's
extra targeted stale-stage diagnosis.
Plain `crab status` remains the Crab hydration/working-tree status view.
Use `crab stage list`, `crab stage list --all`, or
`crab stage list -R pipelines/` as the DVC-style replacement for
`dvc stage list`; `--name-only`, `--fail`, and path-qualified targets such as
`models/dvc.yaml:train` are accepted.
Use `crab stage add -n <stage> ... command` as the DVC-style replacement for
`dvc stage add`. The common authoring flags map directly: `-d/--deps`,
`-p/--params`, `-o/--outs`, `-O/--outs-no-cache`, `--outs-persist`,
`--outs-persist-no-cache`, `-m/--metrics`, `-M/--metrics-no-cache`,
`--plots`, `--plots-no-cache`, `-w/--wdir`, `--always-changed`,
`--desc`, `-f/--force`, and `--run`. Crab writes `crab.yaml`, validates the
resulting DAG before saving, and adds `--json` for scripts that want structured
authoring results. `-O/--outs-no-cache` also accepts absolute local paths and
external URLs such as `file://`, HTTP(S), `s3://`, `gs://`, `azure://`, and
`remote://` aliases backed by those URLs for DVC-style external outputs, which
Crab tracks in `crab.lock` without caching.
Use `crab workflow dag [target]` as the DVC-style replacement for `dvc dag`.
Crab accepts `-o/--outs`, `--full`, `--md`, `--mermaid`, `--dot`, and
`--collapse-foreach-matrix`; targets follow the same aliases as `crab run`,
including `models/dvc.yaml:train`. Crab also keeps `--format ascii|mermaid|dot`,
`--recursive`, and `--json` for terminal, nested-workflow, and scripted use.
Use `crab freeze <stage>` and `crab unfreeze <stage>` as DVC-style replacements
for `dvc freeze <stage>` and `dvc unfreeze <stage>`; path-qualified targets
such as `models/dvc.yaml:train` resolve to the declaring nested `crab.yaml`.
Frozen stages stay skipped even when `crab repro --force` or `crab run --force`
is used, matching DVC. `dvc repro --no-commit` maps to
`crab repro --no-commit` or `crab run --no-commit`: commands execute and
`crab.lock` is updated, but fresh run-cache entries and output xorbs are not
written. The same target flags work for experiments, including queued
experiments:
`crab exp run --queue --downstream train -S model.lr=0.001,0.01` persists the
target selector with each queued task. DVC's
`dvc exp run -C secrets.env --temp` or
`dvc exp run --queue -C secrets.env` maps to
`crab exp run -C secrets.env ...` and
`crab exp run --queue -C secrets.env ...`: Crab copies each requested
repo-relative path into the isolated experiment worktree before DAG execution,
and queued entries replay the same copy-path list when workers start.

DVC-style experiment review flows can use `crab exp show --md` or
`crab exp show --csv` to export recent experiment tables,
`crab exp show --sort-by model.lr --sort-order asc` to sort by captured
params or metrics, and `crab exp show --only-changed --drop seed
--keep model.lr` to hide noisy captured param/metric keys. Use
`crab exp show -aT --all-commits --rev HEAD --num -1 --no-pager
--hide-failed` when porting DVC history-selector scripts. Crab accepts DVC's
`--param-deps`, `--sha`, `--hide-queued`, `--hide-workspace`, and `--force`
flags too: `--hide-failed` filters experiments persisted with `failed`
metadata status, while queued tasks, workspace rows, pager output, and show
caches are not part of Crab's local `exp show` table. Use unambiguous ID
prefixes with `crab exp diff <id_a> <id_b>`, and
`crab exp diff <id_a> <id_b> --all --no-path` to include unchanged values
while hiding file prefixes in text/Markdown output. Use
`crab exp diff <id_a> <id_b> --md --precision 3 --param-deps` for a Markdown
params/stages/metrics report suitable for PR comments; `--param-deps` is
accepted because Crab experiment metadata already contains the experiment
parameter overrides rather than a wider params table. `crab exp list` is a
DVC-style alias for `crab exp ls`, and `--sort-by name` sorts by persisted
experiment labels from `crab exp run --name` and `crab exp save --name`.
`--sort-by message` sorts by DVC-style experiment messages from
`crab exp run -m/--message` and `crab exp save -m/--message`.
Use `crab exp run --queue -S key=a,b --name sweep --message "grid search"` as
the DVC-style spelling for queued sweeps; comma-separated choices,
`choice(...)`, and Hydra-compatible stop-exclusive `range(...)` values expand
to Cartesian products. Then
`crab exp run --run-all -j 2` processes the pending queue. The native Crab
spellings are `crab exp queue -S ...` and `crab exp start --jobs ...`. Use
DVC-style `crab queue status` and `crab queue logs <task>` to inspect running
or completed task output. Use
`crab queue kill <task>` or `crab queue kill --force <task>` to interrupt
running tasks, and `crab queue stop --kill` to stop launching new work while
force-killing current tasks. Use `crab queue remove` selectors such as
`--queued`, `--success`, and `--failed` to clean queue task records and
associated task logs without deleting experiment metadata. Use
`crab exp clean` as the DVC-style `dvc exp clean` replacement after a crashed
or externally force-killed worker; it removes stale experiment tmpdirs and queue
housekeeping files while preserving saved experiment metadata.
Use `crab exp rename <id> <name>` to update a local experiment label; pass
`--force` to allow a duplicate label. If the experiment was already pushed,
run `crab exp push --force <id>` to publish the renamed metadata.
Use `crab exp save --name <label>` to capture the current workspace as an
experiment without rerunning the DAG, and add `-m/--message <text>` to carry
DVC's custom experiment-save message. DVC save targets map directly:
`dvc exp save -R dir_a/dvc.yaml` becomes `crab exp save -R dir_a/dvc.yaml`,
and Crab treats `dvc.yaml` path targets as aliases for the migrated
`crab.yaml` files. These targets filter the saved stage hashes and declared
metrics in experiment metadata; Crab still captures the whole workspace
snapshot for `exp apply`. Unlike DVC, Crab snapshots untracked workspace files
by default; `-I/--include-untracked` is accepted as a compatibility hint for
migrated scripts.
After selecting a winner, use `crab exp apply <winner_id>` to overlay the
captured local experiment workspace snapshot onto the current workspace, or
`crab exp branch <winner_id> [branch]` as the DVC-style spelling for creating
a Git branch from the experiment.
Use `crab exp push <id>` or `crab exp push --all` to share experiment
metadata and apply snapshots through the configured `crab://` object-store
remote, and `crab exp pull <id>` or `crab exp pull --all` to download them in
another checkout. Crab does not take DVC's separate Git remote argument here:
the Crab remote is the object-store remote configured for the repository.
Pulled experiments can be shown, diffed, and applied without rerunning the
workflow. Stage-cache reuse is still explicit: use `crab workflow push-cache`
and `crab run --pull-cache` when you want future runs to reuse published stage
outputs.
Then use `crab exp remove <id_or_name>` to delete specific local experiments,
`crab exp remove --keep <winner_id_or_name>` to keep one or more winners and
prune the rest, or `crab exp remove --all --dry-run` to preview a full local
cleanup. DVC queued experiment removal maps to `crab exp remove <queued_id>`
for specific pending tasks or `crab exp remove --queue` for every pending
queued task. DVC history selectors map locally too: `crab exp remove --rev
HEAD~1 --num 3 --dry-run` previews experiments whose recorded base commits
are in that first-parent window, and negative `--num` means all first-parent
commits. DVC remote cleanup maps to `crab exp remove -g origin <id_or_name>`
when the Git remote resolves to a `crab://` URL; a direct `crab://` URL also
works. Remote deletion removes Crab experiment refs and objects from the
configured object-store remote used by `crab exp push` and `crab exp pull`.

## Manual steps after migration

After running `crab migrate from-dvc`:

1. **Review warnings.** The migration report lists anything that couldn't
   be converted automatically. Address each warning.

2. **Validate the pipeline.**
   ```bash
   crab run --validate
   ```
   Fix any schema errors or undefined template references.

3. **Convert the lockfile.** The migration tool does NOT convert
   `dvc.lock` to `crab.lock`. Run the pipeline once to generate a
   fresh lockfile:
   ```bash
   crab run
   ```
   This re-executes all stages (no prior cache exists). If you want to
   avoid re-execution, populate the cache first by running with existing
   outputs present.

4. **Update CI scripts.** Use `crab repro` when you want to keep the DVC
   command shape, or `crab run` as the native Crab spelling. Preserve target
   flags such as `--single-item`, `--downstream`, `--pipeline`,
   `--all-pipelines`, `--glob`, `--force-downstream`, `-R/--recursive`,
   `--dry`, `-i/--interactive`, `--no-run-cache`, and `--no-commit`. Replace
   `dvc status` checks with `crab status --workflow`, preserving targets,
   `--json`, `-d/--with-deps`, and `-R/--recursive` where used. Replace
   `dvc exp run --dry` with `crab exp run --dry`, and preserve experiment
   copy-path overlays and messages with `crab exp run -C <path> -m <message>`
   or `crab exp run --queue -C <path> -m <message>`. Preserve targeted
   workspace captures with `crab exp save -R <target> -m <message>`. Replace
   `dvc stage add` with `crab stage add`, replace `dvc stage list` with
   `crab stage list`, replace `dvc dag` with `crab workflow dag`, replace
   `dvc exp clean` with `crab exp clean`, and replace `dvc push` with
   `crab run --cache-push`.

5. **Update `.gitignore`.** DVC adds entries like `/model.pkl` for
   tracked outputs. Crab uses the same pattern — your existing
   `.gitignore` likely works as-is.

6. **Remove DVC artifacts.** Once satisfied:
   ```bash
   rm dvc.yaml dvc.lock
   rm -rf .dvc/
   git rm .dvc/.gitignore  # if tracked
   pip uninstall dvc        # optional
   ```

7. **Commit the migration.**
   ```bash
   git add crab.yaml crab.lock .gitignore
   git commit -m "Migrate pipeline from DVC to crab"
   ```

## Example: 5-stage DVC pipeline

### Before (`dvc.yaml`)

```yaml
vars:
  - codedir: src
  - datadir: data

stages:
  download:
    cmd: "python ${codedir}/download.py --out ${datadir}/raw.csv"
    deps:
      - ${codedir}/download.py
    outs:
      - ${datadir}/raw.csv
    always_changed: true

  clean:
    cmd: "python ${codedir}/clean.py"
    deps:
      - ${codedir}/clean.py
      - ${datadir}/raw.csv
    outs:
      - ${datadir}/clean.parquet

  featurize:
    cmd: "python ${codedir}/featurize.py"
    deps:
      - ${codedir}/featurize.py
      - ${datadir}/clean.parquet
    params:
      - features.window_size
      - features.columns
    outs:
      - ${datadir}/features.parquet

  train:
    cmd:
      - mkdir -p models
      - python ${codedir}/train.py
    deps:
      - ${codedir}/train.py
      - ${datadir}/features.parquet
    params:
      - model.lr
      - model.epochs
      - model.arch
    outs:
      - models/model.pkl:
          persist: true
      - models/checkpoints/:
          push: false
    metrics:
      - metrics/train.json

  evaluate:
    cmd: "python ${codedir}/evaluate.py"
    deps:
      - ${codedir}/evaluate.py
      - models/model.pkl
      - ${datadir}/features.parquet
    metrics:
      - metrics/eval.json
    plots:
      - metrics/roc.csv
```

### After (`crab.yaml` — generated by `crab migrate from-dvc`)

```yaml
vars:
  - codedir: src
  - datadir: data

stages:
  download:
    cmd: "python ${codedir}/download.py --out ${datadir}/raw.csv"
    deps:
      - ${codedir}/download.py
    outs:
      - ${datadir}/raw.csv
    nondeterministic: true

  clean:
    cmd: "python ${codedir}/clean.py"
    deps:
      - ${codedir}/clean.py
      - ${datadir}/raw.csv
    outs:
      - ${datadir}/clean.parquet

  featurize:
    cmd: "python ${codedir}/featurize.py"
    deps:
      - ${codedir}/featurize.py
      - ${datadir}/clean.parquet
    params:
      - features.window_size
      - features.columns
    outs:
      - ${datadir}/features.parquet

  train:
    cmd: "mkdir -p models && python ${codedir}/train.py"
    deps:
      - ${codedir}/train.py
      - ${datadir}/features.parquet
    params:
      - model.lr
      - model.epochs
      - model.arch
    outs:
      - models/model.pkl:
          persist: true
      - models/checkpoints/
    metrics:
      - metrics/train.json

  evaluate:
    cmd: "python ${codedir}/evaluate.py"
    deps:
      - ${codedir}/evaluate.py
      - models/model.pkl
      - ${datadir}/features.parquet
    metrics:
      - metrics/eval.json
    plots:
      - metrics/roc.csv
```

### Migration report

```
Migration Report
==================================================
Stages converted: 5
Output written to: crab.yaml
Warnings: none
==================================================
```

The `checkpoints/` output keeps `push: false`: Crab still writes and reads the
local stage cache, but skips remote publication for the whole stage because a
remote stage-cache entry must be complete. `cache: false` is different: it keeps
output hashes in `crab.lock` but disables run-cache reuse for the whole stage.
Per-output `remote:` is preserved and active for workflow cache transfers.
Add matching `[workflow.remotes.<name>]` entries in `.crab.toml` when your DVC
project depended on different storage backends per output.
