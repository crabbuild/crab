# Crab Workflow Engine vs DVC Pipeline Engine — Deep Comparison

## Executive Summary

Crab's workflow engine has demonstrated advantages over DVC for parallel DAG
execution, crash recovery, resource-aware scheduling, structured output, and
content-defined storage. It is not yet a general DVC replacement: migration
cutover, checkpoint transport, artifact remote/GC integration, provider live
qualification, and the complete data-command profile remain gated by Plan 014.
This document maps current Crab behavior to DVC concepts and names the gaps;
the verdict column is not a release-parity claim.

Sources: [DVC docs](https://dvc.org/doc/user-guide/pipelines),
[DVC dvc.yaml spec](https://dvc.org/doc/user-guide/project-structure/dvcyaml-files),
[DVC run cache](https://dvc.org/doc/user-guide/pipelines/run-cache),
[DVC experiments](https://dvc.org/doc/user-guide/experiment-management/running-experiments),
[DVC Hydra composition](https://doc.dvc.org/user-guide/experiment-management/hydra-composition),
[Hydra packages](https://hydra.cc/docs/advanced/overriding_packages/),
[DVC repro](https://dvc.org/doc/command-reference/repro),
[DVC dag](https://dvc.org/doc/command-reference/dag),
[DVC stage add](https://dvc.org/doc/command-reference/stage/add),
[DVC stage list](https://dvc.org/doc/command-reference/stage/list),
[DVCLive](https://doc.dvc.org/dvclive/live),
[DVC freeze](https://dvc.org/doc/command-reference/freeze),
[DVC unfreeze](https://dvc.org/doc/command-reference/unfreeze),
[DVC exp run](https://dvc.org/doc/command-reference/exp/run),
[DVC exp save](https://dvc.org/doc/command-reference/exp/save),
[DVC exp clean](https://dvc.org/doc/command-reference/exp/clean),
[DVC queue kill](https://dvc.org/doc/command-reference/queue/kill), and
[DVC queue stop](https://dvc.org/doc/command-reference/queue/stop).

---

## 1. Pipeline Definition

| Aspect | DVC (`dvc.yaml`) | Crab (`crab.yaml`) | Verdict |
|--------|-------------------|------------------------|---------|
| Format | YAML 1.2, `deny_unknown_fields` | YAML 1.2, `deny_unknown_fields` | Parity |
| Stage command | `cmd:` (string or list of strings); list entries execute sequentially in separate shell invocations and stop on first failure | `cmd:` shell string, DVC-style shell list with the same separate-shell fail-fast semantics, or `cmd: {argv: [...]}`; shell strings use `/bin/sh -c` on Unix and `cmd.exe /D /S /C` on Windows, while argv stays shell-free | Core parity; platform shell syntax is not translated |
| Dependencies | `deps:` (file paths and supported external URLs/paths) | `deps:` local paths, DVC-style URL strings, DVC-style `remote://name/path` aliases through `[workflow.remotes.<name>]`, explicit `stage_out:`, pinned `url:` deps with `b3:<64-hex>` digests, and digest-less live HTTP(S), `file://`, S3, GCS, and Azure deps; schema also reserves `crab:`, `git:`, `oci:`, and SSH/HDFS-style URL forms for configured resolvers | Core parity for local, HTTP(S), file, S3, GCS, Azure, and `remote://` aliases backed by those URLs; SSH/HDFS dependency backends remain a gap |
| Outputs | `outs:` path strings or path-key maps with `cache`, `persist`, `push`, `remote` subfields; external outputs are tracked but not cached | `outs:` path strings, DVC path-key maps, or explicit `path:` maps with `cache`, `push`, `persist`, `remote`, `kind`, `max_bytes`, and distinct `checkpoint` fields; `cache: false` disables stage-cache reads/writes as in DVC, `push: false` keeps local cache but suppresses remote stage-cache publication, and supported local/HTTP/object-store external outs are tracked when non-cached | Core parity for the documented local and object-store profile; remote output routing and SSH/HDFS/WebDAV/Drive/OSS providers remain capability-gated |
| Parameters | Top-level params files plus stage-level dot-path keys, file-scoped keys, and whole-file refs; defaults to `params.yaml`; supports YAML, JSON, TOML, Python params files; `dvc params diff [--targets <path> ...] [--all] [--deps] [--json] [--md] [--no-path] [a_rev] [b_rev]` defaults to `HEAD` vs workspace | Same stage-level refs and params formats; Python files are parsed as safe literal top-level assignments, class constants, and `self.*` assignments inside `__init__` for hashing/status; `crab params diff` defaults to `HEAD` vs workspace, uses declared workflow/stage params before falling back to `params.yaml`, keeps same-named params path-scoped, and accepts DVC-style `--targets <path>... -- [a_rev] [b_rev]`, `--all`, `--deps`, `--json`, `--md`, and `--no-path` | Core parity for params tracking, explicit files, path-scoped output, workspace/default diff, two-ref diff, dependency-only diff, `--all`, JSON, Markdown, and no-path |
| Metrics | Top-level and stage-level `metrics:` file lists; stage metrics may use DVC output settings; `dvc metrics show [-a] [-T] [--all-commits] [--json] [--md] [-R] [targets...]` prints declared or explicit metric files, and `dvc metrics diff [--targets <path> ...] [-R] [--all] [--json] [--md] [--no-path] [--precision <n>] [a_rev] [b_rev]` compares declared or explicit metric files, defaulting to `HEAD` vs workspace | Top-level and stage-level `metrics:` file lists; stage metric hashes are written to `crab.lock`, and structured stage metrics accept DVC `cache`, `persist`, and `push` settings; `crab metrics show` defaults to workspace metrics, uses declared workflow/stage metric paths before falling back to `metrics.json`, keeps same-named metrics path-scoped, and accepts DVC-style targets, `-R/--recursive`, `-a/--all-branches`, `-T/--all-tags`, `-A/--all-commits`, `--json`, and `--md`; `crab metrics diff` defaults to `HEAD` vs workspace and accepts DVC-style `--targets <path>... -- [a_rev] [b_rev]`, `-R/--recursive`, `--all`, `--json`, `--md`, `--no-path`, and `--precision <n>` | Core parity for declared metrics, explicit files, recursive directory targets, path-scoped show/diff output, workspace/default diff, two-ref diff, history show, JSON, Markdown, no-path, and precision |
| Plots | Top-level or stage-level `plots:` with rich x/y/template config and render commands, plus `dvc plots show`, `dvc plots diff`, and `dvc plots templates [template]` | Top-level and stage-level `plots:` accept paths, arbitrary plot IDs, DVC multi-source `x`/`y` mappings, and DVC confusion-style plots where `x` and `y` come from different files; stage plot files/directories are hashed into `crab.lock`; top-level configs also carry `x`, `y`, `no_header`, `x_label`, `y_label`, `title`, and `template` metadata for `crab plots show [targets...] --show-vega --no-header --x-label --y-label --html-template --open`; `crab plots diff` defaults to `HEAD` vs workspace, accepts DVC-style `--targets <path-or-id>... -- <rev> [rev ...]`, `--baseline <ref>` compares that ref vs workspace, `--target <ref>` enables explicit ref overlays, and recursive image/data-series rendering is supported; `crab plots templates [template]` lists/dumps built-in and local Vega-Lite JSON templates with DVC anchors | Parity for core data-series, image, directory, plot ID, cross-file x/y, custom HTML wrapper, workspace/default diff, multi-ref diff, and template workflows; DVC still ahead on VS Code/Studio dashboard ecosystem polish |
| DVCLive files | Modern DVCLive writes `params:`, `metrics:`, and `plots:` entries; older projects may contain stage-level `live:` sections | Modern DVCLive-generated declarations pass through; migration maps legacy `live:` paths to a directory output plus `<live-dir>/metrics.json` and the recursive `<live-dir>/plots` target, and preserves explicit DVCLive plot declarations when present | Migration parity for local DVCLive outputs; DVC still ahead on Studio/live-dashboard ecosystem polish |
| Artifacts | Top-level `artifacts:` model/artifact metadata | Top-level `artifacts:` is validated as catalog metadata; local `crab artifacts list/show/get/version create/promote/history` is available for lockfile-backed local outputs; remote publication, clean-clone enumeration, and GC reachability remain gated | Local lifecycle preview; not replacement parity |
| Working dir | `wdir:` per stage; relative deps, outs, and stage params resolve from it | `wdir:` per stage, validated relative to repo root; relative deps, outs, default `params.yaml`, and file-scoped stage params resolve from it | Parity |
| Stage authoring | `dvc stage add -n <stage> [-f] [-d path] [-p [file:]keys] [-o path] [-O path] [--outs-persist path] [--outs-persist-no-cache path] [-m path] [-M path] [--plots path] [--plots-no-cache path] [-w path] [--always-changed] [--desc text] [--run] command` writes or updates `dvc.yaml` and checks graph integrity | `crab stage add` accepts the supported core authoring flags, writes `crab.yaml`, rejects overwrites unless `--force`, validates the resulting workflow through Crab's parser and DAG graph builder, writes `checkpoint: true` explicitly for `--checkpoints`, and adds `--json` output; ordinary `run/repro` still fail closed until experiment lineage is wired | Core parity except experiment checkpoint lifecycle; Crab adds JSON |
| Stage listing | `dvc stage list [-R] [--all] [--fail] [--name-only] [targets...]` lists stage names and descriptions from workflow files | `crab stage list [-R] [--all] [--fail] [--name-only] [--json] [targets...]` lists Crab stages, accepts DVC-style `path/to/dvc.yaml:stage` aliases, generates descriptions from `desc`, outs, metrics, plots, or deps, and adds structured JSON output | Parity; Crab adds JSON |
| Frozen stages | `frozen: true`; `dvc freeze <stage>` and `dvc unfreeze <stage>` toggle it; `--force` does not override frozen stages | `frozen: true`; `crab freeze <stage>` and `crab unfreeze <stage>` toggle it, including DVC-style `path/to/dvc.yaml:stage` aliases; `--force` does not override frozen stages | Parity |
| Always changed | `always_changed: true` | `always_changed: true` compatibility alias and `nondeterministic: true` native spelling; both mark status stale, skip local/remote run-cache lookup, and force execution | Parity |
| Stage description | `desc:` field | `desc:` field | Parity |
| Meta field | `meta:` (arbitrary ignored data) | `meta:` (arbitrary ignored data) | Parity |

### Templating & Iteration

| Feature | DVC | Crab | Verdict |
|---------|-----|--------|---------|
| Variable substitution | `${param.key}` from default `params.yaml`, additional YAML/JSON/TOML/Python params files, or `vars:` | `${...}` substitution from default `params.yaml`, top-level `params:`, `vars:` file references including Python literal params files, DVC selected-key imports, recursive vars merging, and explicit `env.*` refs | Parity |
| `foreach` stages | Iterate over literal or params-file list/dict values, expand to N stages | `foreach` + `do` expansion from literal or params-file list/dict values | Parity |
| `matrix` stages | Cartesian product expansion, including params-file dimensions and scalar or composite dict/list values | `matrix` expansion from literal or params-file dimensions with scalar suffixes and variable-index suffixes for composite values | Parity |
| Dictionary unpacking in cmd | `${mydict}` → `--key val` args | Same for shell-form `cmd:` strings, including nested keys and scalar lists | Parity |

**Gap assessment**: Crab now covers the major DVC templating and expansion
surfaces (`vars`, params substitution, `foreach`, `matrix`, command dictionary
unpacking). DVC remains ahead for custom Hydra plugins/resolvers and the
surrounding tooling/documentation users expect from polished experiment sweeps.

---

## 2. DAG Execution

| Aspect | DVC (`dvc repro`) | Crab (`crab repro` / `crab run`) | Verdict |
|--------|-------------------|------------------------|---------|
| Execution model | **Sequential** — stages run one at a time in topo order | **Parallel** — independent stages run concurrently (semaphore-bounded) | **Crab far ahead** |
| Parallelism config | None (single-threaded) | `--parallelism N` or `[workflow] parallelism` | **Crab ahead** |
| Partial failure | Stops at first failure | `--keep-going` (independent branches continue) | **Crab ahead** |
| Ignore errors | Not supported | `--ignore-errors` (attempt all stages) | **Crab ahead** |
| Resource constraints | Not supported | `resources: {cpu, gpu, memory}` gates scheduling | **Crab ahead** |
| Stage timeout | Not supported | `timeout: "1h"` per stage | **Crab ahead** |
| Retry on failure | Not supported | `retry: {max_attempts, on_exit_codes, on_signals, backoff}` | **Crab ahead** |
| Crash recovery | None — re-run from scratch | Journal-based resume (committed stages skipped) | **Crab far ahead** |
| Scheduler lock | `.dvc/tmp/lock` (whole project) | Per-ref scheduler lock with configurable timeout | **Crab ahead** |
| Target selection | `dvc repro <stage>` runs one stage + upstream; `path/to/dvc.yaml:stage`, `--single-item`, `--downstream`, `--pipeline`, `--all-pipelines`, and `--glob` tune the target set | `crab repro <stage>`, `crab run <stage>`, and `crab exp run <stage>` use the same target modes; DVC-style `path/to/dvc.yaml:stage` aliases resolve to Crab's canonical dotted nested names; queued experiments persist the selector | Parity |
| Single-item mode | `--single-item` (one stage only) | `crab repro --single-item <stage>`, `crab run --single-item <stage>`, or `crab exp run --single-item <stage>` | Parity |
| Force downstream | `--force-downstream` executes descendants of a changed stage even if their own inputs would hit run cache | `crab repro --force-downstream <stage>`, `crab run --force-downstream <stage>`, and `crab exp run --force-downstream <stage>` do the same; queued experiments persist the flag | Parity |
| Interactive confirmation | `dvc repro -i/--interactive` asks before reproducing each stage | `crab repro -i/--interactive` and `crab run -i/--interactive` ask before executing each selected stage that would otherwise run; declined stages are recorded as intentional skips for that run | Parity |

**Key insight**: DVC runs stages **strictly sequentially**. There is no
intra-pipeline parallelism in `dvc repro`. DVC's parallelism only exists at
the *experiment* level (`dvc queue start --jobs N`), where entire pipeline
copies run in isolated workspaces. Crab parallelizes *within* a single
pipeline run.

---

## 3. Caching & Invalidation

| Aspect | DVC | Crab | Verdict |
|--------|-----|--------|---------|
| Cache key | MD5 of (cmd + dep hashes + param values) | Blake3 of (cmd + dep hashes + param values + env + flags) | **Crab ahead** (stronger hash, more inputs) |
| Hash algorithm | MD5 (legacy), SHA-256 (DVC 3.0+) | Blake3 (faster, collision-resistant) | **Crab ahead** |
| Run cache | `.dvc/cache/runs/` — stores `dvc.lock` backup; `--no-run-cache` forces command execution | `.crab/cache/stages/{shard}/{hash}.json` stores full entries; `crab run --no-run-cache` and forced execution bypass local/remote lookup while still recording fresh outputs | Parity |
| No-commit execution | `--no-commit` updates `dvc.lock`/`.dvc` metadata but skips storing execution outputs in the cache | `crab run --no-commit` executes stages and updates `crab.lock` while skipping fresh local/remote run-cache writes and output xorbs | Parity |
| Cache hit behavior | Restores outputs, metrics, and plots from cache, skips execution | Restores outputs, metrics, and plots from cache, skips execution | Parity |
| Remote cache | `dvc push/pull --run-cache` | `crab run --cache-push` / `--pull-cache` | Parity |
| Cache integrity | MD5 verification | Blake3 verification + schema version check | **Crab ahead** |
| Explain miss | Not supported | `--explain-miss` (field-by-field diff) | **Crab ahead** |
| Cache-only mode | Not supported | `--cache-only` (exit 3 on miss) | **Crab ahead** |
| Disk pressure | No handling | Graceful degradation (skip writes, warn) | **Crab ahead** |
| Read-only cache | Not supported | Auto-detected, operates without cache | **Crab ahead** |
| `cache: false` per output | Disables run cache for that stage | `cache: false` per output | Parity |
| `push: false` per output | Keeps local cache, skips remote push | `push: false` per output suppresses remote stage-cache publication | Parity |
| `remote:` per output | Pushes selected output to a named data remote | `remote:` is parsed and reported, but a destination must be explicitly mapped and live-verified; the current migration/provider gate does not claim DVC-style per-output remote parity | Gap; fail closed until the provider matrix and remote refs are qualified |
| `persist: true` | Output not deleted before re-run | `persist: true` | Parity |

### What DVC hashes for the run cache

DVC's run cache key is a hash of:
- The literal `cmd` string
- MD5/SHA-256 of each dep file
- Param key-value pairs (from params files)

Crab's stage hash includes all of the above plus:
- Environment variable allowlist values
- `hermetic` / `nondeterministic` flags
- `wdir`
- Output path declarations (so renaming an out invalidates)

Bare stage-level params are resolved from declared top-level params files, or
from `params.yaml` when no top-level file list is declared. File-scoped refs
such as `custom.yaml: [model.lr]` resolve only against that file, and a null
file-scoped ref such as `sweep.json:` tracks every scalar value in that file.
The resolved scalar values are written to `crab.lock`, included in the stage
hash, and surfaced by `crab workflow status --json --why` when a param-only
change makes a stage stale.

---

## 4. Lockfile

| Aspect | DVC (`dvc.lock`) | Crab (`crab.lock`) | Verdict |
|--------|------------------|------------------------|---------|
| Format | YAML with `schema: '2.0'` | YAML with `schema_version` + `crab_hash_algo` | Parity |
| Content | cmd, dep hashes, param values, out hashes+sizes | cmd, dep hashes, param values, out hashes+sizes+modes | **Crab ahead** (preserves file modes) |
| Canonical form | Not enforced | Double-quoted strings, sorted keys | **Crab ahead** (deterministic diffs) |
| Multi-stage | All stages in one file | All stages in one file | Parity |
| Templating expansion | `foreach`/`matrix` expanded in lock | `foreach`/`matrix` expanded in lock | Parity |

---

## 5. Experiment Management

| Aspect | DVC | Crab | Verdict |
|--------|-----|--------|---------|
| Experiment tracking | `dvc exp run` creates hidden Git refs | `crab run` with experiment worktrees | Parity |
| Param override CLI | `--set-param key=value`, `+key=value`, `++key=value`, `~key`, Hydra config-group overrides, and `file:key=value` for custom params files | `crab exp run --set-param ...` (`--set` alias) and `crab exp queue --set-param ...` support overwrite/add/upsert/remove forms for YAML/JSON/TOML/Python literal params files; when `[hydra] enabled = true`, Crab composes Hydra defaults-list config groups into `params.yaml`, honors package directives such as `# @package _global_`, then applies scalar overrides and consumes group overrides such as `train/model=efficientnet` | Parity for core override and Hydra composition operations |
| Interactive confirmation | `dvc exp run -i/--interactive` asks before reproducing each stage | `crab exp run -i/--interactive` forwards the same confirmation mode into the isolated experiment worktree | Parity |
| Copy ignored/untracked paths | `dvc exp run -C/--copy-paths <path>` overlays ignored or untracked paths into temp/queued experiment workspaces | `crab exp run -C/--copy-paths <path>` copies repo-relative files, directories, or symlinks into the isolated experiment worktree before DAG execution; queued entries persist the path list and replay it when workers start | Parity |
| Experiment name/message | `dvc exp run -n/-m` and `dvc exp save -n/-m` persist a label and custom message for experiment review | `crab exp run -n/-m`, `crab exp save -n/-m`, and queued experiment entries persist both fields in experiment metadata; `exp show`, `exp ls`, Markdown, CSV, and JSON expose them, and `--sort-by message` sorts on messages | Parity |
| Experiment queue | `dvc exp run --queue` + `dvc queue start` | `crab exp run --queue` or `crab exp queue`, then `crab exp start` | Parity |
| Parallel experiments | `dvc queue start --jobs N` or `dvc exp run --run-all -j N` (isolated workspaces) | `crab exp start --jobs N` or DVC-style `crab exp run --run-all -j N` | Parity |
| Queue cleanup | `dvc queue remove --all/--queued/--success/--failed [task...]` removes non-active queue tasks without deleting experiment data | `crab queue remove --all/--queued/--success/--failed [task...]` removes pending, successful, or failed queue entries while preserving experiment metadata | Parity |
| Experiment temp cleanup | `dvc exp clean` removes experiment temporary files and outdated queue message files after crashed workers | `crab exp clean` removes stale experiment tmpdirs, active-run markers, kill requests, and orphan queue logs while preserving saved experiment metadata and active queued tmpdirs | Parity |
| Queue logs | `dvc queue logs [-f] task` shows running or completed task output | `crab queue logs [-f] task` reads active task logs from the isolated experiment worktree and persists completed task logs under the queue; `queue remove` deletes associated task logs | Parity |
| Queue interrupt | `dvc queue kill [-f] [task...]` interrupts running tasks; `dvc queue stop --kill` stops queue processing and kills current tasks | `crab queue kill [-f] [task...]` writes kill requests consumed by running stage supervisors; `crab queue stop --kill` stops new work and force-kills current queue tasks. Killed tasks are marked failed and workers continue or stop according to the command | Parity |
| Grid search | `--queue --set-param 'key=range(0.01,0.1,0.01)'`, `choice(...)`, or comma-separated choices | `crab exp run --queue -S key=range(...)`, `crab exp queue -S key=choice(...)`, and comma-separated value lists use Hydra-compatible stop-exclusive ranges and Cartesian products; non-queued `exp run` rejects sweep expressions instead of treating them as scalar strings | Parity |
| Experiment comparison | `dvc exp show`, `dvc exp list`, `dvc exp diff` | `crab exp show` lists experiments with text, Markdown, CSV, JSON, names, messages, status, precision, sort-by-column/key, `--only-changed`, `--drop`, and `--keep` output; it accepts DVC history selectors (`-a`, `-T`, `-A/--all-commits`, `--rev`, `-n/--num`), `--no-pager`, `--param-deps`, `--sha`, `--hide-failed`, `--hide-queued`, `--hide-workspace`, and `--force`, with `--hide-failed` filtering persisted failed metadata and queued/workspace/cache flags mapping to Crab's local metadata model; `crab exp ls` and DVC-style `crab exp list` provide compact lists; `crab exp show <id>` shows full metadata; short unambiguous ID prefixes are accepted; `crab exp diff` compares params, stage hashes, and metrics with text, Markdown, precision, JSON, `--all` unchanged rows, `--no-path`, and DVC-compatible `--param-deps` | Core parity |
| Experiment removal | `dvc exp remove <name>...`, queued ids, `--queue`, `--all`, `--keep`, `--rev`, `--num`, and `-g/--git-remote` | `crab exp remove <id-or-name>...`, `--queue`, `--all`, `--keep`, `--rev`, `--num`, `--dry-run`, and `-g/--git-remote`; accepts exact names, unambiguous local or remote experiment prefixes, pending queued experiment prefixes, and history selection by recorded base commits on first-parent history. Remote deletion resolves a Crab Git remote name or direct `crab://` URL and removes the object-store experiment refs/objects used by `exp push`/`exp pull` | Core parity, different remote transport |
| Experiment apply | `dvc exp apply <name>` restores experiment files into the workspace | `crab exp apply <id>` overlays the captured local or pulled experiment workspace snapshot, overwrites conflicts, and removes files deleted by the experiment | Core parity |
| Experiment save | `dvc exp save [-R] [-f] [-n name] [-I path] [-m message] [targets...]` snapshots the current workspace as an experiment | `crab exp save` snapshots the current workspace without running the DAG, persists `--name` and `--message`, accepts `-R/--recursive`, `-f/--force`, `-I/--include-untracked`, and DVC-style workflow targets such as `models/dvc.yaml`; targets filter saved stage hashes and declared metrics while Crab still captures modified, deleted, and untracked workspace files by default | Core parity; Crab captures untracked files by default |
| Experiment rename | `dvc exp rename <experiment> <name>` renames an experiment | `crab exp rename <id> <name>` updates the local experiment label, rejects duplicate labels unless `--force` is passed, and preserves the immutable experiment id | Core local parity |
| Experiment branch | `dvc exp branch <experiment> [branch]` creates a Git branch without switching to it | `crab exp promote <id> [-b branch]` and DVC-style `crab exp branch <id> [branch]` create a Git branch containing the captured experiment snapshot; if the branch name is omitted, Crab derives one from the experiment name or id | Core parity |
| Experiment persistence | `dvc exp push/pull` to Git remote plus data remote | `crab exp push/pull` shares experiment metadata refs, stage-ref lists, and apply snapshots through the configured `crab://` object-store remote; supports explicit ids, unambiguous prefixes, `--all`, and `--force` | Core parity, different transport |

**Gap assessment**: Crab has both intra-pipeline parallelism and queued
experiment parallelism, workspace save/apply, local and remote experiment
cleanup workflows, and experiment apply from pulled snapshots. DVC remains
ahead for custom Hydra plugins/resolvers and the polished dense-table display
ecosystem around queued runs. Crab keeps stage-cache reuse on
`crab workflow push-cache` /
`crab run --pull-cache` rather than coupling it to experiment transport.

---

## 6. Observability & Output

| Aspect | DVC | Crab | Verdict |
|--------|-----|--------|---------|
| Structured output | Limited (some JSON in newer versions) | `--json` (envelope) and `--jsonl` (streaming events) | **Crab far ahead** |
| Progress events | Text-based ("Running stage 'X'") | Typed JSONL events (started, cache_checked, produced, committed) | **Crab far ahead** |
| OTLP tracing | Not supported | OpenTelemetry spans per stage | **Crab ahead** |
| Dry-run | `dvc repro --dry`, `dvc exp run --dry` | DVC-style `crab repro --dry`, `crab run --dry-run`, `crab run --dry`, and `crab exp run --dry`; YAML DAG dry-runs emit `workflow.dag_plan` without writing `crab.lock` | Parity |
| Pipeline status | `dvc status [-d] [-R] [--json] [targets...]` reports changed deps/outs or an up-to-date pipeline | `crab status --workflow [-d] [-R] [--json] [targets...]` and `crab workflow status [--with-deps] [--recursive] [--json] [targets...]` report up-to-date, stale, never-run, frozen, and in-flight stages; `--why <stage>` adds field-level diff proof for deps, params, env, and cmd inputs | Core parity; Crab adds targeted explain output |
| DAG visualization | `dvc dag [-o] [--full] [--md] [--mermaid] [--dot] [--collapse-foreach-matrix] [target]` renders stage or output DAGs as ASCII, Mermaid, Markdown-wrapped Mermaid, or Graphviz DOT | `crab workflow dag [-o] [--full] [--md] [--mermaid] [--dot] [--collapse-foreach-matrix] [--format ascii\|mermaid\|dot] [--json] [target]` renders the same target/output views and adds structured JSON | Parity; Crab adds JSON |
| Verbose/debug | `-v` / `-vv` flags | `RUST_LOG=debug` / tracing levels | Parity |

---

## 7. Data Management Integration

| Aspect | DVC | Crab | Verdict |
|--------|-----|--------|---------|
| Large file tracking | `.dvc` files + `dvc add` | Pointer blobs + `crab add` (CDC chunking) | **Crab ahead** (dedup) |
| Remote storage | S3, GCS, Azure, SSH, HTTP, HDFS | S3, GCS, Azure are the currently qualified object-store paths; SSH/SFTP, WebDAV, HDFS/WebHDFS, Drive, and OSS remain parser/runtime gaps until live provider gates pass | Provider qualification pending |
| Content-defined chunking | Not supported (whole-file hash) | Gearhash CDC with 3-tier dedup | **Crab far ahead** |
| Lazy checkout / VFS | Not supported | FUSE mount, on-demand hydration | **Crab far ahead** |
| Git LFS compat | Separate tool | Built-in LFS transfer agent | **Crab ahead** |

---

## 8. Multi-Root / Discovery

| Aspect | DVC | Crab | Verdict |
|--------|-----|--------|---------|
| Multiple `dvc.yaml` files | Scans project trees with `-R/--recursive` or all pipelines with `-P`; targets can name `path/to/dvc.yaml:stage` | `crab run -R`, `crab exp run -R`, `--all-pipelines`, or `[workflow] discover` config; `path/to/dvc.yaml:stage` is accepted as an alias for the corresponding nested Crab stage | Parity |
| Stage name scoping | Flat namespace (must be unique across all files) | Dotted prefix from directory path (`data.clean`) | **Crab ahead** |
| Cross-file deps | Implicit via file path overlap | Implicit via file path overlap + `stage_out:` explicit | **Crab ahead** |

---

## 9. Side Effects & Hooks

| Aspect | DVC | Crab | Verdict |
|--------|-----|--------|---------|
| Side-effect stages | No explicit concept | `side_effects: true` + `on_cache_hit:` hook | **Crab ahead** |
| Post-stage hooks | Not supported | `on_cache_hit` command execution | **Crab ahead** |
| Watch mode | Not supported | `--watch` (re-execute on dep changes) | **Crab ahead** |

---

## 10. Gaps Where DVC Is Ahead (Action Items for Crab)

### High Priority

1. **Plot dashboard ecosystem** — Crab renders declared and ad-hoc data-series
   and image plot sources, recursive plot directories, plus multi-ref overlays to
   table, Vega-Lite JSON, or HTML, including browser opening, DVC-style
   Vega-Lite anchors, arbitrary plot IDs, custom HTML wrappers, `plots show`,
   `plots diff`, and `plots templates` listing/dump support. DVC still has
   richer VS Code/Studio dashboards and broader docs around visual comparison.

2. **Hydra custom resolver/plugin ecosystem** — Crab supports queued param
   sweeps, range expansion, comma-separated values, add/upsert/remove override
   prefixes, Hydra defaults-list config-group composition, package directives,
   nested and relative interpolation, and safe built-in resolver-style
   expressions such as `join`, `oc.env`, `oc.select`, `oc.decode`,
   `oc.create`, `oc.deprecated`, `oc.dict.keys`, and `oc.dict.values`. DVC
   still has tighter integration with arbitrary Python Hydra plugins, custom
   resolvers, launchers, and full OmegaConf expression semantics.

3. **Experiment comparison UX** — Crab stores structured experiment metadata,
   lists it with text/Markdown/CSV/JSON export, sort-by-param/metric support,
   and DVC-style changed/drop/keep param/metric filtering, and renders
   `exp diff` as text, Markdown, or JSON. DVC still has more display polish
   around dense experiment tables.

### Medium Priority

4. **Broader remote backends** — DVC supports more storage backends. Crab
   focuses on object-store remotes (S3, GCS, Azure) plus git-native data
   movement.

---

## 11. Gaps Where Crab Is Ahead (Competitive Advantages)

1. **Parallel DAG execution** — DVC is strictly sequential within a pipeline.
   This is Crab's biggest differentiator for multi-stage pipelines.

2. **Crash recovery / Journal** — DVC has no resume mechanism. A killed
   `dvc repro` re-runs everything from scratch.

3. **Retry with backoff** — DVC has no built-in retry. Users must wrap
   commands in shell retry loops.

4. **Resource-aware scheduling** — DVC has no concept of CPU/GPU/memory
   requirements per stage.

5. **Structured JSONL streaming** — DVC's output is text-based. Crab
   emits machine-parseable events suitable for desktop UIs and CI systems.

6. **Content-defined chunking** — DVC hashes whole files. Crab deduplicates
   at the chunk level, dramatically reducing storage for large files with
   small changes.

7. **Explain-miss diagnostics** — DVC offers no way to understand *why* a
   stage was invalidated beyond `dvc status` (which shows what changed, not
   the hash diff).

8. **Blake3 hashing** — Faster and more collision-resistant than MD5/SHA-256.

9. **Watch mode** — Continuous re-execution on file changes.

10. **Side-effect hooks** — `on_cache_hit` enables notifications, deployments,
    etc. without re-executing the stage.

---

## 12. Architecture Comparison

```
┌─────────────────────────────────────────────────────────────────┐
│                         DVC Architecture                         │
├─────────────────────────────────────────────────────────────────┤
│  dvc.yaml ──parse──▶ Stage objects ──toposort──▶ Sequential     │
│                                                   executor      │
│                                                      │          │
│  For each stage:                                     ▼          │
│    1. Check dvc.lock (has anything changed?)                    │
│    2. If changed: delete outs, run cmd, hash outs, update lock  │
│    3. If unchanged: skip ("didn't change, skipping")            │
│    4. Run cache: if same (cmd+deps+params) seen before,         │
│       restore outs from .dvc/cache without running              │
│                                                                  │
│  Experiment queue (separate system):                             │
│    - Copies workspace to .dvc/tmp/exps/                         │
│    - Runs full pipeline in isolation                            │
│    - Multiple workers = multiple full pipeline copies           │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                       Crab Architecture                        │
├─────────────────────────────────────────────────────────────────┤
│  crab.yaml ──parse──▶ Stage objects ──Graph::build──▶ DAG     │
│                                                          │      │
│  DagScheduler:                                           ▼      │
│    ready_queue (min-heap) ◀── source nodes (in-degree 0)        │
│    semaphore (bounds concurrency to N)                           │
│    resource_pool (CPU/GPU/memory tracking)                       │
│                                                                  │
│  For each ready stage (parallel):                                │
│    1. Compute stage_hash (blake3 of cmd+deps+params+env+flags)  │
│    2. Check local cache → remote cache → execute                │
│    3. If execute: run cmd, hash outs, write cache entry          │
│    4. Journal: write state transitions (crash-safe)             │
│    5. On success: decrement in-degree of consumers,             │
│       push newly-ready stages to ready_queue                    │
│    6. On failure: apply keep-going policy                       │
│                                                                  │
│  Experiment system:                                              │
│    - Worktree-based isolation (git worktree)                    │
│    - Metadata stored as refs in crab remote                     │
│    - Filesystem-backed queue with parallel workers              │
└─────────────────────────────────────────────────────────────────┘
```

---

## 13. Conclusion

Crab's strongest demonstrated differences are parallel execution, crash
recovery, retry, resource scheduling, structured observability, and
content-defined storage. The schema converter can preserve many DVC pipeline
fields, but conversion is not a data migration and does not authorize deleting
`.dvc/`. Keep DVC state until the repository-aware migration report and a
byte-identical clean-clone verification pass. Consult the Plan 014 gate table
before describing Crab as a replacement for any broader DVC profile.
