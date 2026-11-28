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

## URL dependency digests

Pinned URL dependencies use `b3:` followed by 64 ASCII hexadecimal characters:

```yaml
deps:
  - url:
      url: https://example.com/data.bin
      digest: b3:abababababababababababababababababababababababababababababababab
```

- Invalid pinned digests return a configuration error before network access.
- Unpinned dependencies read and hash content unless a validated external hash
  index entry has a matching strong validator, size, and credential scope.
- Digest parsing accepts upper- and lowercase hex; non-ASCII text in the
  hex field is rejected without byte-boundary panics.

## Usage

Parse a workflow and inspect its execution order:

```rust
use crab_workflow::{Graph, parse_yaml};

fn example() -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}
```

Use `crab-workflow` for parsing, planning, hashing, and state contracts. The
product command owns user-facing repository discovery and chooses when to
invoke the executor, scheduler, or status renderer.

The SDK's optional `results` feature re-exports workflow result contracts and
readers. This library is part of the SDK's package closure; external consumers
verify those exports against the packaged crates without workspace paths.

For `crab run --json` and `--jsonl`, stdout contains only structured results
and events. Child stdout and stderr stream to the CLI's stderr; stage log files,
stderr failure tails, and declared stdout capture are preserved independently.
JSONL stage events flush immediately so consumers can show progress during a run.

Stage `wdir` is relative to the repository root, including in recursively
loaded workflow files. Local path dependencies and declared outputs resolve
against that working directory; without `wdir`, recursive discovery resolves
paths against the YAML file's directory. Graph ownership uses those same physical
paths, so producers in different working directories can use the same local
filename. Explicit `stage_out.out` identifies the repository-relative artifact
(including the producer's `wdir`); it is independent of the consumer's directory.
Within a nested file, discovery prefixes local `stage_out` names and adds the
YAML directory to outputs only when the producer has no explicit `wdir`.

Cached artifact paths are repository-relative and already include stage `wdir`.
The product materializer resolves them against the invocation repository, so
experiment worktrees do not publish cache hits into the caller’s directory.

## Retry policy

Retry policy counts the initial execution in `max_attempts`. Configure both
delay fields when retries should wait: their defaults are zero, and
`max_backoff` caps even the first delay.
Parsing rejects zero attempt budgets and negative or non-finite multipliers
in both stage policies and defaults, before execution can schedule a retry.

```yaml
stages:
  download:
    cmd: "python download.py"
    retry:
      max_attempts: 4
      initial_backoff: "500ms"
      max_backoff: "2s"
      backoff_multiplier: 2
      on_exit_codes: [1]
```

Four consecutive failures with exit code 1 produce three retry delays:
`500ms → 1s → 2s`. `retry::should_retry` only decides eligibility and delay;
the product retry loop owns waiting, output cleanup, and journal transitions.

## Scheduler ownership

`DagScheduler::new` owns pre-committed stage admission. Skipped producers
satisfy dependency counts, and excluded consumers never re-enter the ready
queue. Callers do not seed skips through completion callbacks. Only dispatched
stages release reserved resources; frozen and conditionally skipped stages own
no reservation. Default failure policy stops admission, reports pending stages
as not started, and drains in-flight results before returning.

Await `SchedulerLock::acquire` inside a Tokio runtime. Contention backoff yields
to other futures, and dropping the acquisition future cancels waiting. Individual
filesystem attempts and PID writes remain synchronous; `try_acquire` makes one
attempt and reports contention without a timer.

The caller chooses the timeout and retains the guard for the protected work.
Execution and `--cache-only` replay both acquire it: replay can publish
outputs even though it does not execute a command or create a journal.

| Resource | Contract |
| --- | --- |
| Advisory lock | Establishes exclusive scheduler ownership. |
| `.lock` file | Retained after release so waiters use the same inode. |
| PID diagnostic | Best-effort holder information, not ownership proof. |
| Windows `.lock.pid` | Readable sidecar, removed before releasing the lock. |

Orphan cleanup recognizes only `.crab.tmp.<UUID>` sidecars. Callers must supply
active run IDs or hold scheduler ownership before sweeping with an empty list.
An inline run that cannot acquire the lock must leave the holder's sidecars
untouched; malformed sidecar-like names remain ordinary user files.

## Boundaries

- [`crab-types`](../crab-types/README.md) owns shared stage hashes and
  cross-crate serialized contracts.
- [`crab-storage`](../crab-storage/README.md) owns remote object access; the
  workflow cache uses it but does not redefine storage semantics.
- [`crab-git`](../crab-git/README.md) supplies Git object/ref mechanics; this
  crate owns stage dependency semantics and execution state.

## Experiment metadata identity

Remote metadata has two identities: the experiment ID selects the object, and
its metadata ref records the canonical JSON hash. `read_experiment_metadata`
checks both before returning a value. Readers that already decoded metadata can
call `ExperimentMetadata::verify_identity` with the requested ID and ref hash.

An ID or hash mismatch returns `WorkflowError::CorruptObject`. Whitespace and
JSON object ordering do not change the canonical hash.

Use `ExperimentMetadata::from_json` to check the supported schema and requested
ID while preserving JSON decoding errors. Remote reads additionally call
`verify_identity` to check the ref hash. These checks do not make two storage
reads atomic.

`collect_local_workflow_live_set` fails if metadata enumeration, reading, or
validation fails. A caller must handle that error before making deletion
choices; an incomplete set is never returned as a successful result.

## Results schema generation

Retained chart normalization shares a 2,000-point display budget across selected
series. Sampling preserves extrema/endpoints, original row ordinals and absence
transitions; plots needing all rows for transformations/aggregation refuse
oversized samples. Different captured template bytes cannot silently share an
overlay. The parser retains tagged CLI string-row serialization while keeping
nonserialized absence witnesses for numeric chart axes and missing-value gaps.
The shared template owner expands every selected y column into a distinct
run/metric series while preserving authored columns for calculations. It fills
DVC scalar, object and array anchors with their JSON types, keeps files sharing
a DVC plot ID distinct, and uses numeric x/categorical y for horizontal bars.
Literal field punctuation is escaped; private encoding aliases protect literal
backslashes from Vega-Lite 5 flattening without replacing source columns.
Native, browser and offline cursor listeners consume the same runtime signal.

Built-in confusion matrices count all observations in the shared owner before
rendering. They retain only distinct class-pair counts per run/metric between
source reads; the 2,000-cell limit applies to the expanded class grid rather than
input row count. Numeric display identities avoid Vega's string-key collisions;
labels retain missing-versus-literal distinctions and numeric ordering. The
shared Chart.reduction contract distinguishes `counted` from `sampled` results.
Authored templates retain their transformations and still refuse oversized
inputs when sampling would change their statistics. Tagged CLI JSON string rows
remain complete; Vega/HTML exports use the same exact-count owner.

Comparison reads cache each evidence body or its source-preserving read error.
Missing, denied or corrupt primary or cross-file coordinate bytes produce
classified chart availability and recovery guidance while verified curves and
captured scalar rows remain visible. Each revision's inputs are verified before
its observations enter an overlay; incompatible declarations or verified
template contents still refuse the chart.
A partial chart can have a specification; an entirely unavailable chart has none.
Provider diagnostics stay out of renderer data. Cancellation and read-admission
rejection still stop the whole operation, including a budget exhausted while
reading plot bytes. Saving or publishing a report continues to verify its required evidence
closure; a partial view does not make unavailable bytes publishable.

Reproduction retains input directory identities as closed trees, including empty
directories. The worktree owner replaces captured roots before restoring files,
so children deleted by a copied or checkpoint overlay cannot return from the
base commit. Capture bounds directories to 1,024 identities independently of the
existing file and byte budgets; incomplete trees make reproduction unavailable.

Results input witnesses come from resolved dependency hashes journaled before
execution or cache replay. The additive `stage_inputs` table leaves existing
schema-1 journal columns and readers intact; admission writes the stage hash and
input rows in one transaction. Input locations preserve repository/wdir and
explicit producer keys. URL and absolute locations are omitted, with their
content digests retained; external digests are deduplicated. Results reads cap
the total input/output witness count and input location length before allocating
unbounded data.

Stage summaries select the final attempt. Failed attempts discard stale lockfile
outputs; cache replay has no observed execution duration unless the journal
records one. Input witnesses carry a nullable location and content hash. Output
witnesses retain required path, hash and size for artifact promotion. Neither
admission nor Results changes cache publication or experiment cache ownership.

The opt-in `schema` feature derives JSON Schema for the Results DTOs and plot
configuration. The CLI enables it for its structured Results payload schema;
SDK and workflow defaults remain unchanged. Regenerate with the existing
`crab/tests/generate_schemas.rs` test and check with `schema_drift`. Client
TypeScript declarations remain generated by the Results owner tests.

Explicit Results migration converts verified experiment v1 metadata through
`ResultSnapshot::try_from(&metadata)`. `ResultOrigin::ExperimentMetadata` retains
recorded values without inventing execution or workspace evidence. The CLI owns
bounded legacy-file admission; ordinary readers accept only canonical Results.
`LocalResults::install_if_absent` uses the existing immutable writer to preserve
current captures. Invalid existing roots are errors, not a repair fallback.
Parent validation combines Windows volume prefixes with their root before
inspecting the filesystem, allowing canonical repository paths while retaining
checks for links below the system ancestors.

Metadata publication uses native file replacement on every platform. Directory
destinations are rejected without moving them aside, preserving existing
checkpoint state on failure.
