# Plan 020: Deliver one native Results workspace

> **Executor instructions:** Deliver the ordered slices below through existing
> workflow, storage, authorization and client owners. Run each slice's checks
> before advancing. Record implementation separately from retained acceptance
> evidence; a table populated from mocks does not complete this plan.
>
> **Drift check:** `git diff --stat 9ce7f64de9c..HEAD -- crates/crab-workflow crates/crab-sdk crates/crab-server crab packages/desktop packages/ui packages/web/content/docs/cli .github/workflows Cargo.lock`
> Compare changed owners against the evidence below before implementation.
> Revise affected slices before editing source if ownership, publication or
> cleanup semantics changed. Update this plan's row in `plans/README.md`.

## Status

- **Priority:** P0.
- **Effort:** L; multiple reviewable delivery slices, not one large PR.
- **Risk:** HIGH for persistence/publication; MED for client composition.
- **Category:** direction.
- **Depends on:** no other plan for local delivery. Remote delivery must meet
  the supported transport, authentication and lifetime gates below; storage-plan
  status alone does not establish readiness.
- **Planned at:** commit `9ce7f64de9c`, 2026-11-12.
- **State:** IN PROGRESS. Shared capture, comparisons, independent report storage,
  CLI/SDK/HTTP reads, desktop and browser compositions implemented. Focused proof
  and dedicated acceptance are being retained; final gate remains open.
- **Navigation decision (2026-11-18):** user requires Results inside Data Workflows,
  with no separate desktop activity-bar entry. Results remains available without
  workflow YAML.

## Outcome and accepted scope

Users queue two real experiments, search their results, choose metric/parameter
columns, select a baseline, compare actual metrics and synchronized curves,
inspect linked evidence, and act on eligible runs. Working comparison state
survives restart. A saved comparison is an immutable report recovered through
remote retrieval; source experiment rename/reset/force-push/removal cannot change
the values or curves it contains.

One workflow-owned interpretation powers CLI, desktop and optional repository
browser views. Local/direct-storage desktop use requires no Crab server.
Published objects are authoritative and indexes rebuildable. Unpublished local
result objects are durable pending-publication state, not disposable indexes.

Required: search/stable paging/sorting; configurable columns; explicit missing
results; baseline deltas and objective direction; dataset/split comparability;
synchronized curves; linked logs/stages/checkpoints/artifacts; Apply/Reproduce/
Resume/Promote; persistent comparisons; immutable reports; offline chart assets;
the full two-run/restart/cold-retrieval acceptance journey. All remain in scope
through the final gate; slices do not authorize a table-only closeout.

## Current state and evidence map

| Surface | Entry, owner and downstream contract | Proof / consequence |
|---|---|---|
| Visible experiments | `packages/desktop/src/views/WorkflowEditorView.tsx:812` mounts `ExperimentPanel`; `agent/src/workflow.rs:491` reads `ExpQueue` | Table at `src/components/workflow/ExperimentPanel.tsx:230` has ID/params/status/queued time. Queue is scheduling state, not Results authority. |
| Results | `crab/src/cmd/exp.rs:1460` calls the runner; `:1564` collects metrics; `crates/crab-workflow/src/experiment.rs:146` owns strict v1 metadata | `crab/tests/workflow_exp.rs:456`, `:501`, `:1556` cover creation/param effects/diff. Final scalars exist; durable curve/log attachments do not. |
| Cleanup | `exp.rs:1566` captures workspace only after success; `:1597` persists metadata before cleanup; `exp_queue.rs:992` retains queued logs | Immediate logs, journals/cache and failed-run plot files can disappear with temporary worktrees. Capture evidence first. |
| Execution inputs | `exp.rs:1360` overlays live copied paths; `crates/crab-workflow/src/exp_worktree.rs:175` copies local config; `src/env.rs:22` reads the current environment | A pinned commit does not identify the execution. Capture effective inputs before commands mutate them; define reproduction eligibility. |
| Plots | `crab/src/cmd/metrics.rs:493` selects show/diff; `:2832` onward parses plot data; `crates/crab-workflow/src/plot_config.rs` owns declarations | `workflow_exp.rs:779`, `:814` plus metric-module tests cover templates/images. Reuse semantics rather than adding another parser. |
| Offline HTML | `metrics.rs:1121` builds HTML; `:1129` loads CDN scripts | New HTML must render with networking denied and retain supported plot behavior. |
| Mutable metadata | `exp.rs:2534` renames metadata; `:5082` writes the current metadata path | Experiment UUID/current meta-ref is not an immutable report identity. |
| Transport | `exp.rs:3218` push and `:3347` pull move workspace/checkpoints/metadata/stage refs | They do not transfer every cache manifest/xorb or logs. Experiment execution sets `cache_push:false` at `:1447`; failed runs lack pushable success snapshots (`:3242`). |
| Cache | `crates/crab-workflow/src/stage_cache_entry.rs:97` enumerates outs/metrics/plots; `cache.rs:820` publishes bytes before manifest/ref | Respect cache/push policy and named remotes. Captured Results evidence is distinct from an executable cache entry. |
| Lifetime | `exp.rs:3251` force-push deletes the old remote experiment; `:3642` removes its namespace | Reports pointing only into `workflow/exp/<id>/` can break. Reports need independent content/lifetime. |
| GC | `crates/crab-workflow/src/gc.rs:70` collects local roots; production repo GC enters `crab/src/cmd/gc/mod.rs:3051` | Other workflow walkers there are test-only. Production candidates at `:3355` cover v2/derived-index prefixes; no existing production report-root traversal can be assumed. |
| Desktop reads | `agent/src/handlers.rs:79` obtains `BrowseSession`; workspace verbs use `get_work_dir` | Bare remote Results cannot depend on editable YAML or a local worktree. |
| SDK | `crates/crab-sdk/src/api/remote.rs:32`, `packages/desktop/agent/src/repository.rs:219` | Preserve captured repository identity, operation limits, cancellation and connection ownership; no raw-store exposure or second provider resolver. |
| UI lifetime | `electron/main.ts:523` routes by repo/session; `src/state.tsx:243` defines `repositoryKey`; `src/lib/session-manager.ts:173` owns view state | Discard stale replies; persist bounded preferences, not native session IDs or result payload caches. |
| Browser | `crates/crab-server/src/api/mod.rs:226` applies read grant/admission/cancellation/paging | Routes remain inside common HTTP security middleware. Cellar cannot become competing Results authority. |
| Promotion | `crab/src/cmd/artifacts.rs:350` calls remote CAS promotion | `exp promote` creates a branch. Artifact promotion is a different action. |

Load-bearing excerpts for drift checks:

```rust
// packages/desktop/agent/src/workflow.rs:491
pub async fn workflow_exp_list(work_dir: &Path) -> Result<Value> {
    let queue = crab_workflow::ExpQueue::new(work_dir.join(".crab/exp-queue"));
    Ok(json!({ "experiments": queue.list_all()? }))
}
```

```rust
// crab/src/cmd/exp.rs:1564
let metrics = collect_metric_values(&tmpdir_path, &declared_metrics);
if dag_result.is_ok() {
    capture_workspace_snapshot(repo_root, &tmpdir_path, &base_commit, &exp_id)?;
}
```

`ExperimentMetadata` has `#[serde(deny_unknown_fields)]`, schema version 1,
an `ExperimentId`, and `metrics: BTreeMap<String, serde_json::Value>` at
`crates/crab-workflow/src/experiment.rs:146`. It is a completed observation,
not an immutable attachment/report catalog.

## Conventions and dependency contracts

- Root/scoped `AGENTS.md` apply. Rust uses owner-specific `Result`/`thiserror`,
  source-preserving errors, Tokio cancellation, bounded I/O and no production
  panics. Reuse `WorkflowStore` and its writer admission in
  `crates/crab-workflow/src/store.rs`.
- Inspect reachable release tags before changing an existing stored/wire/CLI
  contract. Results gets its own schema; adding fields to strict experiment v1
  readers is not automatically compatible. No speculative legacy readers.
- `packages/desktop/DESIGN.md:5`: “Use Vercel Geist's neutral visual language
  with shadcn/ui composition.” Reuse semantic tokens, controls, bundled fonts,
  compact rows, focus/high-contrast/reduced-motion behavior. Browser conventions
  live in `packages/ui/README.md`. Keep React 18/19 compositions independent.
- Generate both clients' Results declarations from actual owner DTOs, following
  the owner-test pattern in `crates/crab-server/src/api/contracts.rs:11`.
  No handwritten duplicate response types or runtime TS-export dependency.
- SDK additions follow `crab/docs/architecture/crab-sdk.md` and
  `crab/docs/architecture/crab-sdk-api.md`: narrow qualified capability,
  unchanged defaults, no leaked CLI/provider/server implementation types.
  Re-exporting workflow Results values requires documented semver obligations.
- Primary contracts checked: [ts-rs 12.0.1](https://docs.rs/ts-rs/12.0.1/ts_rs/)
  supports test-driven generation and `serde-json-impl`;
  [Vega-Lite data](https://vega.github.io/vega-lite/docs/data.html) supports inline
  data and external URLs; [Vega Embed](https://github.com/vega/vega-embed)
  supports module bundling. Check exact selected package source/types before
  using APIs. Preserve current Vega-Lite v5/DVC plot behavior; no chart major
  upgrade, dependency patch/override, or vendor-source modification.

## Target contracts and ownership

### Results, live state, comparisons and reports

Create a focused `crates/crab-workflow/src/results/` owner. Add contract/query/
plot/storage submodules only where actual callers justify them. Export the
narrow snapshot/query/attachment/report surface used by all three products.
Queue records may enrich pending/running rows but never replace result records.

| Concept | Meaning |
|---|---|
| Experiment ID | Existing UUIDv7 identity; distinct from stage, attempt and checkpoint IDs. |
| Result snapshot | Immutable observed values, source metadata digest, resolved params, code/input identities, outcome, stage summaries and attachment availability. |
| Live projection | Current labels/queue state and provisional evidence from the admitted owner; not a sealed report. |
| Working comparison | Mutable repository-scoped selection/baseline/columns/filter/sort/axes preferences. |
| Saved report | Content-addressed frozen comparison specification, displayed metadata/values, selected plot/config/media evidence, provenance and renderer identity. Editing creates another report. |

Scalar identity is `(normalized repository-relative file, escaped field path)`;
use JSON-pointer-equivalent escaping so dotted keys cannot collide with nesting.
Preserve numeric/string/bool/null versus missing/invalid/not-captured/corrupt/
denied/unsupported states. Missing is not zero. Store resolved params separately
from overrides. Capture typed reproduction intent; never execute stored
`cli_args` as a command string.

Capture a separate `ReproductionSpec` after source checkout, Hydra/parameter
overrides, copied-path overlays and any checkpoint application, but before the
DAG can execute. Record the effective source/overlay tree, workflow bytes,
target/selection policy, resolved parameter bytes, nonsecret execution policy,
and declared runtime/environment prerequisites. Per-stage input identities
must come from the same admission/hash witnesses the engine uses, including
produced/external inputs; never reconstruct them by rehashing paths after a
command may have changed them.

Eligibility requires retained/verified source and required input bytes plus
verifiable declared execution prerequisites. Missing, mutable external or
unverifiable environment inputs produce explicit unavailable reasons. Do not
persist an entire `.crab/local.toml`, inherited environment, credentials or
credential-derived fingerprints in result/report data. Capture only nonsecret
effective policy and declared nonsecret prerequisite evidence; resolve fresh
credentials through the existing auth owner. If required policy cannot be
separated safely, exact reproduction is unavailable. The new reproduction path
must not reapply today's copied paths/config or silently inherit a changed
prerequisite. Preserve ordinary run/resume semantics while adding this typed
restoration boundary to the existing worktree owner.

Dataset/split bindings refer to observed input artifact digests and explicit
split identity. Do not infer semantics from filenames. Unknown stays unknown.
Objective direction is declared comparison metadata, never guessed from metric
names. These choices need no new config/env option. Absolute delta is candidate
minus baseline; relative delta is unavailable for zero baseline. Improvement
coloring requires declared direction and comparable evidence; unverified raw
differences may be shown with their reason. No significance claim from one run.

### Evidence and independent lifetime

Capture declared metric/plot bytes and configuration, stage-attempt summaries
and bounded logs before cleanup for both immediate and queued runs. Preserve
partial/failed evidence without manufacturing a successful apply snapshot.
Execution outcome and capture outcome are separate; expose capture errors and
never mask the primary DAG failure. Older supported experiments lacking captured
evidence remain explicitly not-captured; do not fabricate or rewrite metadata.

Attachments identify exact bytes/hash/size/media kind, source stage/attempt/path
and access policy. Models/datasets link to immutable content/artifact versions;
do not eagerly copy whole models into reports. Workspace paths, mutable labels
and signed URLs are not historical identities. Unavailable history never falls
back to current workspace or latest version.

Validate this initial layout in slice 1, then use owner helpers everywhere:

- `workflow/results/objects/<b3>`: immutable evidence bodies.
- `workflow/results/runs/<experiment-id>/<b3>.json`: immutable result manifests.
- `refs/crab/results/<experiment-id>`: published sealed manifest identity.
- `workflow/results/reports/<b3>.json`: immutable comparison reports.
- `refs/crab/results-reports/<b3>`: independently retained report roots.

These control roots are workflow object-store records, not advertised Git
commit refs. Never interpret their BLAKE3 targets as SDK Git `ObjectId` values.
Run discovery pointers may advance only by explicit verified CAS; snapshots and
report roots stay immutable. Readers pin the resolved manifest digest for the
operation, and reports never re-resolve a mutable run pointer while rendering.

Validate identity/schema/paths/size before materialization. Publish evidence
before the root with existing conditional-create/CAS and writer fences. Pull
verifies a requested closure in temporary state before atomic installation.
Interrupted operations are idempotent and never acknowledge an incomplete root.

Reports own the frozen bytes necessary to render selected data/plots/config and
displayed metadata. Their lifetime is independent of experiment force-push,
reset/removal and journal cleanup. External artifact/checkpoint links may expose
availability but cannot promise retention/reproduction without a retained
closure. Respect `push:false` and named-remote access; excluded evidence remains
explicitly excluded, or publication refuses a report requiring it. Logs are
opt-in report content. Never embed inherited environments, credentials,
connection configuration or signed URLs.

Extend experiment push/pull with captured Results, not implicit executable
stage-cache publication. Failed Results must publish without a success workspace.
Independent report push/pull/discovery uses report roots. Initial retention
keeps data while its experiment/report root is retained; no new automatic
duration or general report GC is required. Prove protection/candidate exclusion
in active production repo/bucket GC, not only test helpers. Do not add shared
xorb references without integrating their actual lifetime owner.

### Bounds, chart behavior and client state

Initial proposed constants: 100 default/200 maximum rows per page; 32 selected
scalar columns; 20 compared runs/series; 2,000 display points per series; 2 MiB
normalized plot response. Bound manifest size/nesting/scalar count/log windows
before allocation. Oversized requests fail explicitly. Original evidence stays
accessible. These are new acceptance targets, not current measured support.

Filter/sort before paging, with ID tie-break and explicit missing ordering.
Bind cursors to repository/query/catalog generation. Reject stale cursors.
Indexes can accelerate reads, but absent indexes must show rebuilding/progress
and bounded partial availability, not a false empty list or an unbounded scan.

Move reusable parser/template/spec logic from `crab/src/cmd/metrics.rs` to the
shared workflow owner. Keep source resolution separate for captured runs and
workspace/Git-revision plots. CLI retains output/terminal/browser-opening policy.
Preserve declared/ad-hoc plots, images, cross-file x/y, DVC anchors and overlays.
Delete the old duplicate logic when switching callers.

Both clients bundle compatible chart dependencies. Select a reproducible,
licensed CLI runtime asset/source-distribution mechanism in slice 1. Standalone
installed CLI exports must need neither runtime npm nor network. Cover Make
install, native desktop staging and native release producers; no hidden npm
downloads in Cargo build scripts or static fallback losing supported plots.

Synchronize cursor/zoom across compatible x domains/units. Disclosed decimation
preserves extrema/endpoints; smoothing is display-only. Missing points stay
gaps. Never derive summary metrics from displayed samples. Results renderers
must not fetch external plot/template/image URLs or execute supplied HTML/JS;
verify isolation/CSP/loader behavior against selected dependency source.

Desktop preferences are a bounded validated `workflowResultsV1` object merged
into `SessionManager.viewState`, keyed by `repositoryKey(repo)`. Persist pinned
run digests/baseline/columns/filter/sort/axes, not sessions/credentials/payloads.
A remote saved comparison is a report, not a localStorage-only selection.

## Scope

Modify only these existing owners and focused new descendants:

- `crates/crab-workflow/src/{lib.rs,error.rs,experiment.rs,exp_worktree.rs,plot_config.rs,params_runtime.rs,stage_cache_entry.rs,store.rs,gc.rs}`;
  new `src/results/` and owner-local tests. Existing contract changes need the
  release inventory described above.
- `crab/src/cmd/{exp.rs,exp_queue.rs,metrics.rs,mod.rs,artifacts.rs,workflow_journal.rs}`,
  `crab/src/main.rs`, `crab/src/core/error.rs`; new `crab/src/cmd/results/`.
  `crab/src/cmd/run.rs` only if existing admission/lock witnesses require a
  narrow Results capture hook; no scheduler/executor policy change.
  `crab/src/cmd/gc/` changes limited to Results lifetime/candidate proof.
- `crab/src/cmd/version.rs`: advertise the new `workflow.results` envelope
  through the existing schema registry. Match the actual Results envelope;
  preserve existing registry entries and wire shapes.
- `crab/src/auth/mod.rs`: the existing direct repository-store constructor must
  classify through the managed profile owner before credential resolution.
  Results/artifact/status/cache and experiment-write callers share this boundary;
  an installed custom authority must never become a direct bucket. Bucket-level
  construction and explicitly resolved managed reads keep their existing owners.
  Add refusal/cancellation proof in the auth/workflow owners and actual Results
  CLI tests; do not introduce a resolver stack or change execution/cache policy.
- New `crab/tests/workflow_results.rs`, existing `workflow_exp.rs`, Results
  schema additions under `crab/schemas/`; no silencing inventory changes.
- `crates/crab-sdk/{Cargo.toml,src/lib.rs,src/client.rs,src/api/remote.rs,src/repository.rs,src/error.rs}`;
  focused Results operation module/tests. Keep minimal/default consumers
  unchanged; a needed opt-in capability must meet the SDK design/gates.
- `packages/desktop/agent/src/{main.rs,handlers.rs,repository.rs,workflow.rs,shellout.rs}`,
  focused Results session/handler/tests; agent manifest for a qualified SDK
  capability only if needed.
- Desktop `src/components/{RepoDetail.tsx,workflow/WorkflowTabs.tsx,workflow/ExperimentPanel.tsx}`,
  `src/views/WorkflowEditorView.tsx`, `src/ipc.ts`, `src/lib/session-manager.ts`,
  new focused `src/components/results/` and result-state modules;
  `electron/{preload.ts,main.ts}` for typed routing/admission only.
- Browser `src/features/repository/page.tsx`, `src/lib/{api.ts,repository-route.ts}`,
  new `src/features/results/` and generated Results declarations for both clients.
- `crates/crab-server/{Cargo.toml,src/api/mod.rs,src/api/contracts.rs}`;
  new `src/api/results/` and focused repository read adapter. Existing auth
  mechanics stay with their owners.
- Relevant client manifests/lockfiles, chart source/build assets, CLI asset/build
  integration and declaration-generation scripts/tests. No unrelated upgrades.
  Cargo manifest changes may update root `Cargo.lock`, with security review;
  CLI build integration is limited to `crab/{Cargo.toml,build.rs,Makefile}` and
  the Results asset source/output paths fixed in slice 1.
- `crab/scripts/check-sdk-features.py` and its focused tests: include the actual
  Results feature combinations and enforce its workflow-owner boundary. Native
  workflow CI runs portable Results CLI regressions on every native runner,
  existing POSIX experiment/plot regressions, schemas and SDK slices. Bind both
  native-workflow and desktop CI producers into qualification source identity;
  keep build and smoke consumers on one isolated Cargo target directory.
- Focused client test trees; canonical `crab/scripts/qualify_results.py`
  and `verify_results_evidence.py`; narrow gate additions to
  `.github/workflows/{workflow-native.yml,desktop.yml,http-server-container.yml,release.yml}`.
- `packages/workflow-results/` holds generated owner DTOs and pure, validated
  preference/display functions shared by React 18/19 clients. Workflow Rust keeps
  comparison, evidence and storage semantics. The canonical dedicated smoke
  orchestrator is `crab/scripts/qualify_results.py`.
- `packages/desktop/docs/guides/workflows.md`, Results docs/reference under
  `packages/web/content/docs/cli/`, new `crab/docs/design/native-results.md`,
  affected SDK API/capability docs and `plans/README.md`.

Out of scope: DVC migration, Python logger/SDK, adaptive search, GPU/container
scheduler policy, generic event bus, MLflow/W&B wiring, semantic model/data
diffs, per-example evaluation, promotion quality/approval gates, mandatory
service, generic GC/storage redesign, unrelated dead-code removal. Generated
`.source/`, baselines, inventories, ignores and expected failures are not
escape hatches for checks.

## Commands and environment

Run from repository root unless stated. Before any compiling command, inspect
`df -h "$HOME/Workspace"` and verify its resolved mount. Create only this
worktree's target; stop if unavailable, never fall back to local `target/`:

```sh
test -d "$HOME/Workspace"
test -w "$HOME/Workspace"
mkdir -p "$HOME/Workspace/crabbuild-target/crab-ios-fa0c00ee-results"
test -w "$HOME/Workspace/crabbuild-target/crab-ios-fa0c00ee-results"
```

Set `CARGO_TARGET_DIR` on every compiling invocation, including npm scripts
that call Cargo. Desktop declares Node 22.12+ within Node 22. Missing deps:
follow root `AGENTS.md`, inspect artifact lookup, use `make install` for Rust
and `npm install` for JS, retry once and report first actionable error. Never
`cargo install` or manually install/copy binaries.

| Purpose | Command | Expected |
|---|---|---|
| Core | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ios-fa0c00ee-results" cargo test -p crab-workflow --locked --lib results` | New tests execute and pass. |
| CLI | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ios-fa0c00ee-results" cargo test -p crab --locked --no-default-features --features simd-accel,tier,watch --test workflow_results` | New integration tests execute and pass. |
| Existing experiments | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ios-fa0c00ee-results" cargo test -p crab --locked --no-default-features --features simd-accel,tier,watch --test workflow_exp` | Existing behavior/contracts pass. |
| Sidecar | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ios-fa0c00ee-results" cargo test -p crab-desktop-agent --locked workflow_results` | Binary-target tests execute; no nonexistent `--lib`. |
| Shared/desktop state | `npm --prefix packages/desktop test -- tests/results-preferences.spec.ts tests/session-manager.spec.ts tests/session-round-trip.spec.ts` | Electron-ABI wrapper passes shared preferences and native persistence. |
| Desktop checks | `npm --prefix packages/desktop run typecheck`; `npm --prefix packages/desktop run lint` | Exit 0. |
| Browser state | Shared preference checks above; `npm --prefix packages/ui run test:browser -- results.e2e.ts` | Canonical shared state plus browser route/account isolation; no duplicated model. |
| Browser checks | `npm --prefix packages/ui run typecheck`; `npm --prefix packages/ui run build`; `npm --prefix packages/ui run format:check` | Exit 0; UI assets built before server Cargo. |
| Server | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ios-fa0c00ee-results" cargo test -p crab-server --locked --lib api::results` | Route/auth tests execute and pass. |
| Existing contracts | `CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ios-fa0c00ee-results" npm --prefix packages/ui run contracts:check` | Generated types match. Extend owner-driven export/check for Results. |
| Browser E2E | `npm --prefix packages/ui run test:browser -- results.e2e.ts` | Tests pass; mocked HTTP alone is not Level 3. |

Focused checks are local; broad suites, packaging, cloud and cross-OS proof use
CI/dedicated environments. Add exact SDK feature/minimal checks from slice 1.
Never accept zero-test selections. New test/module names above are deliverables,
not commands claimed to pass on the planning commit.

## Ordered delivery slices

### 1. Freeze contracts and implementation boundaries

Create `crab/docs/design/native-results.md`. Inventory release contracts and
consumers; record source capture, publication, cleanup, active GC, size limits,
transport capability and asset producer/consumer decisions. Validate the new
namespace through direct and managed read/publication paths; unsupported paths
must refuse explicitly. Decide CLI-runtime asset distribution and narrow SDK
read exposure before changing existing formats, updating this plan's scope if
another owner is necessary. No hidden build/runtime fallback.

Define `ReproductionSpec` capture timing, required witnesses, nonsecret policy
projection and eligibility/refusal cases here. Trace checkout, local-config
copy, Hydra overrides, copied paths and environment sanitization together;
source commit alone is not sufficient evidence.

**Verify:** consumer searches record callers/callees/tests for every changed
contract; `git diff --check` exits 0. Every requested capability maps to a final
acceptance check. This is a design gate, not functional acceptance.

### 2. Implement shared snapshots and comparison queries

Add schema/identity/bounds validation, scalar identity, availability, canonical
snapshot encoding, comparison math, query/cursor semantics and attachment
descriptors in `crab_workflow::results`. Generate client declarations from
owner DTOs using the existing test-export pattern; constrain JSON integers to
safe wire representation. Keep semantic decisions out of renderers.

**Verify:** core tests cover canonical hash, corruption/schema mismatch,
missing/null/zero, field collisions, zero baseline, unknown direction/provenance,
paging/cursor/budget errors. Export/check types; a second generation is unchanged.

### 3. Capture evidence before cleanup

Integrate pre-execution `ReproductionSpec` capture into `run_exp_run_with_id`
at the defined boundary, and post-execution observed evidence before teardown.
Converge immediate/queued Results log ownership while preserving queue-log
contracts. Capture resolved params, declared metrics/plots/config, attempts and
typed reproduction intent. Seal outcomes/availability explicitly, preserving
primary execution errors and partial evidence. Existing unsupported evidence
states stay visible instead of using current-workspace data.

**Verify:** CLI tests execute real argv/shell fixtures for success, failure,
cancellation and cache hits. Failed immediate logs/plots survive cleanup.
Changing the caller workspace leaves historical results unchanged. Existing
`workflow_exp` remains green. A fixture command mutates its params/copied input
after starting; recorded reproduction inputs remain those used at admission.

### 4. Share plot semantics and bundle offline assets

Extract current parser/templates/spec construction into the shared owner;
separate captured-source from workspace/revision resolution. Switch CLI and
delete duplicate old logic. Implement the agreed runtime producer/distribution,
synchronized axes/cursor and disclosed bounded display-only decimation.

**Verify:** existing plot regressions and new parity tests pass. A browser opens
CLI-generated HTML with all networking denied and verifies actual curves/images,
tooltips and template plots without console errors. Asset reproducibility,
digest/license checks and standalone installed-CLI offline proof pass.

### 5. Transport results and independent immutable reports

Implement evidence-first/root-last publication and verified atomic pull.
Extend `exp push/pull` with Results without implicit cache publication. Add
`crab results list/show/compare` and `crab results report
create/show/export/push/pull`, structured schemas/help and shared-owner calls.
Report discovery is independent of the mutable experiment catalog. A failed
run can publish evidence without claiming an apply snapshot.

Protect report content from exp remove/reset/force-push and journal cleanup;
test actual production GC exclusions/roots and writer fences. Preserve policy
exclusions rather than silently uploading forbidden attachments.

**Verify:** CLI/owner tests round-trip two runs/report through real filesystem
object storage into an empty checkout. Interrupted uploads expose no incomplete
root; interrupted pulls install nothing partial. Rename/remove/republish source
experiments and discard indexes: report values/curves stay identical. Corrupt
bytes, unavailable remotes and policy exclusions are explicit. Dedicated
RustFS proof precedes remote acceptance.

### 6. Wire bounded native reads and correct actions

Expose shared reads through existing SDK/BrowseSession operation lifetimes,
including bare remotes. Add typed IPC/preload routing with repository/native
session capture, cancellation, bounds and late-response rejection. No
`get_work_dir` requirement for reads; no new provider resolution stack.

Writes use existing supervised CLI owners and workspace capabilities. Add
`crab exp reproduce <id> --json` using captured typed intent, source code snapshot
and verified input identities through the existing experiment-worktree runner;
create a new ID linked to the source. Never replay an untrusted CLI string or
label current-HEAD execution exact reproduction. Missing required inputs or
environment evidence returns an actionable unavailable reason.
Use the worktree owner's typed restoration path to prevent current copied paths
and local configuration from overwriting the captured state.

Apply preserves the existing explicit overwrite behavior and previews affected
paths, including deletions and a local-edit warning, before confirmation. The
existing CLI has no dirty-workspace rejection to reuse. Resume uses
`exp run --resume`, showing the checkpoint and actual base
commit; its current default is current HEAD. Artifact promotion uses immutable
version IDs and existing CAS `--expected`. `exp promote` branch creation stays
separately named. Bare/read-only sessions explain unavailable actions. Browser
scope here is read/export, with the same capability explanations.

**Verify:** sidecar tests exercise local/bare-remote/session replacement,
cancellation, denied actions and exact-source opening. CLI tests reproduce
after HEAD changes, resume checkpoints, apply recorded deletions and detect CAS
conflicts. SDK exact-feature and minimal/default checks pass.
Change copied inputs, local execution configuration and declared runtime/env
prerequisites after the source run: reproduction must restore/verify the
recorded state or refuse visibly, and must never execute today's replacements.

### 7. Replace the desktop queue table with Results

Make Results repository-scoped within Data Workflows, reachable without a
selected/present/parseable YAML. Retain queue controls but remove the competing
queue-derived run table. Add the query-driven table, column chooser/search/sort,
baseline/curves/evidence panels and capability-aware actions. Reuse exact-source
file viewers/output dock. Persist bounded working preferences in SessionManager;
save comparisons as reports. Loading/no runs/filtered-empty/denied/offline/
corrupt/partial states remain distinct; do not catch errors into an empty list.

**Verify:** desktop checks and state tests pass, including repository switches,
late replies, corrupted/oversized preferences and restart. Packaged acceptance
in slice 10 establishes real visible data, not only a mocked component.

### 8. Add the optional repository browser view

Add `api::results::routes` inside existing repository-authenticated routing.
Reuse resolved store/layout, shared interpretation, read grants, admission,
cancellation and 100/200 paging. Every report/attachment belongs to the admitted
repository; IDs are not grants. Do not fetch arbitrary user URLs.

Add lazy `/owner/repo/results` navigation and pinned report/run routes to
`packages/ui`, generated types and `useRequest` cancellation. Authenticated
report links retain their immutable identity and recheck current access.
Static HTML is a separate explicit export. No server-owned result database.

**Verify:** build UI then run server tests. Real identities exercise allowed/
denied repositories, revocation, cross-repo attachment IDs, stale cursors and
cancellation. Browser tests prevent previous-route data flashes. Real HTTP/
storage proof compares browser and CLI values.

### 9. Measure bounds and document behavior

Exercise a 10,000-run disposable catalog at the defined row/column/series limits.
Record CI hardware and cold/warm timing. Proposed budgets: 200-row table
interactive within 250 ms after data arrival; bounded comparison interactions
within 100 ms; chart initial render within 500 ms. Changes need documented
product rationale, not relaxed tests hiding regressions. Measure heap/readers/
handles across repeated switches, reloads and chart destruction.

Document command/result states, objective/provenance meanings, reports/offline
export, publication/retention boundaries, actions and reproduction requirements.
Update SDK capability docs for the exact implemented slice.

**Verify:** retained scale evidence includes timings, memory/handle envelope,
bounded responses and rebuild/sampling indicators. Keyboard/theme/high-contrast/
a11y checks pass. For web docs changes, run web typecheck/lint/tests/link checks.

### 10. Retain complete acceptance evidence

Add `tests/e2e/workflow-results.spec.ts` to the dedicated packaged gate beside
`.github/workflows/desktop.yml:270`. Set `E2E_SMOKE=1`; the default project
otherwise intentionally selects nothing. Add a Results smoke producer/verifier
with a required named capability set, source SHA, binary/package/runtime digest,
platform/provider and workflow run/attempt. No credentials in evidence.

**Verify:** the full journey below passes against packaged clients, actual
server routes and isolated real object storage. Verifier tests reject missing,
false, stale or differently bound checks and zero relevant tests.

## Acceptance journey and test plan

Use two deterministic real experiments, not preloaded result JSON. Their
scripts produce nested metrics (including zero), resolved params, a loss CSV,
an image plot, a model, logs and acknowledged checkpoint state. Different
parameters produce known numeric and curve differences.

1. Queue both from packaged desktop and execute the bundled CLI. Verify actual
   file bytes plus visible run IDs/values. Immediate runs use the same evidence
   owner; synthetic 10,000-run data is only supplementary scale proof.
2. Search/sort/select columns/baseline/objective and dataset/split binding.
   Assert exact deltas, zero-baseline handling, missing and comparability states.
3. Overlay curves/synchronize axes; open captured logs and model metadata.
   Change workspace files afterward: historical values and opened evidence stay
   fixed at the captured identity.
4. Exercise Apply, reproduce after HEAD moves, checkpoint Resume and artifact
   promotion/CAS conflict. Inspect actual files/new IDs/refs, not success toasts.
5. Save a report. Close/reopen the app with the same profile: working selection,
   baseline, columns and values return.
6. Publish through real transport to an isolated endpoint. Remove access to
   original worktree/cache, retrieve in another checkout/app profile and recover
   the same saved comparison. Rebuild disposable indexes from canonical objects.
7. Open through a real authorized server. A second identity cannot enumerate or
   read denied reports/attachments. Revocation removes the first identity's
   access; knowing a report ID does not grant access.
8. Rename/reset/remove/force-publish source experiments, clean journals and run
   safe repo-scoped maintenance in the isolated namespace. Report contents stay
   identical. Never run bucket-wide GC.
9. Open exported HTML with network denied: curves/images/selected data work,
   without CDN/service/token dependence or executing supplied HTML/JS.
10. Fail/cancel a run; corrupt/remove evidence; revoke a native session during a
    read; disconnect storage. Each state is visible and preserves other runs.

Restored artifacts must be byte-identical. Newly executed ML runs follow their
declared reproducibility/tolerance contract; exact stored-byte reconstruction
does not prove arbitrary GPU training is deterministic.

Test placement and patterns:

- Core `results`: identity/schema/math/provenance/cursors/bounds and plot parity.
- CLI `workflow_results`: real execution/capture/actions/transport/report lifetime.
  Preserve `workflow_exp` checkpoint (`:326`), queue logs (`:614`), apply/deletion
  (`:1287`, `:1322`), rename (`:1439`) and remote round-trip owner tests.
- SDK/sidecar: pinned repository/session/operation, bare remotes, cancellation,
  budget/error semantics and minimal feature gates.
- Shared/desktop `tests/results-preferences.spec.ts`, session tests and packaged
  `tests/e2e/workflow-results.spec.ts`; use
  `tests/e2e/workflow-complex.spec.ts:11` and `packaged-app.ts:63` as patterns.
- Server `src/api/results/tests/mod.rs` and `tests/browser.rs`: real storage/auth;
  browser `tests/browser/results.e2e.ts` plus shared preference tests above.
  Browser HTTP mocks supplement, not replace, Level-3 real-server proof.
- Evidence verifier: exact required capability set and source/package binding,
  including rejection of missing/stale/zero-test evidence.

Dedicated packaging (CI/test environment, not routine local proof):

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ios-fa0c00ee-results" npm --prefix packages/desktop run package:dir
```

Then with working directory `packages/desktop`:

```sh
E2E_SMOKE=1 npx playwright test tests/e2e/workflow-results.spec.ts --project=e2e-smoke --workers=1
```

Use Xvfb on Linux as existing CI does. Qualify supported native OS packages
and record tested provider/managed profiles. Test real release assets, not a
development renderer with a different CLI.

## Git workflow and done criteria

Local qualification retained on 2026-11-18, macOS arm64 with owned RustFS 1.0.3
storage: real two-run CLI journey, checkpoint Apply/Resume, production repository
GC/report lifetime, authenticated HTTP/S3 reads, three packaged desktop/offline
tests and two browser tests plus the real-HTTP browser journey all passed. Native acceptance opens Data Workflows →
Results, verifies no standalone navigation entry, queues two real runs, compares
0.1/0.2 metrics and two synchronized curves/image plots, publishes a report,
restarts, then retrieves/reopens it in a cold clone without workflow YAML.

The captured comparison is now the first content in a report-only repository.
Unit/layer curves fit the actual native panel and 390px browser width with visible
legends. Comparison tables keep their Run column while scrolling; unavailable
actions explain their prerequisites. Fresh finishing reviews marked the scoped material corrections resolved; the
documenter confirmed incumbent tokens and system files were preserved. The
latest review includes forced colors and the labeled narrow identity layout.

Focused proof includes 25 Results owner tests, the alternate JSON ordering
feature, SDK bare-storage/budget/cancellation reads and minimal features, Apply
deletion/preview and checkpoint regressions, 36 preference/session tests, type
checks, focused lint, native/browser builds, generated runtime/contracts, and
the evidence verifier. First-session persistence now uses the shared session
writer, so a repository need not be switched away from before saving preferences.

Measured debug catalog scan at 10,000 runs: 4.86s cold / 5.06s warm, 644,892-byte
200-row/32-column response. Browser rendering after data arrival: 120.4ms. Two
real-run chart overlays initialized in 94.7ms; linked cursor update took 33.6ms.
Heap growth after ten report switches: 855,056 bytes. Other comparison
interactions and maximum-series/custom-template budgets remain open.

The final gate remains open for the remaining full failure/platform matrix and
CI/release qualification. Local macOS proof is not a claim that every supported
OS/provider or every failure path has been qualified.

Additional focused proof on 2026-11-18: failed immediate/queued runs retain
partial metrics, curves and logs after cleanup; interrupted queue execution
preserves an earlier successful run. Interrupted report upload/download exposes
no partial root, retry succeeds, and both GC writer admissions are released.
The native read test covers local and bare repositories without workflow YAML
and rejects reads using closed session scopes. All 36 existing `workflow_exp`
regressions and 35 shared preference/session persistence tests passed.

The refreshed native fixture exposed an asynchronous column-picker bug. Both
clients now update picker state immediately and atomically update query and
comparison preferences. Comparison columns distinguish automatic (`null`), empty
(`[]`) and explicit choices; core regression covers all three. Frozen dataset,
split and evaluator identities are inspectable as read-only report fields.
The CLI uses typed Results payloads and generated `workflow.results` JSON Schema;
schema drift/validation checks and 21 Results owner checks passed. Actual Results
CLI responses in the dedicated journey validate against that schema. Refreshed
full qualification passed against the current source and packaged application: 32
named capability checks, including actual metrics/curves, failed-run display,
restart, cold retrieval, HTTP grants, frozen identities and forced-color charts.

The mutation fixture exposed a runner bug: lockfile dependencies were rehashed
after commands could overwrite them. Inline, YAML/watch and parallel DAG results
now carry admitted file hashes/sizes through `StageExecution` into lockfile writes.
Serialized lockfile shape stays unchanged. The real mutation/reproduction case
and 78 adjacent experiment, cache, directory, wdir, explain-miss and DAG tests
passed. Directory/external/produced-input witness coverage still needs its final
contract audit; this fix proves regular file admission.

The forced-color inspection exposed pale chart labels after a dark-to-forced
transition. Shared runtime styles now use live system colors; categorical curves
also retain unique dashes. Browser proof checks painted SVG contrast and series
signatures. Axe 4.13's foreground calculation reads Chromium's original
`-webkit-text-fill-color` under forced colors, so ordinary light/dark/narrow Axe
checks remain separate from that direct painted-color check. The forced capture
uses an actual viewport: a minimal dependency probe showed full-page screenshot
resizing can reset painting while the forced-color media query remains true.

Explicit remaining implementation/proof audit: Results-specific cache-hit capture;
managed transport support/refusal qualification; remaining native OS packages;
the full comparison-interaction budget and transformed/compound/custom axis
semantics. Chart initialization and linked cursor
timing are measured in the refreshed browser journey; high-contrast captures
were independently inspected. The other gates remain open; the earlier two-run proof does not complete them.

The report-retention and custom-parameter audits are implemented: report bodies
are restricted to metric/plot/image/template/workflow evidence and optional logs;
model/input/checkpoint payloads remain explicit immutable links. Authored plot
parameters survive owned interaction injection, and reserved-name conflicts fail
visibly. Twenty-five owner checks pass, including a cold report round trip with
saved axes and an excluded model body.

Compatible simple line charts now share validated numeric x ranges. Working
ranges persist with selections; reports freeze them, while inspection overrides
stay temporary and Reset restores the frozen range. Real-storage core and
packaged native restart/cold-retrieval journeys passed with these ranges. The
offline cursor assertion exposed Vega-Lite's inherited-field validity filter
discarding independent empty rule data. Materializing the rule's x field fixes
that compiler interaction; the visibility assertion is retained. Fresh combined
qualification passed against the current source and rebuilt Darwin-arm64
application: all 32 named checks, 12 relevant integration tests, offline cursor
lines after zoom/reset, working-range restart and frozen-range cold retrieval.

The axis-control finishing review also exposed form remounts dropping keyboard
focus. Both React clients now key controls by coordinate identity and update
committed-bound-tagged drafts without replacing the form. Native/browser tests
exercise keyboard Zoom and Apply with retained focus; browser proof repeats
keyboard Zoom and checks actual SVG x ticks. Mode-specific copy distinguishes
persistent working ranges from temporary report inspection. Fresh final captures
received a scoped `ship` disposition; existing design guidance needs no changes.

The final UI correction batch preserves editable identity drafts across comparison
refreshes, keys forms by pinned snapshot/report, and cancels evidence reads when
the report closure changes. Hash fields occupy a full labeled row on narrow
screens; the column panel stays within viewport insets. The reviewer returned
`ship` for these scoped fixes, with no material findings. This is not completion
of the remaining plan contracts above.

Implementation branch: `feat/native-results-workspace`, based on the
operator-selected `origin/crab-v2`. This plan is uncommitted. Use coherent commits
such as `feat(workflow): capture durable result snapshots` and
`feat(desktop): compare native experiment results`; do not push or open a PR
without instruction. Format touched Rust before each commit with the correct
external target environment.

The branch was fast-forwarded to `origin/crab-v2` at `040916ccd05` on 2026-11-18;
the implementation was preserved through an automatically reapplied stash. The
last full 32-check package proof above precedes this sync and the following
source-input correction; it is retained historical proof, not current completion.

A real two-stage reproduction fixture initially failed because capture marked an
upstream producer's future directory member as a missing source file. Output
ownership is now shared by planning and source capture. The fixture uses an
ignored copied source directory, changes its current bytes and HEAD, then
reproduces the original metrics while regenerating derived data. Unit cases keep
working-directory normalization, directory boundaries, unselected producers and
frozen/conditional/persistent output state explicit. Current focused proof:
20 graph checks, 25 Results owner checks, 36 experiment checks, four local Results
journeys and seven directory-cache checks passed; the copied-directory witness
was rerun after the sync. Two evidence-verifier tests passed. Non-test growth
moves output ownership into one shared boundary used by both real callers.

The acceptance runner now requires this second input-capture case and a named
`produced_inputs_regenerated` capability: 33 checks when all products are enabled.
A fresh combined package run remains required after the remaining execution
contracts are closed. Cache-hit capture and admitted directory/external/produced
input identities still need proof/implementation; root-input reproduction is not
proof of those admission witnesses. Do not alter cache policy merely to produce
a green fixture: ordinary experiments currently use private temporary caches.

Admitted-input capture now uses per-attempt journal facts written atomically
before execution or cache replay. Results keeps directory and produced-input
hashes, omits external/absolute locations, and preserves external content
identity. Input and output witnesses are separate contracts: outputs retain byte
size for the existing promotion guard. Final-attempt summaries discard stale
outputs after failure and expose missing observed duration explicitly.

Current focused proof after the sync: all 747 workflow library tests pass,
including journal migration/rollback/bounds, resume, real cache replay without a
second child invocation, and Results stage-summary checks. Five local CLI
Results journeys pass; the real directory/produced and external-source cases
mutate input bytes during execution while retaining admitted hashes. Schema
drift plus 72 validation cases, 36 experiment and seven directory-cache tests,
minimal/Results SDK slices, both client typechecks, focused lint and 28 shared
preference/session checks pass. Docs links pass (399 pages, 4,308 fragments).

The dedicated runner now requires all three input cases and
`admitted_input_identities`: 34 named capabilities with all products enabled.
Individual capability flags are set only after their corresponding proof passes.
Source binding includes workspace manifests, Cargo.lock and Cargo configuration.
CI builds the browser assets before compiling the HTTP server that embeds them.
Fresh final-source packaged native/browser/HTTP qualification passed: all 34
capabilities and 14 relevant integration tests on Darwin-arm64, based on
`040916ccd05` with source-tree digest
`96bdff98aec882ad1a6c1c3b3de8fcc2d8acc38f4fe900abe4f0cf033018c064`.
The current source, chart runtime and all recorded CLI/package binary digests
were independently rechecked after completion. Native proof verifies external
input visibility after restart/cold retrieval and unavailable timing on an
actual failed run. Actual experiment cache-hit product proof, managed transport,
other OS packages and remaining chart/interaction contracts remain open.

The final visual review found colliding UUIDv7 prefixes on offline attachment
rows. Table and attachment labels now share the full immutable run ID, alongside
optional names. The regression fails against the earlier export and passes the
fresh real-run export, verifying each metric Preview returns its owner's score.
Twenty-seven Results owner checks pass. Rebuilt-package qualification includes
that regression with network denied; the scoped final review returned `ship`.
This is current local proof, not whole-plan or cross-platform completion.

The managed-transport audit reproduced an installed-profile routing defect:
Results' direct repository-store constructor could interpret a custom managed
authority as a bucket before resolving credentials. The shared constructor now
classifies through `ManagedRepositoryResolver` and requires direct storage before
credential selection. Results, named workflow artifact stores, status/cache and
experiment writes share that protection. Bucket-level construction is unchanged;
managed reads already resolved explicitly through their grant owner remain on
that path. This is a namespace correction, not a new managed publication API.

The regression failed before the fix and passes afterward. Actual CLI tests make
a real run/report, exercise report push/pull, run pull and experiment push under
installed, corrupt, reserved and ordinary direct authorities, then verify the
local report is unchanged. The incomplete direct provider is a rejection trap,
so no ambient credentials or network are used for refusal proof. Current focused
checks pass: 30 auth, three workflow, six local Results, 36 experiment and seven
directory-cache tests; SDK `results,service` feature check; two verifier tests;
web typecheck/lint/10 tests and links (399 pages, 4,308 fragments).

The acceptance producer now requires `managed_results_transfer_refusal` and one
nonzero transport test: 35 capabilities and 15 relevant tests with all products.
The previous 34-check package proof predates this correction. Real managed-service Results
qualification remains open: the required managed authority, organization, token
cache, CLI and fixture-control environment entries are absent here. Cache-hit
product proof, other OS packages and remaining chart/interaction contracts also
remain open. Non-test growth is confined to the shared repository authority
check, cancellation ordering, its public contract and explicit acceptance
bindings; stored formats/defaults and execution/cache policy do not change.

The acceptance audit also reproduced five verifier false accepts: different valid
CLI/application digests and mismatched platform/workflow run/attempt metadata.
The verifier now compares all of those fields with an independent binding.
Its standalone CLI requires artifact paths (`--cli`, plus `--packaged-cli`,
`--agent` and `--application` for desktop), platform and workflow run/attempt;
it hashes the supplied files rather than accepting digest-shaped text alone.
The producer records package hashes before testing and rehashes current binaries
at completion, rejects source/HEAD changes, and invalidates previous successful
evidence before fixture setup can fail. Four verifier tests pass, including
changed package bytes and setup failure leaving an explicitly unsuccessful
artifact.

Fresh combined qualification passed against the rebuilt Darwin-arm64 package:
35 capabilities and 15 required test entries, HEAD `040916ccd05`, source-tree
digest `b20588fa64a1c857072c7567e81e7352e26abe29a547ac189d4acc2fe1ead1bc`.
The standalone verifier also passed with independently supplied current source,
runtime, actual CLI/package files, platform and workflow run/attempt. These are
the defined local smoke capabilities, not completion of every plan requirement.

The next completion audit must retain these distinctions:

- Browser layout tests consume a real CLI report but intercept all `/api/`
  requests (`packages/ui/tests/browser/results.e2e.ts:99`). The additional
  `results-server.e2e.ts` journey now drives the embedded application through
  real TCP, OIDC and RustFS using those CLI-captured runs; retain both proofs.
- The original HTTP Results test uses Alice, Bob and an outsider. The new
  browser journey also removes Alice's existing grant, retries the same session,
  and rejects report/evidence IDs in another repository she can administer.
- Results actions now also have a third packaged journey in
  `packages/desktop/tests/e2e/results-actions.spec.ts`: Apply/cancel/deletions,
  pinned Reproduce after HEAD/code/input changes, current-HEAD checkpoint Resume,
  and artifact promotion/CAS conflict with actual retrieved remote bytes.
- Ordinary experiments get fresh private `.crab/cache` directories and disable
  remote cache publication/lookup. Stage hashes include the stage name. Runner
  cache replay and Results summaries are tested separately; do not manufacture
  an experiment cache hit or silently change that policy to close its gate.

The next qualification slice drives the embedded browser through real HTTP,
OIDC and an isolated RustFS namespace using the two CLI-captured runs. It must
publish with model/input bodies unavailable, reload the immutable report, reject
the same IDs in another authorized repository, and revoke the existing session's
repository grant before retrying report/evidence reads. The server publication
helper currently fetches every non-log attachment, unlike the canonical
`Attachment::report_content` policy used by local/remote report storage; the
regression must fail there before that helper is corrected. Shared repository
setup stays in `test_support/http`; the Results suite must drain its server and
Cell runtimes on both success and assertion failure. This closes only the real
browser/publication/authorization gaps, not native action or release gates.

The first combined journey failed before authentication: the embedded asset
owner returns 404 for `/team/private/results` because its browser-route allowlist
omits Results. In-app navigation must reach the publication regression first;
the same journey must then reload and directly open the saved report URL after
the route is registered. Keep unknown asset/API namespaces returning 404.

In-app navigation reached real metrics 0.1/0.2, four curve paths and two retained
images. Saving then returned 404 with the two linked model bodies removed. The
HTTP helper now filters present bodies with `Attachment::report_content`, as
local save and remote report push/pull already do. The asset owner registers
Results alongside its existing application pages, with direct/report-query
reload assertions added to its existing route table. The browser fixture uses
one signed-in identity for both authorized repositories, and removes that
identity's primary-repository grant through the revision-checked members API.
It retains failure screenshots without traces and drains HTTP/Cell runtimes
after assertions. Qualification must rerun against these corrections.

Strict server-library Clippy found seven Results-owner issues. Nested guards now
use let chains, the redundant iterator `must_use` attribute is removed, and the
large remote reader variant is boxed at its three SDK/HTTP construction sites.
The new reader is unshipped and not serialized; wire formats and storage identity
are unchanged. Strict server-library Clippy, 20 focused owner tests and the
minimal opt-in SDK Results test pass. Rebuild the native package and repeat the
source/package-bound qualification after this final owner cleanup.

The two installed Playwright versions differ (desktop 1.65.0, repository UI
1.62.1). The CI Results setup now installs Chromium through each package's own
CLI; installing only desktop Chromium does not provision the UI's pinned browser
revision. Local tests use installed Chrome, so that local pass cannot qualify the
CI browser-install contract or another operating system.

Fresh combined qualification passes on the rebuilt Darwin-arm64 package: 39
named capabilities and 16 required test entries, plus the separately recorded
scale case. The source-tree digest is
`241fc4710ee886198bf2a00e152d3e4b3381912b6afb51ff773f52c28102dd7d`
at HEAD `040916ccd05`. The standalone verifier also passes using the source
binding captured before qualification and the actual CLI, packaged CLI, sidecar
and application files. Packaged tests confirm Data Workflows placement without a
Results activity-bar entry, restart, cold retrieval and offline rendering. The
real browser case confirms actual values/curves, publication with linked model
bodies absent, persisted/direct report reload, repository isolation and revoked
access while the identity remains signed in. This closes the combined browser
and current-grant gaps; native UI action journeys, cache-hit policy proof,
managed-service qualification, other OS packages and remaining chart/performance
contracts still keep this plan IN PROGRESS.

Non-test changes in this slice use the existing attachment-retention predicate
and asset route owner, with no new storage format, config, dependency, fallback
or parser. The shared reader's indirection reduces its enum size; the test-only
second-repository setup lives at the prescribed shared fixture boundary. New
test surface supplies the previously missing combined product proof.

- [ ] Every accepted capability is implemented and has retained proof.
- [ ] Focused core/CLI/SDK/sidecar/client/server commands run nonzero tests and
      pass; generated contracts and exact feature/minimal slices are verified.
- [ ] Existing experiment/plot/apply/checkpoint/cache policy contracts hold.
- [ ] Native Results is neither queue-only nor external-tracker-dependent.
- [ ] Historical reads never substitute workspace/latest-ref bytes.
- [ ] Report values/curves survive metadata changes, source removal, index
      rebuild, restart, cold retrieval and offline export.
- [ ] No incomplete publication root is acknowledged; policy exclusions and
      capture failures remain visible; active maintenance lifetime is proven.
- [ ] All ten journey steps pass with source/package-bound evidence, including
      failure, permission, session and cancellation paths.
- [ ] Related format/lint/type/build/tests/docs pass. Broad/release evidence is
      retained from CI/dedicated environments, not inferred from source review.
- [ ] `git diff --check` passes; review `git diff --numstat`. Non-test growth
      earns shared ownership for actual consumers; duplicated old plot/table
      paths are deleted in switching slices.
- [ ] Files remain in scope; dependency/lockfile changes are reviewed;
      `plans/README.md` records status accurately.

The next native action journey uses normal `crab init` and the packaged app.
Queue two runs that acknowledge checkpoints; confirm Apply restores exact model
and parameter bytes and deletes recorded paths, while Cancel preserves edits.
Publish immutable artifact versions from those outputs, exercise promotion and
CAS conflict through the native prompts, and inspect remote stage-selected
bytes. Move HEAD and alter code/inputs before Reproduce; inspect its new ID,
captured values, source commit and source-result lineage. Resume must show the
actual current HEAD/checkpoint and produce a new descendant from restored model
state using today's configuration. Do not remove generated local settings just
to enable Reproduce: capture currently rejects every local-settings file, even
the default one `init` creates. Reproduce that default-path behavior before
deciding which private execution policy must remain a refusal.

The native action regression reached Apply and promotion/CAS effects, then
confirmed Reproduce was disabled by init's comment-only TOML. Capture now permits
only a semantically empty local-settings table, without retaining its bytes;
nonempty private settings retain the prior explicit refusal. A real CLI
regression covers both cases before the packaged journey is repeated.

The existing reproduction specification has only a source commit, so it cannot
name which immutable result produced a new run. Add optional `source_result`
(`RunSelection`) to the unshipped workflow-owned `ReproductionSpec`. Original
runs store null; each reproduction names its immediate source ID and digest,
validated as a distinct canonical experiment and content identity. Capture must
carry this relation through execution, never infer it from current names/refs.
Regenerate the actual JSON Schema/TS owners and display the relation with captured
run identities in both clients. Reports freeze it as a link; it adds no recursive
payload retrieval or source-root retention requirement. Existing Results formats
are unshipped, so no compatibility reader or alternate codec is introduced.

Fresh combined qualification passes after these changes: 46 capabilities and 18
required test entries on the rebuilt Darwin-arm64 package. Source-tree digest:
`8a19e6be9d6c9d09e695b3aceccae960f5d5862bffa87cb8d6fe0cdf108d8bf2`,
HEAD `040916ccd05`. The independent verifier passes with the pre-run source
binding and actual standalone/packaged CLI, sidecar and application files.
The four native flows are exercised through UI controls; assertions inspect
restored/deleted files, new result IDs, pinned source bytes/commit, inherited
checkpoint state/current configuration, remote artifact bytes and CAS refusal.
The source ID/digest remains frozen in a report after source rename. The real
browser reads an additional CLI-produced provenance report without a run root
for its reproduced child, displays its exact ancestry at 1280/390 widths, and
retains the existing repository isolation/revocation proof.

The two response enums box their largest Run payload after the ancestry field
increased its size. This changes Rust allocation only, not JSON/TS wire shape;
owner contract checks and regenerated CLI schema pass. Focused owner proof is
28 tests; strict server-library Clippy, minimal Results SDK test, client builds
and types, targeted React/TypeScript rules, four verifier tests, web build/types,
10 web tests and documentation links pass. Web lint has zero errors and 16
existing warnings. The desktop-wide lint script cannot start because this
checkout and `origin/crab-v2` have no ESLint configuration; the focused lint uses
the already-established external Results rule set, not a new repository config.
Fumadocs regeneration adds only the Results document import; import renumbering
in generated `.source` files is not hand-edited.

Independent Impeccable finish review returned scoped `ship` for the ancestry
display/status clearing and all three fresh captures. Documentation review
confirmed no new visual rule or design-file change is needed. This does not
close the whole plan. Still inspect checkpoint-derived replay prerequisites:
the native Resume test proves state restoration, but its original/current
tracked models share an iteration value, so it does not prove Reproduce of a
resumed run restores the initial checkpoint rather than that base commit's
model. A next regression should give those states different counters, reproduce
the resumed run, and compare actual output bytes. Its typed input capture or
explicit unavailability must be proven before closing exact replay. Cache-hit
policy proof, remaining chart/interaction budgets, managed-service qualification,
other OS packages and broad release gates also remain open.


The materialized-input regression fails on the first replay of a resumed run:
the checkpoint starts at 1, the new base commit starts at 100, and replay
produces 101 instead of 2. The experiment runner must pass the actual paths
materialized by copied-path overlays, checkpoint application and prior replay
into the existing input capture owner. Retain their pre-command bytes in
`ReproductionSpec.files`; reuse that closure across reproduction generations.
Keep planner output classification unchanged: only explicitly materialized
outputs become initial inputs. Capture must deduplicate paths before charging
the file/byte budgets. No serialized field, storage key or compatibility reader
is needed. The regression covers both checkpoint and copied output seeds across
two generations; existing producer-regeneration and bounded-capture tests guard
the sibling paths. Qualifier evidence must require this nonzero regression.

Implemented that materialized-input path through the experiment runner and
shared capture owner. Copied paths return their normalized materialization
identities; checkpoint application returns the paths it actually restored;
reproduction descendants carry their retained input paths forward. Capture reads
after all overlays, merges with planner source inputs and reuses existing
attachments before charging budgets. A source directory above half the 64 MiB
budget proves overlapping declared/replay paths do not consume the budget twice.
The previously failing checkpoint replay now produces 2 rather than 101, and
the copied-output case retains its 100 seed and produces 101 across generations.
No serialized shape, storage layout, dependency or execution/cache policy changes.

The rebuilt Darwin-arm64 package passes 47 capability checks and 19 required
test entries, plus the dedicated 10,000-run catalog check. Source-tree digest:
`a595985e4297d030ae740c301be5e29a251ee8caf7ac214def6d464053a5bb5c`,
HEAD `040916ccd05`. Independent verification passes against the pre-run source
binding and actual standalone/packaged binaries. All three packaged UI journeys,
real browser/OIDC/storage tests, source browser tests and transfer/capture faults
pass. Native navigation still asserts no standalone Results button before
opening Data Workflows → Results. Focused CLI tests pass 8/8 with the owned-S3
test separately qualified; shared Results tests pass 28 with two explicit gates
run separately where applicable; four verifier tests and documentation links
pass. Formatting and `git diff --check` pass. The small production increase
preserves one initial-input closure shared by actual callers rather than adding
another restoration path.

This proves file seeds, not every directory replacement shape. Before closing
tree replay, exercise copied/checkpoint directories that delete tracked children
or introduce empty directories, then replay their descendants. Also retain the
remaining cache-policy, chart/interaction, managed-service, other-platform and
broad release qualification gates above. This backend slice changes no UI
design; the prior scoped finish review remains applicable.

Directory replay reproduces the suspected gap: a resumed source observes
`empty/` and `keep.txt`, while its replay observes `keep.txt` and the base
commit's `stale.txt`. The current file-only closure cannot represent absence or
empty directories. Add required `directories: Vec<String>` to the unshipped
workflow-owned `ReproductionSpec`; no alternate reader/default is needed.
Capture every traversed input directory before commands, within a 1,024-directory
bound, sort/deduplicate its identities and carry them across replay generations.
The worktree owner must clear only captured root directories in its owned
temporary checkout, recreate captured empty descendants, then write verified
files. Preserve existing base-checkout file modes before directory replacement.
Validate directory paths, sorted uniqueness, bounds and present-file ancestry at
the shared contract boundary and before restoration. `std::fs::remove_dir_all`
documents no-follow link removal; retain the existing parent-link checks
(https://doc.rust-lang.org/std/fs/fn.remove_dir_all.html).

Consumers: CLI capture/runner and worktree restoration write/read this shape;
local/remote snapshot and report owners already hash/verify the complete spec;
SDK, sidecar, HTTP and clients exchange the same generated schema/TS shape.
Regenerate through existing owner tools and verify those consumers. Extend the
real CLI regression across copied and checkpoint directory seeds and two replay
generations; retain the file-seed, producer-regeneration, bounded-capture and
immutable-report tests. Navigation/UI design remain unchanged.

Combined directory-contract acceptance reached both native action/offline tests,
then the later queue batch lost its Start button. Fifteen normal isolated
repetitions passed. A controlled real-IPC ordering probe reproduces the exact
failure: enqueue returns one new ID; a fresh list contains three rows with the
new run pending; an earlier two-row reply then overwrites that UI state. The
backend is correct. `ExperimentPanel` applies every reply without ownership.
Give each fetch a monotonically increasing request identity; only the latest
request may settle rows/loading, and effect cleanup invalidates prior requests.
Preserve the controlled ordering in the packaged persistence journey, asserting
that the fresh three-row reply arrives before the older two-row reply while
Start and the actual failed result remain reachable. The test bridge is frozen;
temporary reply ordering uses the pinned Electron main-handler registration
whose source was inspected at
https://github.com/electron/electron/blob/v45.2.3/lib/browser/ipc-main-impl.ts.
Keep all backend calls real. Delete temporary trace probes before qualification.
This is an asynchronous state fix, with no design/navigation change. A separate
intermittent chart-focus observation still needs its own proof; do not attribute
it to this queue race without evidence.

A metadata-only native reopen event with the same repository/session reproduces
chart focus loss before any axis update: the document remains focused but its
active element becomes body. Results effects depend on the whole display repo
object and clear the comparison whenever capabilities/metadata replace it. The
browser sibling also depends on a whole object, supplied as a fresh inline
`{...repo, csrf}` by the repository page. Use memoized transport read scopes:
native repository ID/session ID; browser account/repository identity and owner/name
address. Narrow Results API arguments (and the shared URL builder's type-only
argument) to fields actually read. Keep permissions, CSRF and display metadata
live separately. Browser lifecycle keys must match the existing issuer/subject
preference namespace. Retain metadata-refresh focus assertions in both clients;
real scope changes still cancel and revalidate reads. No visual design, storage
format or browser authorization policy changes are intended.

The source/binary-bound package passes 51 local capability checks (20 required
test entries plus the dedicated scale check), tree
`d67e773e4bf6031ab65c57fb6041335254903894cad220e18088921e0c1d6e22`.
Independent verification passes; that evidence is archived separately before
the next renderer correction. Fresh Impeccable review returns scoped `fix`,
not whole-plan approval: dark browser plot guides retain the initial light
foreground. Stable read scopes exposed presentation's dependence on incidental
comparison re-fetches. The shared renderer samples `style.color` once.

Corrected weak default axis/legend/title paint in the shared SVG renderer to use
inherited `currentColor`, which Vega's inspected SVG renderer writes directly
as paint attributes. Authored per-guide config overrides those weak defaults;
explicit authored paint still wins. Theme updates do not re-query experiment data
or remount controls. Painted SVG contrast assertions cover actual
browser light→dark→light transitions and an offline-body foreground transition
with focus/range preservation. The bundled offline asset was rebuilt through its
existing producer; the native sibling and the same four review views were
recaptured. The same reviewer scored material fix #1 resolved, disposition
`ship` at that fix's scope. Minimum SVG text contrast across light→dark→light→dark
was 17.93, 16.91, 17.93 and 16.91. This is not whole-surface or Plan 020 approval.

Final local qualification for this correction passes all 53 capability flags and
20 required test cases plus the dedicated 10,000-run scale check, on the packaged
Darwin-arm64 application with owned RustFS storage. Pre-run source-tree binding:
`160ff56005b4dd8b0dbfc9f2d2976cee423f3cbcc004fa00bcf3811b63464ec6`.
Generated `results-runtime.js` SHA256:
`22bb4fcd627a8147739313849fbfb97470db1511c2f623b1f53c22cf4add3736`.
Independent verification matches this binding and the actual CLI, packaged CLI,
agent and application digests. Evidence includes forced old-after-new real queue
replies, metadata-only refresh with keyboard focus retained, and offline
foreground transitions with network denied. Remaining maximum-chart/interaction,
managed-service, other-OS and broad release gates remain open.

The finishing documenter checked the same four captures, token/style owners,
controls, navigation, stable read scopes, renderer and regression evidence.
Existing design documentation already covers this extension; no design-system
files changed. Pre-existing design-document format drift, missing sidecar and
absent PRODUCT.md were reported and preserved. The scoped review/documentation
gate is complete; the full Plan 020 gate remains open.

### Chart fidelity and maximum-overlay qualification

The next gate reproduced three owner failures before correction: CSV/JSON/YAML
absence made numeric y axes nominal; decimation removed missing intervals; and
LOESS/aggregate plots silently received sampled rows. The shared parser now
retains nonserialized absence and original-ordinal witnesses. Tagged
`v2.0.0-alpha.1` CLI JSON string rows remain unchanged. Both retained Results and
workspace/Git-revision plots use the same normalization; both cross-file x
callers move absence with the coordinate through `PlotRow::replace_x`.

Sampling retains endpoints, extrema and both sides of every absence transition.
Unrepresentable gaps and oversized statistics/transform inputs refuse visibly
with source bytes retained. Selected runs with different captured template
bytes under one path cannot silently use the final run's template. The owner
regressions cover these boundaries, literal category strings and 2 MiB/series
refusal. Bounded custom transformed/compound views preserve their authored
semantics without acquiring unproven shared-axis controls.

The initial real browser probe rendered all 20×2,000 SVG points in 2,303ms and
zoomed in 790ms. This exceeded the existing 500ms/100ms budgets. New normalized
overlays share 2,000 display points across all series; this does not relax either
time budget or discard original evidence. The retained maximum fixture contains
40,000 original points, 1,940 displayed points and a 79,602-byte chart response.
Measured initialization/zoom/cursor: 164ms/43ms/7ms. The offline report now shows
sampling counts, matching both native clients. A 390px regression also identified
unbroken report hashes overflowing to 553px; report identity and manifest text
now wrap without changing authored chart layouts or internal table scrolling.

Focused owner proof passes 39 tests including catalog scale and the renderer
fixture. Actual offline rendering proves two curve segments around a missing
interval, calculated values 10/20/30/40/50 and a compound mean of 3. Current
maximum-renderer and narrow-view changes require the refreshed bound package,
all 58 capability checks/32 required cases, fresh scoped visual review and
documentation check before this correction's acceptance gate closes. The
earlier 53-check evidence is retained independently and is not evidence for
these later source changes.

Two-run template qualification then exposed merged LOESS/minimal curves and
combined confusion cells: one curve and four aggregate cells appeared where two
curves and two independent four-cell matrices were required. Built-in curve
transforms now group by revision. Confusion templates retain revision in their
aggregate/impute groups, facet by series, and normalize within each y class/run.
Zero-observation combinations of the observed classes receive explicit zero
cells. Both CLI and retained Results share these corrected template semantics.
Primary contract: [DVC template source](https://github.com/treeverse/dvc-render/blob/main/src/dvc_render/vega_templates.py)
and [DVC plot identity/anchors](https://doc.dvc.org/command-reference/plots/templates).
The browser checks actual separated paths, cell counts and painted normalized
values; semantic HTML exports also receive named keyboard-scrollable plot regions.
Matrix imputation receives a separate pre-render cell budget: sparse class
observations can expand far beyond their row count. Border cases on either side
of 2,000 cells pass/refuse before Vega allocates the grid. The visual correction
uses equal matrix width/height and a product-owned scrolling viewport around the
Vega-owned ARIA host; headings/export actions remain outside horizontal panning.

At that point, multi-y built-in overlays, current DVC object/array-valued
anchors and horizontal-bar axes still needed exact downstream tests; the
following correction supplies that bounded proof. Large transformed inputs still refuse when they
cannot fit the current lossless/statistical boundary; that limitation remains
part of the unfinished chart gate. These checks do not establish universal
template parity or close the whole plan.

The corrected Darwin-arm64 package now passes all 58 source-bound capability
checks, 32 required test cases and the dedicated 10,000-run scale check. Source
tree: `a0cc48e5379685309e32d965d6a4e525f839d30d3dbd4619701e7b9d99c146af`.
Generated runtime SHA256:
`46e7b382a3690d9deef5f072b1d364de1a92b859d7144b9ead8b4d5b85fb6733`.
Independent verification matches pre-run binding and the actual debug CLI,
packaged CLI, agent and app archive. Default owner checks pass 38 with three
intentional ignored producers; the dedicated gate separately executes all ten
chart cases and the catalog scale producer. CLI plot regressions pass 39/39;
both client type checks/browser build, runtime producer, verifier and docs checks
pass. Web lint retains its 16 existing warnings with no errors. The fresh chart
review returns scoped `ship` over all six current captures, with no material
fixes. The documenter independently checked the captures, current source tree,
runtime hash, external 58/32 evidence, pre-run binding and verifier success;
the existing design system was preserved. Recorded maximum-overlay timings are
172.1ms initialization, 45.2ms zoom and 8ms cursor update. This scoped review and
documentation gate is complete; broader parity/transport/platform gates remain
open.

### DVC template parity follow-up

The next shared-owner correction adds nine focused regressions: all selected y
fields across runs; current typed DVC anchors; source files sharing a plot ID;
literal field names; metadata/source-name collisions; horizontal-bar axes;
multi-y matrix expansion bounds; unknown anchors versus literal captured text;
and numeric linked domains for backslash columns. Four original regressions
failed before the fix. Template expansion now retains wide source columns for
authored expressions while assigning each selected metric a separate series.

Primary dependency proof includes DVC's `vega.py` and `vega_templates.py`, and
local Vega-Lite 5 `compile/selection/{assemble,point}.ts` and `util.ts`. Its
flattening drops literal backslashes in encoded fields and rewrites resolved
selection keys. Shared normalization keeps the source columns and adds encoding
aliases; the bundled runtime owns a scalar cursor signal consumed by native,
browser and offline reports. The stale CLI PlotRow import is removed. Custom
axis labels and source-file/header declarations must match before overlaying.

The offline fixture renders six views, including four separate curves for two
runs × two selected metrics, typed current anchors, grouped horizontal bars, and
literal-column hover. It rejects all external requests and checks both 1280px
and 390px layouts. This is synthetic template/dependency proof, supplementary to
the real queued-run acceptance journey. The corrected Darwin-arm64 package now
passes all 60 source-bound capability checks, 42 required test cases and the
separate 10,000-run catalog check. Independent verification matches the pre-run
source/runtime binding, actual debug CLI and unchanged packaged CLI, agent and
application archive. Source tree:
`dfca88bad6e4fb0fec1cea38b80dfb58e80aed13ce6ac833cc8befa107bd2a1d`.
Generated runtime SHA256:
`af20a84eb867bbf7357425e37a583672c669568f0435c20f315e111d513fc54e`.

The three packaged journeys explicitly open Data Workflows → Results, assert no
standalone Results navigation button, execute actual runs/actions, and recover
the comparison after restart and cold retrieval. Five browser cases and the
real HTTP/browser publication journey pass. Focused owner defaults pass 47 with
three intentional ignored producers; the dedicated gate executes all ten chart
and nine template cases. CLI plot checks pass 39; both client type checks,
explicit browser-test type probe, UI build, runtime producer, verifier and docs
link checks pass. The shared template owner grows to implement typed anchors,
long-form series and dependency-safe field/cursor normalization; it replaces the
single-y/string-only path, rather than introducing another client renderer.

Fresh visual review returns scoped `ship`, with no material fixes over all eight
wide/narrow captures. Documentation verifies the same captures, source and
runtime, preserving the desktop design system. Reviewed maximum-overlay timing:
171ms initialization / 42.6ms zoom / 7.5ms cursor. The preceding 58/32 evidence
remains historical. Large transformed inputs, managed-service and full
platform/release gates remain open; this correction does not establish universal
DVC template parity or complete Plan 020.

### Exact counted matrices follow-up

Owned `confusion` and `confusion_normalized` templates now count all retained
observations before rendering. The shared accumulator feeds both CLI Vega
exports and retained Results, replacing raw matrix rows with exact weighted
pairs. Results bounds the expanded class grid at 2,000 cells; input row count
does not force statistical sampling. A two-run/two-field fixture counts 400,000
observations into sixteen pairs per chart. Authored overrides keep their own
semantics and the existing explicit refusal for unsafe oversized transforms.

Primary Vega source shows aggregate keys string-coerce values and join fields
with `|`; numeric class IDs prevent null/text and delimiter collisions. Typed
normalized identities preserve numerical equality/order and distinct missing
labels. Vega-Lite's header/axis compiler rewrites label expressions textually,
so literal labels live in parameter values. Dimension tooltip titles remain
distinct from count/normalized statistics. The generated `ChartReduction`
contract distinguishes counted aggregation from sampled curves in all three
clients and frozen report validation. Tagged CLI JSON rows remain unchanged;
an actual external-process test checks all 10,000 rows alongside counted Vega
export. The new owner pays for its size by sharing exact counts, class identity
and renderer-safe labels across these existing callers.

Fresh Darwin-arm64/RustFS qualification passes all 64 capability checks and 49
required cases, plus the separate 10,000-run catalog check. Source tree:
`fda29718151d5f3df6ce068fafe51f47db662fcc701aaccb7c7d905bbe941493`.
Runtime remains
`af20a84eb867bbf7357425e37a583672c669568f0435c20f315e111d513fc54e`.
Independent verification matches the pre-run source/runtime binding, actual debug
CLI and unchanged pre-recorded packaged CLI, sidecar and application digests.
Three packaged journeys pass, including the eight-observation matrix before and
after restart/cold retrieval. They open Data Workflows → Results and reject a
standalone primary navigation entry. Six browser cases and the real HTTP/browser
journey pass; owner defaults pass 52 with three intentional ignored producers.
Both client type checks, UI build, minimal/results SDK checks, schema/declaration
generation and web build/type/lint/tests/links pass. Web lint retains its sixteen
existing warnings and no errors. Fresh maximum-overlay initialization/zoom/cursor
measurements: 164.5ms / 42.1ms / 7.6ms.

The fresh visual review returns scoped `ship` with no material fixes across all
fourteen wide/narrow offline captures. Documentation preserves the incumbent
design system and distinguishes this source from historical 60/42 evidence.
This closes the counted-matrix correction's local acceptance, not Plan 020.
Unsafe large authored transforms, the full visible failure matrix, managed
service qualification, remaining native OS packages and CI/release evidence
remain open.

### Partial primary curves and visible failure recovery

The shared comparison reader now retains verified plot bodies or their original
typed errors. A missing or corrupt primary plot body leaves captured scalar
rows and healthy selected runs' curves visible. Missing, denied and corrupt
availability remain distinct; an entirely unavailable chart carries no spec.
Provider diagnostics stay out of renderer data. Cancellation and typed storage
read-admission rejection still terminate the operation. The real filesystem SDK
regression first caught an exhausted request budget being swallowed as chart
availability; the corrected reader returns `LimitExceeded`.

Real packaged fault checks corrupt/remove captured primary curve bytes, restore
them, close the native session and reopen a new session. Frozen report values
and healthy curves survive evidence-preview failures. Working comparisons retain
three healthy paths when one of four selected primary curves is unavailable.
Long integrity messages previously pushed Retry entirely outside the 900px pane;
both clients now wrap the text while preserving the button's width. The fresh
390px browser capture uses a controlled API error over a real CLI-produced
report; real HTTP/storage/browser proof runs separately.

Fresh Darwin-arm64/RustFS canonical qualification passes all 72 capabilities and
52 required cases, plus the separate 10,000-run catalog check. Source tree:
`d8a978d7af1cd57349e6b38cffd0334b34ebf05e54b732a66cbd3c508dc7fa4e`.
Runtime remains
`af20a84eb867bbf7357425e37a583672c669568f0435c20f315e111d513fc54e`.
Independent verification matches current source/runtime, the actual debug CLI
and the pre-recorded packaged CLI, native agent and application hashes. All
three packaged journeys, six browser cases and the real HTTP/browser journey
pass. Results remains inside Data Workflows; packaged checks reject a standalone
primary navigation entry. Fresh scoped visual review returns **ship**, with no
material fixes across eight wide/narrow captures. Native captures come from a
timed diagnostic repetition using the real RPC handler and original assertions;
their manifest preserves this provenance rather than relabeling them as the
canonical run's captures.

An earlier canonical attempt timed out at the five-second cold-retrieval
assertion while Retrieve was still busy. Its evidence is retained separately.
Three original-deadline diagnostic repetitions passed at 311/540/701ms, followed
by the successful unchanged-source canonical rerun. No source or assertion was
changed to suppress the failure; its cause remains unproven. This is an open
qualification flake, not a new remote-transfer timing guarantee.

These named cases qualify primary plot bodies. The scoped documentation gate
is complete and preserves the eight reviewed capture hashes. The following
correction addresses cross-file coordinates and bare-remote UI proof; it does
not retroactively extend the 72-check evidence. Authored-template/transform
failure variants, remaining visible/in-flight failures, managed-service access,
other native OS packages and CI/release qualification remain open. This
correction does not complete Plan 020.

### Cross-file coordinates and stale chart paint follow-up

A new local/remote owner regression first returned two metric rows but no
healthy curve when one run's coordinate body was corrupt. Input verification
now completes per revision before any observations enter the overlay. Primary,
coordinate and captured-template read failures share classified evidence
handling; incompatible declarations or verified template meanings still refuse
the affected chart. The named partial-preservation regression covers primary
and coordinate bytes, including the healthy coordinate domain. Separate tests
cover classified whole-chart template unavailability without provider details;
partial authored-template/transform guarantees remain unqualified.

The packaged regression also exposed old SVG paint under an unavailable-chart
notice: it counted four paths where only three valid paths should remain.
Vega View's `finalize` contract removes timers/listeners/tooltips, not rendered
SVG. Both React clients now clear the visible container for every specification
change and give each async renderer its own detachable host. An absent spec
cannot retain old paint, and a late renderer cannot restore an obsolete host.
Offline reports construct immutable chart hosts once and have no equivalent
working-comparison update path. Browser network-fixture transitions already
passed before the correction because their intermediate loading render unmounts
the chart; this is sibling transition proof, not a browser failure reproduction.

The real native fixture now produces per-run cross-file coordinate bytes, checks
single-run corruption/removal, all-coordinate unavailability and byte-restoration
recovery. It also opens the published report directly through a bare remote
session in Data Workflows and verifies publishing is disabled. The bare-session
probe passed against the prior qualified package before becoming a retained
acceptance assertion. No standalone Results navigation entry is added.

Focused owner checks pass 54 with three intentional producers ignored; the
filesystem SDK budget/cancellation regression, both client type checks, strict
native-test type probe, browser transition cases and verifier tests pass.
Frozen candidate source:
`a270d17a523bd723d3707340b902d6df1cdf326e36eeb8a67dad2193b556d062`.
Runtime remains
`af20a84eb867bbf7357425e37a583672c669568f0435c20f315e111d513fc54e`.
The rebuilt Darwin-arm64 package passes canonical qualification of all 75
capabilities and 52 required cases, plus the separate 10,000-run catalog check.
Independent verification matches the frozen source/runtime and actual debug
CLI, packaged CLI, sidecar and application hashes, including the pre-run
packaged hashes. All three native journeys, six browser cases and the real
HTTP/storage/browser journey pass. Maximum-overlay initialization/zoom/cursor:
167.6ms / 44.5ms / 7.5ms. The preceding successful 72/52 evidence remains retained
for its own source; the earlier unexplained retrieval timeout remains recorded.

Fresh scoped visual review returns **ship**, with no material fixes across six
captures. Initial native captures had a transient completion toast over the
notices/legend; replacement captures only add user dismissal in an external
copy of the canonical test, retaining every assertion and the same package.
Both unavailable notices and the 900px legend are now legible. The canonical run
supplies the fresh 390px browser capture. The manifest retains these distinct
provenances. A pre-existing empty chart tab stop is a source-only nonmaterial
focus boundary, not a newly approved design change. Fresh documentation review
independently reconciles the source/runtime, four binaries, 75/52 evidence and
all six captures, preserving the incumbent design system. This scoped gate is
complete. Whole Plan 020, partial authored-template/transform
failure variants, actual outage/in-flight failure proof, managed-service access,
other native OS packages and CI/release qualification remain open.

### Catalog completeness audit: saved and pre-capture experiments

The post-qualification audit found a separate accepted-scope gap. A real local
`crab exp save --name manual-saved --json` retains `metrics.json` with score
`0.5`, and `exp show` returns that value, but `crab results list --json` returns
zero rows. The owned fixture proof is retained as `results-saved-gap.json` for
the same qualified `a270d17…` source. The 75-check matrix does not cover this
path and must not be used to claim catalog completeness.

Source boundary: `crab/src/cmd/exp.rs:2630` writes saved experiment v1 metadata
and its workspace snapshot without installing a Results snapshot. Both CLI
Results and the native reader enumerate only canonical Results roots through
`crates/crab-workflow/src/results/local.rs:112`. `v2.0.0-alpha.1` already ships
the saved-experiment metadata/metrics contract. The current doctor has no
experiment-to-Results migration, and the exhaustive Results-owner/caller search
found no other projection/import path.

Follow-up must cover fresh manual saves and explicit migration of supported
pre-capture metadata. Reuse the experiment owner's verified metadata decode;
do not add a runtime legacy reader or substitute today's workspace for history.
Keep observed saved metrics distinct from unobserved execution inputs,
resolved parameters, stages and reproduction prerequisites. Preserve the
shipped saved-workspace Apply contract, with exact snapshot guards; never call
a saved workspace a successful observed execution. Choose the migration entry
point after checking existing doctor/reindex ownership, then add real CLI and
visible client proof before claiming this gate complete. Explicit migration remains
open. Fresh manual-save capture now uses the owned saved-workspace copy for both
metadata and Results observations. It does not read today's worktree on reopen or
borrow old execution journals. Refreshed packaged/native proof remains pending.

Two retained real-CLI regressions now live in
`crab/tests/workflow_results/saved.rs`. The saved-observation case requires real
saved metrics, parameter values and curve bytes to survive workspace edits,
without borrowing prior logs, stage execution or reproduction prerequisites.
The fresh-save regression now passes: status is `saved`, metric/parameter values
and two actual curve points survive edits, Apply restores the model, and altered
saved parameter bytes are refused. Validation forbids execution evidence on saved
observations. Shared action policy permits saved-workspace Apply/branch while
keeping execution-dependent actions unavailable. This is focused proof, not the
historical 75/52 package qualification.

Source follow-up: the private workspace copier intentionally excludes `.crab`,
while custom template resolution accepts `.crab/plots/<name>.json`. Fresh saved
observations currently resolve templates inside the owned copy, so this declared
visualization dependency needs a real save/reopen regression and an explicit
retention boundary. Do not copy private `.crab` state into publishable evidence or
claim complete saved-template coverage from the simple-curve regression.

### Apply artifact-integrity audit

The saved-workspace contract audit exposed another action gap on the qualified
source. After a real successful run, modifying its retained experiment workspace
model still lets `exp apply --result-digest <selected-digest>` succeed and write
the altered model. `results-apply-integrity-gap.json` retains the owned-fixture
reproduction. `validate_result_source` checks the selected metadata hash but
does not verify the workspace payload against captured artifact witnesses.

The retained regression uses a same-size model alteration, current source/model
edits and a captured deletion target. Before the fix it succeeded incorrectly,
overwriting the model/source and deleting the current file. The expected behavior
is an integrity error with every current file preserved. Both new tests fail
for the intended user-visible reasons in `results-saved-before.log`; this is
before-fix evidence, not passing proof. The 75/52 qualification belongs to the
preceding frozen source and does not cover either new regression.

The fix must prepare and verify the exact bytes used by workspace actions
before destructive writes; checking a live source then copying it later is not
enough. Read the complete Apply/preview/branch paths and checkpoint sibling
contracts before changing the shared guard. Reuse stage output and retained
post-run evidence witnesses, including directory/hash semantics owned by the
workflow hasher. Keep existing checkpoint restoration independent of terminal
workspace availability. Do not add public whole-workspace fingerprints that
could encode private files merely to simplify verification. Preserve the tagged
ordinary experiment Apply contract and make the new selected-result guarantee
explicit.

The production fix now prepares one owned workspace copy, verifies its captured
final observations and stage output witnesses, then consumes that exact copy.
Preview, Apply and branch creation share it. Destination preflight rejects
directory symlinks before writes/deletions. Files stream through atomic sidecars;
copied directory modes match the executor's directory hash contract. Focused
real-CLI tests pass for all three corruption refusals, external-target preservation
and ordinary directory restoration. Existing Apply (including deletions), saved
workspace and branch integration tests also pass. The checkpoint sibling/control
regression passes: ordinary restore succeeds and a symlink target preserves outside
bytes. Nine materialization tests include interrupted streaming preservation. Both
client typechecks, 28 preference/session tests, four verifier tests and docs links
pass. Refreshed packaged/native qualification remains pending at this update.

### Saved-workspace scoped native proof and finishing review

Frozen source `95eca44…` passed the real CLI storage journey and all three
packaged native journeys. The native queue/compare/restart/cold-retrieval acceptance
and offline HTML with network loading denied are fresh. Four saved/refusal captures
at 1280/900 pixels were opened and checked. This scoped proof does not replace
full qualification. Three preceding harness failures are retained: UUID/name
selector assumptions, an ambiguous table/comparison row selector, and the standalone
runner omitting the CLI-generated offline HTML prerequisite. Assertions were kept;
selectors were scoped to their actual controls and the real fixture producer ran.

Fresh finishing review requests two material corrections: raw CLI tracing JSON must
leave the primary refusal, and Retry must describe its actual read-refresh behavior.
The CLI already publishes a typed stdout error envelope. The generic sidecar JSON
runner now preserves that semantic error through existing Command/RPC fields,
sharing error validation with the cache boundary while retaining cache's deliberate
partial-inspection policy. Startup/signal failures still carry process diagnostics.
Both new owner tests and the cache partial-report sibling test pass. Results now
separates concise preview refusal/preservation/recovery copy from expandable
diagnostics; Refresh view promises only read refresh. Updated native proof will
exercise keyboard diagnostics, refresh without repair, repeated refusal and actual
recovery. Rebuild, recapture, reviewer verdict and documentation reconciliation
are pending at this update. The full verifier contract now requires 84 capabilities
and 61 cases; it has not yet been fully rerun on this extension.

### Saved capture and workspace guard qualification — current bound source

Full qualification now passes **84 capability checks / 61 required cases**, plus
the separate 10,000-run catalog test, on source
`4346b1abd02f5bca1013cbdf80d78d56192d0defe1351cb2cb913e99be083229`.
Shared runtime remains `af20a84e…`. The independent verifier matches current
HEAD/source/runtime and the actual debug CLI, packaged CLI, sidecar and app archive
to the binding recorded before the full run. Safe retained records are
`results-saved-84-evidence.json`, `results-saved-84-proof-summary.json`,
`results-saved-full-source-binding.json` and
`results-saved-84-independent-verifier.log`. Archived evidence is byte-identical
to the canonical producer's final evidence.

Both finishing findings are scored resolved: primary guarded-preview refusal is
concise and preserves semantic diagnostics; Refresh view accurately describes
read refresh. The reviewed current native captures show saved status/value/action
eligibility and actual integrity refusal at 1280/900 pixels. Assertions also check
refresh without repair, repeated refusal while corruption remains, then successful
Apply after restoring original captured model bytes. The finishing `ship` verdict
covers those two fixes only. Documentation independently checks the evidence and
preserves the incumbent system. Results remains under Data Workflows → Results.

This closes the fresh manual-save and observed-output/destination guard slice;
it does not complete Plan 020. Explicit pre-capture migration, the saved
`.crab/plots` declared-template closure, remaining authored-template/transform
failure variants, actual outage/in-flight proof, managed-service qualification,
other OS packages and CI/release gates remain open. The previous 75/52 record and
earlier scoped 95eca44 proof remain historical, not substitutes for this run.

### Declared saved-template retention

Two real CLI regressions reproduced the next gap on the 84/61 source: a saved
`.crab/plots` asset was absent from retained evidence, and an oversized override of
`linear` rendered the built-in instead of reporting unavailable custom semantics.
Before-fix records are `results-saved-template-before.log` and
`results-template-failure-before.log`.

Saved capture now accepts an explicit authoring-template root while parameters,
metrics, plots and outputs still read the owned workspace copy. It retains only
referenced render assets; the private Crab tree remains excluded. Execution and
reproduction keep the isolated worktree root. A real sibling regression commits
one template, changes/deletes the live file, then proves both an execution and its
reproduction retain the committed bytes. Shared template retention now gives a
resolved-but-unretained custom asset an unavailable identity, preventing same-name
built-in substitution. It preserves scalar observations and existing capture budgets.
No serialized DTO or dependency changes were needed.

A further real negative test exposed that resolving authoring resources could admit
another private `.crab` JSON file as a template. Retention now canonicalizes the
resolved resource and permits only the public `.crab/plots` namespace within Crab
state. Isolated execution-root ancestors are excluded from that relative-path
check. The private-boundary regression and existing sibling cases must pass before
claiming this slice complete. `results-template-private-before.log` records the
observed pre-fix admission; no real private credentials were used.

Seven saved/workspace CLI cases pass, including source removal, offline export,
built-in override, private-state exclusion, over-limit capture and the execution/
reproduction sibling. The native journey now covers an authored saved curve after
source removal, unavailable custom evidence without old SVG paint, and publication/
cold retrieval of the healthy template report after moving the entire local Results
cache away. These native assertions, refreshed packaging and scoped rendered review
are pending at this update. The extended matrix requires **89 capabilities / 65
cases**; the earlier 84/61 proof remains tied to source `4346b1a…`.

### Declared-template retention and recovery qualification

Final source-bound qualification passes **89 capabilities / 65 required cases**,
plus the separate 10,000-run catalog, on source
`f04cc3792c2ee803b60bb8cdb00a595ea9e75ef4595ae6836c7b08f8dac0b2a5`.
The parent independently recomputed current HEAD/source, unchanged shared runtime
`af20a84e…` and all four actual artifacts against the pre-run binding. The
independent verifier passes; archived/canonical evidence bytes match. Safe records:
`results-templates-89-evidence.json`, `results-templates-89-proof-summary.json`,
`results-templates-full-source-binding.json` and
`results-templates-89-independent-verifier.log`. Earlier pending/count statements
above describe development time; they do not qualify this final source.

All eight saved/workspace CLI cases pass. Private-template admission covers Crab
state, canonical traversal and Git state without publishing the fixture body.
Declared custom and same-name override assets survive source removal and offline
export. Execution/reproduction retain the committed tree's template. A resolved
but unretained custom template remains unavailable, preserving scalar observations.
The private template module owns bounded resource admission and immutable identities
through the existing resolver; it does not introduce a second renderer or reader.
No serialized DTO, configuration or dependency change was required by this slice.

The canonical package passes all three native journeys, including authored saved
curves after source deletion, unavailable custom evidence with 0.75 retained and
no stale SVG, and cold report retrieval after moving the whole local Results cache.
It checks the precise constraint/new-observation guidance and inspects the full
original diagnostic through the retained-template entry. Six final 1280/900px
captures are current, parent-opened and byte-identical to this run. The independent
finishing reviewer scores the single recovery-copy finding resolved; **ship**
covers that finding only. Results remains under Data Workflows → Results, with no
standalone primary navigation button. Both clients consume the capture owner's
message; no component layout, tokens, navigation or renderer changed in that fix.

The full matrix also passes six browser cases and the actual browser/HTTP/storage
publication/reload/revocation journey. Catalog proof returns 200 rows / 32 columns /
644,892 bytes for 10,000 runs. Pre-fix 4ca158 89/65 and prior 4346b1 84/61 evidence
remain separate historical records; the earlier unexplained retrieval timeout is
not erased or converted into a timing guarantee.

This closes the declared saved-template retention/recovery slice. Explicit
pre-capture metadata migration, other partial authored-template/transform variants,
actual provider outage/in-flight failures, managed-service qualification, other
native OS packages and CI/release proof remain open. Neither scoped visual ship nor
this Darwin-arm64/RustFS matrix completes Plan 020 or approves release.

### Explicit pre-capture metadata migration in progress

A real CLI before-case proves v1 experiment values remain visible through
`exp show` while Results is empty and `results migrate` is unrecognized.
`results-migration-before.log` records that gap. Tagged `v2.0.0-alpha.1` owns the
strict experiment-v1 shape; no release tag owns the new Results module. The
unshipped Results DTO now requires `ResultOrigin` on snapshots and summaries;
experiment metadata itself is unchanged. Owner-generated client types and CLI
JSON Schema were regenerated, without a fallback reader or guessed origin.

`crab results migrate <ids>` / `--all` uses the original metadata decoder behind
bounded regular-file admission. It gathers and validates sources before creating
roots. The shared conversion preserves recorded scalar metrics, names, timestamps
and reported status, without interpreting today's tree, parameter overrides, CLI
arguments or old stage hashes as captured evidence. Imported snapshots forbid
parameters, stages, attachments, plots and reproduction. Dataset identity still
requires an input witness; the real CLI test initially exposed the incorrect
assumption that a declared identity alone could fill this gap. The validator stays
strict; the regression now proves that refusal before comparing recorded values.

Create-only installation preserves valid current captures and refuses corrupt
existing roots. It reuses `tempfile` 3.27.0's selected no-clobber writer contract;
this is a no-overwrite guarantee, not a claim of universal filesystem atomicity.
Per-run I/O interruption can be retried. Missing original file bytes mean imports
must reject scalar overflow rather than truncate values. Both clients mark imported
status, share action restrictions, and retain explicit comparison/capture reasons.
Digest-based CLI actions and artifact promotion enforce the same evidence boundary;
existing non-Results experiment-workspace commands keep their tagged behavior.

Three actual CLI cases pass, covering preserved values/report independence,
idempotence, bad source preflight, non-downgrade and corrupt-root refusal. The
owner suite passes 54 with three explicit producer cases ignored; both client
typechecks, generated contracts/schema and four verifier cases pass. Native and
real HTTP/browser fixtures now assert imported provenance, disabled execution and
cold report recovery, but have not run against the refreshed package yet. The
extended required matrix is 94 capabilities /68 cases plus a separate10,000-run
catalog; final package, rendered finishing/documentation and full proof are pending.
Historical89/65 source f04cc379 remains qualified only at its own binding.

### Metadata migration and action-recovery qualification

Final source-bound qualification passes **94 capabilities / 68 required cases**,
plus the separate 10,000-run catalog, on source
`7c557170ab22ee13fa42ad88e59ddaa1f9948ce2eeb105f6e44e5030e3d830e3`.
The independent verifier matches current HEAD/source, shared runtime and all four
actual artifacts to the pre-run binding. Archived and canonical evidence bytes
match. Safe retained records: `results-migration-94-evidence.json`,
`results-migration-94-proof-summary.json`,
`results-migration-full-source-binding.json` and
`results-migration-94-independent-verifier.log`. The preceding pending statements
record development time; the pre-fix 675c0cc 94/68 and template f04cc379 89/65
records remain historical, with separate bindings.

The three migration CLI cases and all three packaged native journeys pass. Native
proof imports actual v1 metadata, displays its recorded 0.375 rather than today's
999, marks reported success as imported, disables actions without captured
execution evidence and retrieves the published immutable report after moving the
entire local Results directory away. The actual browser/HTTP/storage journey
shows imported 0.4/0.6 values and no invented curves in an immutable report, plus
the imported catalog status in a separately authorized namespace. Publication,
reload and access revocation remain exercised through the real service.

All eight refreshed native/browser captures were opened at both required widths
and match the current run byte-for-byte. The scoped finishing finding is resolved:
metadata-only execution limits and new-experiment recovery guidance precede local
workspace or immutable-report context in both clients. Browser action tooltips
follow the same shared reason. The **ship** verdict covers that one finding only;
it does not approve the whole surface or release. Results stays inside
Data Workflows → Results, without a standalone activity-bar entry.

Documentation reconciliation independently matches final evidence and all eight
captures; the incumbent DESIGN hash is unchanged. Parent format and diff checks
pass. The scoped review record is
`.impeccable/review/results-metadata-migration/documentation-review.md`.

The catalog returns 200 rows / 32 columns / 648,892 bytes for 10,000 runs. Its
observed cold/warm query times are 5.02/5.21 seconds in this fixture; these are
measurements, not a latency guarantee. This slice adds a strict unshipped Results
provenance field and one explicit migration path, not a second legacy reader.
Experiment-v1 metadata remains unchanged. Conversion and create-only storage live
with the shared owner; CLI composition owns source admission and opt-in migration.
The new production surface serves actual CLI, SDK, HTTP and both client consumers.

This closes explicit pre-capture metadata migration at the recorded scope.
Partial authored-template/transform variants, actual provider outage/in-flight
failures, managed-service qualification, other native OS packages and CI/release
proof remain open. The prior unexplained five-second retrieval timeout remains
unresolved historical evidence. This Darwin-arm64/RustFS proof does not complete
Plan 020 or establish release readiness.


### Live storage disconnection and in-flight session proof in progress

The existing native failure helper covers local corrupt/missing attachments and
post-close reads; it does not prove storage loss or session closure during a real
remote read. The extended packaged journey now sends normal signed requests
through a loopback HTTP proxy to the owned storage fixture. The proxy preserves
headers/bytes, holds an actual successful attachment response and then disconnects
the socket. The final fixture disconnects all current connections and refuses
subsequent requests for the app's entire storage endpoint until restored. No response payload,
native RPC or product retry policy is replaced. Both supported endpoint variables
select the proxy for Electron; synchronous fixture CLI setup retains its direct
endpoint so it cannot block the proxy's Node event loop.

Assertions require a real GET/200 before disconnection, visible failure with the
same immutable report/scalars/curves, successful evidence recovery after transport
restoration, and a second held response released by actual native session closure.
Returning to the retained local report must preserve both runs without late remote
evidence or errors. Native-test types pass. Scoped source 717347bd passed the
39.4-second packaged journey with per-object transport disconnection, recovery and
in-flight close. Its three new captures were opened: real boundary error and
Refresh view remain visible at 1280/900 widths; the local report recovers without
late evidence. It does not claim specialized network-cause copy or browser outage
proof. The final whole-endpoint fixture and improved subject captures are now in
the complete matrix, requiring **96 capabilities / 68 cases** plus the separate
10,000-run catalog. Four verifier cases and final native-test types pass. Full
qualification is running; no new failure capability is qualified yet. The 7c557170
94/68 evidence above remains tied to its preceding source, with no outage/in-flight
claim. Production source, generated contracts, renderer runtime and actual package
artifacts are unchanged by this proof extension; no package rebuild is required.

### Native transport-failure qualification

The final matrix reached terminal exit 0 and passes **96 capabilities / 68 cases**
plus the separate 10,000-run catalog on source
`bc20aa4f4bd75f2ddf91e0037d872290c4554cf8b4a66705a3a6dadc9c5ea456`.
Independent verification matches current HEAD/source/runtime and all four actual
artifacts to the pre-run binding. Runtime and package hashes remain identical to
the preceding 7c557170 product. Safe records are
`results-outage-full-source-binding.json`, `results-outage-96-evidence.json`,
`results-outage-96-proof-summary.json` and
`results-outage-96-independent-verifier.log`. Archived/canonical evidence matches
at SHA-256 `6490ad20d379491ad0722699a23e7af81f4073c4807cf6b0b7d1fb98028f233d`.

All three native journeys, six browser cases and the actual browser/HTTP/storage
case pass with one successful attempt each and no skips/flaky results. The native
transport test receives a real attachment GET/200 before dropping every connection
and denying the app's entire endpoint. The SDK boundary error is visible while
the immutable report, 0.1/0.2 values and four verified curve paths remain intact.
Restoring forwarding and selecting the attachment again returns its actual CSV.
A second held GET/200 is released by native session close; the local retained
report then opens without an error or late remote evidence. Both new checks are
required by the verifier; omitting either is rejected by its regression suite.

Three final captures were opened and archived byte-identically: disconnected
1280/900 × 1000 and local recovery 1280 × 1000. The error/Refresh view control,
report identity and comparison remain visible. Existing narrow workflow-list
compression is unchanged. These are runtime proof captures, not a new visual
review or a broader ship verdict. Production UI and browser failure behavior
were not changed. Native-test types, four verifier cases and diff checks pass.
Catalog cold/warm queries measured 4.87/4.90 seconds; response remains 200 rows /
32 columns / 648,892 bytes. These fixture measurements establish no general SLA.

This closes native direct-storage endpoint-loss/recovery and in-flight session
closure at the recorded Darwin-arm64/RustFS scope. Browser storage outages,
other provider failures, partial authored-template/transform variants, managed
service qualification, other native OS packages and CI/release proof remain
open. The earlier unexplained retrieval timeout remains unresolved. Whole
Plan 020 is still in progress.

### Browser storage-disconnection proof in progress

The actual HTTP/browser fixture now routes its S3 provider through a test-only
loopback proxy. Hyper handles persistent HTTP connections, and reqwest streams
the original signed headers/body to real storage; successful bodies are never
invented. A separate private fixture listener arms a retained-attachment hold,
reports the actual GET/200 witness, disconnects the server's entire endpoint and
restores forwarding. Existing provider retries/pooling and production HTTP routes
are unchanged. The cfg(test) owner drains its connection tasks and closes both
listeners; outer harness cleanup restores storage before shutting down owners.

The browser journey still publishes from the two real CLI runs and checks current
grants/revocation. New assertions require visible HTTP 503, the same immutable
report and both metric values/four curves during storage loss, then actual CSV
recovery after reconnection. The final membership check can itself return the
existing permissions-unavailable 503 while storage is down; an outage must not
be represented as a new grant or an empty result. Embedded UI build/type checks
pass. Scoped real-storage/browser proof is running; source bc20aa4f's 96/68
qualification remains historical and does not cover this new browser check.

The initial scoped run compiled successfully but its hold assertion refused the
journey: the selected object_store 0.14.1 builder gives `AWS_ENDPOINT_URL_S3`
precedence over generic `with_endpoint`, so that fixture override did not route
requests through the proxy. The selected upstream `S3Endpoint` contract and build
selection were read; the fixture now overrides that service-specific key directly.
The GET/200/held assertions remain unchanged. First-failure source 229193bb,
logs and Playwright output remain under `results-browser-outage-scoped`;
corrected proof will use a separate output directory. This failure establishes
no browser outage or product defect.

Corrected source 29a8596e passes the scoped actual HTTP/storage/browser journey:
one Playwright case, no skip/failure/flaky result, 43.4 seconds in the browser.
The proxy observes GET/200 before disconnection. The server then returns its
existing `membership_unavailable` 503 because current permissions cannot be
revalidated while storage is down. The report identity, 0.1/0.2 and four curves
stay intact; reconnecting and selecting the attachment returns its actual CSV.
Publication/reload and subsequent authorization/revocation still pass. Three
full-page wide/narrow/recovery captures were opened; the notice and Retry are
readable, with existing internal data scrolling. This is runtime proof, not a
new visual ship verdict. The required matrix now includes
`browser_storage_disconnect`: **97 capabilities / 68 cases** plus the separate
10,000-run catalog. Full proof on that final verifier contract remains pending.

Capture-isolation follow-up: the outage screenshots leave the browser at 390px,
which would mislabel the following existing lineage capture as wide. Restore the
caller's viewport in the outage block's cleanup before final rendered proof. The
running 1c3e4af6 matrix remains frozen; its result cannot qualify that later test
cleanup. This concerns evidence composition, not production layout.

Shared-boundary correction: the viewport-corrected ee2c319a run reached the real
GET/200 and transport loss, then returned HTTP 504 from `app::admit`'s existing
30-second request budget instead of the earlier observed membership 503. It stays
failed and source-bound, not replaced by the prior pass. The captured read error
incorrectly warns about a possibly completed write. Extend this slice's scope to
`crates/crab-server/src/app.rs` and its shared HTTP integration tests: safe read
timeouts must describe retrying the read, while mutation timeouts retain their
existing ambiguity/recovery warning. All sibling route consumers share that owner;
Results must not add another timeout or rewrite generic errors locally. The
browser check will validate the two documented status/message pairs rather than
guessing that one failure boundary always wins. Preserve timeout duration,
authorization, cancellation and mutation behavior. Rendered copy review is
required after this now user-visible correction.

The correction lives in shared admission, selected with http 1.5's safe-method
contract. The existing 30-second budget, response envelope, status, permits and
mutation wording stay intact. A real middleware regression drives GET and POST
through the same stalled handler, concurrently, and checks both 504 recovery
messages. The browser retains precise allowed 503/504 message pairs, then holds
a second actual GET/200 without disconnecting it to deterministically exercise
the read-timeout presentation/recovery at both widths. It restores its original
viewport before sibling captures. The verifier now requires both HTTP and
browser timeout proof: **99 capabilities / 69 cases**, plus separate 10k catalog.
The failed ee2c319a record, reports and screenshot remain archived under
`results-browser-outage-timeout-before`; older 97/68 proof is not current proof.
UI build/types, four verifier cases, formatting and diff checks pass. Focused
middleware proof is running; real-browser recaptures and full qualification are
pending. This scope preserves the existing neutral Operate system and changes no
navigation, tokens or layout. No context/detector rerun or drift repair applies.

### Browser storage/read-timeout qualification

Final source-bound qualification reaches terminal exit0 and passes **99 capability
checks / 69 required cases**, plus separate10,000-run catalog, on source
`a116cbe5c6c0de969c69b6437b6a63edaed944a7352881bc370886abfd3031bf`.
Independent verification matches current HEAD/source/runtime and all four actual
native artifacts to the pre-run binding. Safe records are
`results-browser-storage-full-source-binding.json`,
`results-browser-storage-99-evidence.json`,
`results-browser-storage-99-proof-summary.json` and
`results-browser-storage-99-independent-verifier.log`. Archived/canonical evidence
bytes match SHA-256
`defc831ebe05ce534b79a76bbc3f104cd7ce1901d62fdf427355698f176c1e4a`.

The two HTTP cases include the real 30-second GET/POST admission regression.
All three packaged native journeys, six browser cases and the actual browser/
HTTP/storage case pass with one successful attempt each, no skipped/flaky or
unexpected result. Real S3 disconnection permits only the documented 503/504
status/message pairs; a second actual GET/200 is held to deterministically reach
shared admission's 504. Both failures preserve the frozen report, observations
and four curve paths. Restoring transport and selecting the attachment recovers
its real CSV. Shared safe-read recovery no longer suggests a completed write;
mutations retain their original ambiguity warning. Browser Refresh view accurately
names read refresh. Authentication/current-grant checks remain on the same path.

The eight parent/reviewer-opened images remain byte-identical to the separate
current-source scoped run: disconnection and read timeout at 1280/390, two actual
CSV recoveries at390, and subsequent lineage at1280/390. They are not claimed to
be images of the later full run's newly queued records. The final canonical PNG
widths were independently verified too. The finishing review contains all five
contract sections and **ship** for this copy/preservation slice only, no material
fixes; broader shell/chart treatment is not approved. Documentation reconciliation
independently verifies the final binding, evidence and all eight scoped captures;
the record is `.impeccable/review/results-browser-storage/documentation-review.md`.
DESIGN and existing PRODUCT/sidecar presence remain
unchanged. Context/detector were not rerun.

Production growth is six shared-admission lines and a browser control-label
correction; no new schema, dependency, config, renderer or provider policy. The
new test-only proxy is isolated with its HTTP fixture and drains connections on
close. Server/UI were rebuilt. No native CLI/agent manifest depends on crab-server,
so unchanged native/runtime artifact hashes are expected and independently
verified. Catalog measured5.51/4.94s cold/warm,200rows/32columns/648,892bytes;
render/zoom/cursor168.1/44.6/8.2ms. These are fixture observations, not an SLA.

This closes browser owned-endpoint loss/read-timeout/recovery at the recorded
Darwin-arm64/RustFS scope. Failed229193bb andee2c319a records, prior29a8596e scope,
pre-cleanup1c3e4af697/68 andbc20aa4f96/68 remain historical. The earlier unexplained
cold-retrieval timeout is unresolved. Authored-template/transform variants,
managed-service/other-provider qualification, other OS packages and CI/release
proof remain open; whole Plan020 remains in progress.

The pre-cleanup 1c3e4af6 matrix passes 97/68 plus the separate 10,000-run catalog;
it was independently verified and archived with its original binding under
`results-browser-outage-pre-cleanup-97-*`. The outage block now restores the
caller's viewport in cleanup. Browser types and focused formatting pass. Final
qualification is running on the corrected source; the earlier archive does not
qualify that capture-isolation change.

### Authored-transform runtime failure follow-up

Current-source offline reproduction confirms a gap in the shared renderer:
Vega 5.33.1 / vega-dataflow 5.7.8 logs a failed calculation and resolves
`runAsync()`; the probe returned success with zero painted paths and zero alerts.
The completed diagnostic is retained as `results-authored-failures/before.json`
in this checkout's external target. Previous 99/69 evidence does not prove this
case or any subsequently changed source.

Keep this correction in the existing chart runtime, used by both React clients,
immutable HTML and workspace `metrics plot` exports. Use the dependency's logger
contract to observe runtime errors, finalize failed views, remove partial paint
and report the failure through each caller's existing alert surface. Do not
throw from Vega's event logger: its event dispatcher does not await the returned
promise. Initial failures reject the render operation; subsequent failures notify
the owning view. Preserve healthy charts, scalars, retained source evidence and
the immutable report identity. No second parser, template rewriting, dependency
change, navigation change or new visual system.

Required proof: real CLI execution/capture/export with healthy, invalid-expression,
initial calculation and user-triggered calculation plots; network-denied browser
inspection of the actual export; rendered browser/native preservation and failure
recovery where feasible; source/runtime/package binding refreshed after changes.
Whole Plan020 and platform/provider/CI/release gates remain open.

#### Authored failure scoped proof and full qualification in progress

Frozen source `b7df97c392fb3f23b162c143dbfd123bb37de133d0682f4adf867b5d2d852120`
passes actual CLI execution/capture/Apply/export, network-denied immutable and
workspace-metrics HTML, browser Results and packaged-native Results. The browser
and native cases take7.31/7.00s respectively, one expected each with no skipped,
unexpected or flaky attempt. Initial calculation, user-triggered calculation
and syntax failures are visible. Healthy curve, score0.25 and report identity
remain; report reload/reopen restores the user-triggered chart's initial state.
Failed chart views, bindings and export controls are removed. Both native and
browser wrappers use the same non-throwing runtime diagnostic owner. Syntax
diagnostics retain their existing dependency wording.

The parent and fresh reviewer opened all six source-bound captures: offline and
browser at1280/390, native at1280/900. The reviewer returned **ship**, all five
sections present and no material fixes, for this failure/preservation scope only.
Record: `.impeccable/review/results-authored-failures/finish-review.md`. Native
capture intentionally scrolls the inner Results panel to errors and surviving
curve; browser/offline images are full-page documents from their root. Incumbent
focus-link and narrow-shell behavior remain outside this verdict. DESIGN and
existing system-file presence are preserved; context/detector were not rerun.

Runtime SHA-256 is `fafdbcbc1d546ae7b02fd1328a40fc72e4811317d965016f6d3111d8d89d65fc`.
Debug CLI, packaged CLI, sidecar and app archive are independently bound before
the expanded full102-capability/72-case qualifier starts. That process is live;
old99/69 evidence does not qualify this changed source, runtime or application.
The current-source scoped records are retained in `results-authored-failures` in
this worktree's external target. Its pre-fix diagnostic and failed initial
checkbox-confirmation assumption remain retained separately. The latter reached
the intended failure but incorrectly waited for checked state after the control
was finalized; tests now click and assert actual visible side effects.

UI build and normal client types, focused new native test types, Rust formatting,
runtime reproducibility, four verifier tests and native packaging pass. An extra
broad `tsconfig.tests.json` check fails on existing missing-JSX and unrelated test
diagnostics; its config is byte-identical to HEAD. Do not report that check as
passing. The mounted volume was nearly full; only this checkout's disposable
debug incremental cache was reclaimed. No qualification records or other target
directories were removed.

The runtime adds one diagnostic/lifecycle path shared by all callers; no parser,
serialized model, dependency, provider policy, config, token or navigation
change. Remaining partial template/capture variants, provider/managed-service
access, other native OS packages, the unexplained earlier retrieval timeout and
CI/release proof stay open. Whole Plan020 remains in progress.

#### Authored failure qualification — current bound source

The expanded qualifier reached terminal exit0 on the same frozen `b7df97c3…`
source: **102 capabilities /72 required cases plus separate10,000-run catalog**.
This supersedes the preceding in-progress broad-proof status. Parent independent
verification recomputed HEAD/source/runtime/debug CLI/packaged CLI/sidecar/app
archive against the pre-run binding and called the full evidence verifier.
Archive/canonical evidence bytes match at
`969acadccf7cfa0424ece62e8249be759cff5563fa0cf0b893c5c6f9241ce536`.
Completed records: `results-authored-full-source-binding.json`,
`results-authored-102-evidence.json`, `results-authored-102-proof-summary.json`
and `results-authored-102-independent-verifier.log` in the external target.

All4 native,7 browser and1 actual HTTP/browser cases have exactly one passed
attempt, zero skipped/unexpected/flaky results and no report-level errors. This
includes actual queued comparison/restart/cold retrieval and the authored-error
journey through real retained CLI objects. Canonical authored PNG widths were
checked too. The six independently reviewed packet images remain from the
separate current-source scoped run; they do not claim byte identity with the
later full run's new record IDs. The visual **ship** scope does not expand.

The catalog check has10,000 runs,200rows,32columns and648,892response bytes;
build4.74s, cold5.05s, warm5.10s. Maximum-overlay render/zoom/cursor measured
178.1/45.1/8.1ms. These fixture measurements do not establish a retrieval or
performance SLA. The earlier unexplained five-second retrieval timeout remains
unresolved. No failing baseline/configuration was changed to pass qualification.

Non-test growth funds one shared diagnostic/lifecycle owner and existing caller
bindings, replacing the separate resize-error append branch. No duplicate
parser, template substitution, storage change, dependency patch, setting or
navigation entry was added. The extra broad test-project typecheck remains a
recorded failure despite passing normal client and focused new native-test types.
Remaining template/capture variants, provider/managed-service access, other native
OS packages and CI/release gates remain open. Whole Plan020 stays in progress.

Fresh documentation reconciliation independently verified the final102/72
binding, artifacts, evidence and six scoped captures, and updated the design
guide. Parent read both completed files. Record:
`.impeccable/review/results-authored-failures/documentation-review.md`.

### Partial template evidence admission follow-up

A new owner regression demonstrates that a missing custom capture rejects the
whole chart as incompatible, even when another selected run has valid data and
template bytes. Capture intentionally rewrites missing templates to an unavailable
identity; `chart_spec` currently compares that identity before evidence admission.
Completed pre-fix test: `results-partial-template-before.log` in the external
target,1failed/0passed, actual `(Unsupported,2,false)` versus required
`(Missing,2,true)`. Prior102/72 qualifies its original source only.

Correct ordering inside the shared chart owner: retain existing non-template
declaration checks; verify each revision's own data/coordinate/template inputs;
skip unavailable revisions with their classified guidance; choose template and
counting policy only from admitted evidence. Compare admitted template identities
and bytes strictly. Never lend one run's template to another or reinterpret an
unavailable custom template as a builtin. No serialized model, reader, transport,
capture, parser, config or navigation change is needed.

Prove missing/not-captured/corrupt/denied templates in both selection orders,
preservation of valid-incompatibility refusal and builtin counting, and actual
CLI/native/browser rendering of partial saved observations. Retain remote
publication/retrieval proof for the resulting explicit partial report. Refresh
source/package-bound qualification after this correction. Whole Plan020 remains
open; existing platform/provider/type/CI/release limitations remain recorded.

#### Partial-template scoped proof and expanded qualification

Frozen source `644b8eebaaf08f6b2317a579a43b700f903910e2ceb6510273d01c87974bc0ec`
passes the new five-state/both-order owner regression, existing12 chart/9 template/
5 matrix cases, real CLI partial-report publication/cold retrieval/export and
the earlier authored-runtime CLI sibling. The partial report keeps metrics0.1/0.2
and loss1/2 only; unavailable3/4 data never enters its chart. Its original local
Results state, source template and YAML are absent before retrieval/export;
the cold catalog has no run roots. The report retains explicit not_captured
evidence and the same immutable identity.

Browser and packaged-native cases pass in9.05/8.15s, one expected each with zero
skipped/unexpected/flaky attempts; offline network attempts are zero. Native reads
the actual cold report with zero workflows, inside Data Workflows → Results.
All six scoped captures were parent/reviewer/documenter-opened and independently
hash/dimension checked at1280/390 offline/browser and1280/900 native. The fresh
five-section reviewer returned **ship**, no material fixes, at this partial
availability scope only. Its dependency row was corrected after re-reading the
selected object_store0.14.1 error/source contract; no dependency changed.
Record: `.impeccable/review/results-partial-templates/finish-review.md`.

UI/focused-native test types, production workflow-library Clippy with warnings
denied, Rustfmt, focused Prettier, runtime reproducibility, four verifier cases,
native packaging and diff checks pass. Renderer and React sources are unchanged;
the corrected shared Rust owner is present in the rebuilt CLI/sidecar. Actual
debug CLI, packaged CLI, sidecar and unchanged app archive are bound before the
expanded105-capability/74-case qualifier. That process is live; earlier102/72
does not qualify this source. Scoped evidence is under
`results-partial-template-scoped` in this checkout's external target; the full
pre-run binding is `results-partial-template-full-source-binding.json`.

The canonical option replaces the separate observed flag and chooses template/
counting policy only after evidence admission. Existing invalid declaration,
valid-template incompatibility, parse and unsafe statistical-sampling refusals
remain. No new format, config, parser, data store, renderer or navigation path.
Fresh scoped documentation reconciles the formerly stale normative template
paragraph; final full-proof reconciliation remains pending. Existing broad
test-project type failures, prior unexplained retrieval timeout and remaining
provider/managed-service/other-OS/CI/release gates stay open.

#### Partial-template qualification — current bound source

The expanded qualifier reached terminal exit0 on frozen `644b8eeb…` source:
**105 capabilities/74 required cases plus separate10,000-run catalog**. Parent
independently recomputed current HEAD/source/runtime/debug CLI/packaged CLI/
sidecar/app archive against the pre-run binding and ran full evidence verification.
Archived/canonical evidence bytes match at
`9e2814cb3aee7a021607ad2332feaacdee5df8c9d3161e4a6bbbf19704e46b8f`.
Completed records in the external target:
`results-partial-template-full-source-binding.json`,
`results-partial-template-105-evidence.json`,
`results-partial-template-105-proof-summary.json`,
`results-partial-template-105-independent-verifier.log`.

All4 packaged native,7 browser and1 real HTTP/browser cases have exactly one
passed attempt, zero skipped/unexpected/flaky results and no report-level errors.
Canonical partial-report PNG widths match intended viewports. The six scoped
review images remain their original scoped-run bytes, not the later full run's
new identities/pixels. The scoped **ship** verdict is unchanged. Final proof
supersedes pending broad-qualification language at this source, without claiming
whole Results, Plan020, DVC parity or release approval.

The separate catalog has10,000 runs,200rows,32columns,648,892response bytes;
build4.72s, cold5.09s, warm4.96s. Maximum-overlay render/zoom/cursor measured
174.1/44.1/8.4ms. Measurements are fixture observations, not a retrieval/SLA claim.
The prior unexplained retrieval timeout, broad test-project type failures and
remaining provider/managed-service/other-OS/CI/release gates stay open. Genuine
template incompatibility, invalid parsing and unsafe statistical sampling remain
explicit existing refusals, rather than being reinterpreted through another
run's template or current workspace bytes.

Fresh documentation reconciliation independently verified final105/74 evidence,
current source/runtime/four artifacts, pre-run binding and scoped/canonical image
boundaries. Parent read the final guide and record:
`.impeccable/review/results-partial-templates/documentation-review.md`.

### Completion audit: regression and dependency gates

The requirement-by-requirement audit found two missing continuous gates and four
format violations in the browser integration. The SDK dependency checker did not
exercise the new Results feature; native workflow CI did not run the existing
experiment/plot regressions alongside Results. Both gates are now wired.
`workflow-native.yml` uses one runner-temporary Cargo target for compilation and
release smoke consumers. POSIX CLI fixtures remain explicitly Unix-only; Windows
SDK/schema checks are not presented as Windows Results execution acceptance.
Target setup writes `GITHUB_ENV` in an early step, following GitHub's
[environment-file contract](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-commands#setting-an-environment-variable).
The primary [context availability table](https://docs.github.com/en/actions/reference/workflows-and-actions/contexts#context-availability)
excludes `runner` from job-level `env`; YAML parsing alone did not establish this
dependency contract. The final seven regression/build/test gates each retain
their own command exit status. Separately retained wiring hash:
`e517b6f6da1fc81bae2f53ff9844bae15103a6fd147b8b509e69229a53dd639e`.

Current-source dedicated checks before the formatting-only browser correction:
36 existing experiment regressions, 55 Results owner tests, 39 metrics/plot
regressions, 22 CLI Results tests, schema drift and Results schema validation,
and minimal/Results SDK library builds pass. The owner suite includes generated
client-contract equality. Its three ignored cases are the explicit declaration
writer and dedicated scale/maximum-overlay producers; the two ignored CLI cases
require actual owned S3 and remain part of the full acceptance producer.

The corrected SDK boundary checker passes all 14 cross-target dependency profiles,
including four Results combinations, and its three focused boundary tests pass.
Both client typechecks and all 36 preference/session tests pass. Browser-wide
Prettier now passes after formatting its four reported integration files; the
production UI build and reproducible runtime/license asset check pass. Web docs
typecheck, 10 tests and links (399 pages / 4,308 fragments) pass; lint exits zero
with the existing 16 warnings. No dependency versions, integrity values,
manifests, lockfiles, stored/wire shapes or runtime asset changed in this audit.

The desktop-wide lint command still exits before linting: tracked HEAD and
`origin/crab-v2` contain no applicable ESLint configuration. This is retained as a
failed command, not a pass or a new ignore/configuration. The previously recorded
broad desktop test-project type failures also remain distinct from the passing
product typecheck. Other provider/managed-service and native OS qualification,
actual hosted CI/release proof, and the earlier unexplained five-second retrieval
timeout remain open. Wiring a job does not prove that job ran.

The previous bound 105/74 acceptance and its root/browser artifacts were archived
before a fresh full run. The fresh run completed successfully against source tree
`3ecbbc32a93a7cc389432ba933571f79f49063abf6eb273538409794d8ebb708`;
its pre-run source/runtime/four-artifact binding is retained in
`results-completion-audit/full-source-binding.json` under the dedicated external
target. Do not use the old source binding as proof of the formatting/gate edits.
Whole Plan 020 remains IN PROGRESS.

The parent independently verified current HEAD/source/runtime and all four actual
artifacts against that pre-run binding, then ran the full evidence verifier:
**105 capabilities / 74 required cases**, plus the separate 10k catalog. All four
native, seven browser and one actual HTTP/browser case have exactly one passed
attempt, with zero skipped/unexpected/flaky results and no report errors. The
runtime and four executable/package digests are unchanged from the preceding
partial-template run; no native product or chart runtime changed in this audit.
Current evidence SHA-256:
`5a75d6dd55f77c5b2feebc0f65ae62e61fa3c9284e75d7d4653139e5298ba642`.
Canonical and retained bytes match in `results-completion-audit/accepted/`;
`proof-summary.json` and `independent-verifier.log` retain the reconciliation.

The 10k catalog returns 200 rows / 32 columns / 648,892 bytes; build/cold/warm
times are 4.61/5.12/5.10 seconds. Maximum overlay render/zoom/cursor measured
187/47/9 milliseconds. Renderer heap after ten switches and explicit GC is
737,644 bytes below its warmed baseline. These are fixture observations, not a
retrieval SLA or proof of native/server reader and OS-handle cleanup. Six current
partial-template PNG signatures/dimensions were checked and archived; earlier
scoped visual reviews remain scoped to their own captures and source bindings.

Completion coverage is assessed against the ordered slices, rather than the
number of green capability flags:

| Required surface | Authoritative proof inspected | Remaining scope |
|---|---|---|
| Shared schema, scalar identity, queries, bounds and generated types | Owner `results` suite (55 passing), generated client equality, CLI schema drift/validation | No schema/dependency change introduced by this audit; full acceptance must retain the current source binding. |
| Admission-time capture, failure/cancellation/cache and exact restoration | Existing `workflow_exp` (36), non-S3 `workflow_results` (22), full producer's input/settings/workspace/checkpoint cases | Supported provider/platform qualification stays separate from these local executable fixtures. |
| Shared plots, retained templates, runtime errors and offline export | Metrics/plot suite (39), runtime/license reproducibility, full native/browser/offline producer | Genuine parse/template/sampling refusals are intentional documented outcomes; no unnamed variant is treated as a completion gate. |
| Independent reports, verified transfer, maintenance lifetime and recovery | Full producer's real S3 transfer, interrupted transfer, repository GC, source removal, cold pull and report reopening | Owned RustFS does not qualify AWS/GCS/Azure or managed-service grants. |
| SDK/session bounds and Apply/Reproduce/Resume/Promote | Minimal/Results builds, all 14 dependency profiles, SDK admission test and packaged action journey | Newly wired native CI has not run; Windows schemas/SDK tests do not substitute for packaged Windows action proof. |
| Native placement, searchable comparison and persistent state | Packaged Data Workflows → Results journey plus 36 preference/session tests | No standalone Results activity-bar entry; native OS packages beyond the recorded Darwin-arm64 run remain unqualified. |
| Browser route, current grants and immutable report publication | Actual HTTP/storage/browser producer, including denied/revoked and cross-repository access | Actual container/release and provider qualification remain distinct from the in-process real HTTP server. |
| Scale, chart interaction and lifecycle envelope | Dedicated 10k catalog, maximum overlay timings, browser ten-switch heap check; the follow-up below adds two real twenty-cycle reader/descriptor/heap journeys | Both scoped measurements and the full bound 107-check Darwin-arm64/RustFS run pass. Other platform/provider qualification remains separate. |
| Format/types/docs and distribution | Passing product types, focused Results lint, browser formatting/build, Rust format, docs checks and bundled runtime/license bytes; the records below prove default Make installation and the actual Darwin-arm64 release archive with both offline HTML exports | Desktop-wide lint configuration and broad test-project types remain failed gates; other native release targets and actual hosted CI/release evidence require their own retained results. |

### Reader and handle lifecycle qualification in progress

The next proof extends the existing real native and HTTP/browser journeys;
production contracts and renderers remain unchanged. Reuse the storage observer
for server logical reads and the native fixture proxy's response lifetime for
physical reads. At settled comparison boundaries every started read must have a
terminal outcome, including the existing disconnection/session cancellation cases.

Warm both destinations for five cycles, then measure twenty actual comparison
switches with chart destruction and reloads. Retain per-cycle reader, renderer
heap and process file-descriptor observations. The dedicated fixture ceiling is
eight descriptors above the warmed baseline per measured native main/sidecar or
server process, and sixteen MiB of post-GC renderer heap growth. These are bounded
qualification envelopes, not new product configuration or a general leak-free
claim. Do not relax them to hide a failed measurement. The existing macOS `fd(4)`
and `lsof(8)` contracts were read; a real sixteen-file probe verified `/dev/fd`
returns to its original count. Other OS evidence must come from its own run.

The scoped native and actual HTTP/browser journeys now pass at source tree
`b5f50facaabeab1fafea59686f19fde6aeef205fa927910673c7bf7ca8713465`.
Native main/sidecar descriptors start and finish at 104/12, with zero measured
descriptor growth; renderer heap finishes 3,882,960 bytes below its warmed
baseline. All 342 measured physical reads finish; after disconnection and native
session cancellation all 367 started reads finish, with zero active readers.
The server starts/finishes at 72 descriptors, briefly reaching 73; renderer heap
finishes 154,328 bytes below baseline. All 1,972 observed server reads finish.
Each journey retains 21 samples (baseline plus twenty measured cycles), after
five warmups. The server uses the existing `StorageObserver` through response
termination; the native proxy requires both upstream and downstream lifetimes
to terminate before releasing its physical reader. A single locked server
counter snapshot avoids falsely unbalanced samples from concurrent completions.

The first native extension completed its twenty measured cycles but failed the
following fault scenario by reusing a pre-reload session ID. It is retained under
`results-resource-envelope/before-stale-session/`. Reload had correctly admitted
a replacement native session; the existing stale-session guard refused the old
handle. The fixture now reopens the same repository through the normal owner
before capturing the next RPC scope. No production session guard changed.

Focused native/browser types, Results fixture lint, Rust/fixture formatting,
runtime/license reproducibility, all five evidence-verifier tests, strict server
test Clippy and `git diff --check` pass. Negative verifier checks reject missing
owners/cycles, stale counters, active/unbalanced readers, omitted cancellation
cleanup and excessive descriptor/heap growth. The dedicated producer deletes
old resource artifacts, embeds the new measurements, and requires two additional
named capabilities rather than accepting a success flag without raw samples.

The scoped packet is `results-resource-envelope/` under this worktree's external
target: `scoped-source-binding.json`, `scoped-proof-summary.json`, the two raw
resource envelopes and exact single-attempt Playwright reports. Its HTTP fixture
seeds retained real CLI observations from the preceding 105-check run; the native
case queues fresh real runs. Full fresh CLI/native/HTTP/browser qualification is
now running against the same source with 107 capabilities / 74 required cases,
plus the separate 10k catalog. `full-source-binding.json` was captured before
that run. Product code, chart runtime, public DTOs, manifests and lockfiles are
unchanged by this follow-up. Desktop packaging includes renderer/Electron outputs
and explicit native resources, not these E2E sources; the four actual artifacts
remain hash-identical. No new visual verdict or whole-plan completion is claimed.

### Lifecycle qualification — current bound source

The fresh full producer completed at source tree
`b5f50facaabeab1fafea59686f19fde6aeef205fa927910673c7bf7ca8713465`:
**107 capabilities / 74 required cases**, plus the separate 10k catalog. Parent
reconciliation independently matches current HEAD/source/runtime and all four
actual artifacts to the pre-run binding. The four native, seven browser and one
actual HTTP/browser case each have exactly one passed attempt, with zero skipped,
unexpected or flaky results and no report errors. Both raw resource envelopes
match the embedded evidence and pass the independent measurement verifier.
Evidence SHA-256:
`ad012d1a3689328d4744e613ad82ca8567e465947556efb3f2eddd357b30635c`.
Retained and canonical evidence bytes match. The new current record supersedes
pending language for this binding; earlier 105-check and scoped records retain
their own identities and ceilings.

The full native trace starts/finishes at 104 main and 12 sidecar descriptors,
with a transient sidecar maximum of 13. Its post-GC renderer heap finishes
4,602,972 bytes below baseline. By the last measured sample all 341 observed
physical reads finish; after the subsequent fault/cancellation cases all 366
started reads finish, with zero active readers. The full HTTP trace starts/
finishes at 71 descriptors, briefly reaching 72; its renderer heap finishes
277,116 bytes below baseline and all 1,971 observed storage reads finish. Both
traces retain baseline plus twenty measured samples after five warmups. Maximum
post-GC heap increase is zero in each recorded trace. These measured process and
reader envelopes close the previously missing repeated-lifecycle proof at this
platform/provider scope, without claiming results for another environment.

The 10k catalog returns 200 rows / 32 columns / 648,892 bytes; build/cold/warm
times are 4.39/4.88/4.86 seconds. Maximum-overlay render/zoom/cursor measured
165.7/42.8/7.3 milliseconds. Runtime and four CLI/package artifact hashes remain
unchanged; `packages/desktop/DESIGN.md` retains its previous digest. No new visual
verdict, dependency change, test ignore/baseline relaxation or product fallback
was introduced. Scoped artifacts remain in `results-resource-envelope/`; full
raw artifacts are retained in its `accepted/` directory. The pre-run binding,
`full-proof-summary.json`, `independent-verifier.log` and nonsensitive host
hardware metadata accompany them. The measured host is Apple M2 Max, twelve
logical CPUs, 32 GiB RAM, Darwin arm64, with Node 22.20.0. Existing desktop check failures, other provider/
managed-service and OS packages, actual CI/release producer execution and the
historical retrieval timeout remain open. Whole Plan 020 remains IN PROGRESS.

### Default Make installation and standalone offline exports

On 2026-11-19, the actual default `make -C crab install` completed on Darwin
arm64 at the same bound source tree
`b5f50facaabeab1fafea59686f19fde6aeef205fa927910673c7bf7ca8713465`.
The original Make target built its FUSE and non-FUSE release variants and cache
server, then installed them through `scripts/release/install-binaries.py` into
an isolated prefix under this worktree's external target. Both `PREFIX` and
`CARGO_BIN` were explicit; the latter names an absent directory, so the install
did not mirror into the user's Cargo bin. The expected Git/NFS helper symlinks
and all three installed binary digests were independently checked. No feature
override, manual installation or runtime npm step was used. Installed CLI SHA-256:
`d086b305b722e5d7d1217a7ab1df48504442775aed118ad3fb8b385e9346b1fa`.

With Node and npm absent from its PATH, that installed CLI queued and executed
two real dataset evaluations. Captured scores are 0.1/0.2; loss falls from 2.5
to zero, with a verified -2.5 baseline delta and improvement under the explicit
minimize objective and matching witnessed dataset/split/evaluator identities.
It created an immutable report and exported HTML, applied the first run and
exported legacy `metrics plot` HTML. After removal of eleven workflow, parameter,
dataset, code, plot, template, model and comparison source files, report export
was byte-identical and reopening returned the unchanged frozen report.

Chrome 157.0.8103.79 opened both actual files with the context offline and HTTP(S)
requests aborted. Results rendered two SVG charts, four curve series and two
embedded images; legacy metrics rendered two charts, two series and one image.
The authored template title rendered in each export. Results Zoom updated both
linked ranges from automatic to 0.25–0.75. Neither export attempted network
access or emitted page/console errors. Both full-page captures were visually
inspected. The packet `results-installed-cli/` retains commands, terminal install
and smoke results, scripts, HTML, DOM observations, screenshots and an independent
`proof-summary.json`. Initial fixture key and dash-selector mistakes are retained
separately; no product code or expected-result file changed to obtain this proof.

Independent reconciliation confirms the original source/runtime and all four
107-check CLI/package artifacts remain unchanged, and re-verifies the retained
full evidence at `ad012d1a3689328d4744e613ad82ca8567e465947556efb3f2eddd357b30635c`.
This closes the default Make-install/standalone offline gate for this platform;
it does not replace native release, other OS/provider or hosted CI evidence.
Results stays under Data Workflows → Results, with no standalone native entry.
Whole Plan 020 remains IN PROGRESS.

### Native CLI release archive and offline exports

On 2026-11-19, the unmodified `crab/scripts/release/release.sh --target
darwin-aarch64` completed both explicit-target release builds and produced the
actual CLI/FUSE/NFS archive at the same source tree
`b5f50facaabeab1fafea59686f19fde6aeef205fa927910673c7bf7ca8713465`.
The build used this worktree's external target and a dedicated `DIST_DIR`, with
no feature override or publication. Archive SHA-256:
`8ac28aabc0734cf18a3041a5213f6043aadd3f43e560d0d882c03b67de043e62`.
Its checksum file matches; the archive contains exactly `crab`,
`crab-fuse-mount` and the `crab-nfs-mount` → `crab` symlink. Extracted binaries
match producer bytes, report version 2.0.0-alpha.1 and have arm64 architecture.
Dependency inspection verifies that the main CLI is FUSE-independent while the
FUSE helper retains its library dependency. Extracted CLI SHA-256:
`001fee12a146fe273db8f66861055ffdbe89957b7a880c51cf7bd4f1c8318950`.

That extracted CLI, with Node/npm absent from PATH, executes thirteen successful
commands covering two real dataset evaluations, captured 0.1/0.2 scores, a -2.5
loss delta with declared direction/comparable identities, report creation,
Apply and both Results/legacy metrics HTML exports. After eleven source/workspace
files are removed, report export remains byte-identical. Independent reads of
both runs and the frozen report match the captured manifests. Both HTML files
contain the exact bundled runtime. Offline Chrome 157 renders Results with two
charts/four series/two images and metrics with two charts/two series/one image.
Actual metric-byte previews, linked range changes, two linked cursor rules and
the authored metrics tooltip (`step: 0`, `loss: 3.25`) pass, with zero network
attempts, page errors or console errors. Both full-page captures were inspected.

The `results-release-cli/` packet retains the archive/checksums, producer and
terminal process receipts, extracted binaries, scripts, manifests, HTML, browser
observations, screenshots and independently reconciled `proof-summary.json`.
The first Cargo cleanup dry run refused the missing cache marker; only the
verified, unused release build directory for this worktree was subsequently
removed. No marker was added, other targets were untouched and the space guard
did not trigger. Source/runtime, installed Make binaries and all four existing
107-check artifacts remain unchanged; independent verification revalidates that
full evidence at `ad012d1a3689328d4744e613ad82ca8567e465947556efb3f2eddd357b30635c`.

This closes native CLI archive/offline distribution proof for Darwin arm64;
other release targets and hosted CI remain open. The current Desktop Results
CI step selects macOS arm64 and Linux; Windows/Intel-macOS Results acceptance
is not proved by that wiring. Actual provider/managed-service qualification,
existing desktop check failures and the historical retrieval timeout remain
open. Reinspection of the timeout's failed assertion/screenshot and three timed
repetitions found no original operation trace establishing its cause; the
packet records that limitation without a source/deadline change or fix claim.
Results remains inside Data Workflows. Whole Plan 020 remains IN PROGRESS.

### Portable CLI fixtures and complete CI source binding

Results CLI regression fixtures no longer have a suite-wide Unix gate or
hard-coded `/usr/bin/python3`/`true` executables. They resolve one absolute Python
interpreter, preserving the executable-byte witness used by reproduction, and
use typed argv for observation-only stages. File/directory symlink cases select
the native standard-library API; existing Unix permission assertions are kept
under their platform contract while directory bytes/membership remain in the
shared suite for both platforms. The native workflow job provisions Python 3.12 and now selects
the Results CLI suite on Linux, macOS and Windows. Existing POSIX experiment
and metrics fixtures keep their distinct selection. This is portable test/CI
wiring, not retained Windows runtime proof.

The source-binding audit exposed a real omission: `workflow-native.yml` changes
could leave the qualification digest unchanged. The producer now includes it
beside `desktop.yml`. A real Git regression first failed before the fix, then
passed; the final fixture verifies an unstaged edit to a tracked producer.
All six source/evidence verifier tests pass, and that command is in native CI.
The current native-workflow file digest is
`5d1a56808c8328677cd3537d67363b608871a268582dd8292d3d3e6c89475d1a`.
YAML parsing verifies one unconditional Results command across all three runners.

All 22 non-S3 CLI Results regressions pass locally; the two explicitly ignored
real-storage cases are separately executed by fresh full qualification. At
source tree `b3c837c5f5cebb79c2c1dc59845807d2f9ee1abc2c5e89854efe8fd875a162a4`,
the full Darwin-arm64/RustFS producer passes **107 capabilities / 74 required
cases**, plus the separate 10k catalog. Independent reconciliation matches the
pre-run binding to current HEAD/source/runtime and all four actual CLI/package
artifacts. Four native, seven browser and one actual HTTP/browser case each
have exactly one passed attempt, with no skips, unexpected/flaky results or
report errors. Evidence SHA-256:
`c85f057e6663b8db86284a5823cde237374f060bdc1d7fd99fb4724a7dbe1985`.
Canonical and retained evidence bytes match. The source closure now includes
both CI files; `source-files.json` independently reconciles all 3,739 included
file identities to that digest.

Native descriptors start/finish at 104 main and 12 sidecar, with sidecar peak 13;
post-GC renderer heap finishes 692,668 bytes below baseline. All 342 observed
reads finish by the last measured sample; all 367 finish after the later
fault/cancellation cases, with zero active readers. The HTTP process starts at
74 descriptors and finishes at 72, never exceeding 74; its renderer heap ends
198,604 bytes below baseline and all 1,975 reads finish. Both traces contain
twenty measured cycles after five warmups, with zero maximum heap growth.
The 10k catalog returns 200 rows / 32 columns / 648,892 bytes; build/cold/warm
times are 4.41/5.17/6.23 seconds. Maximum-overlay render/zoom/cursor measured
178.1/45.1/8.8 milliseconds. These are fixture observations, not retrieval SLAs.

The packet `results-portable-cli/` retains scoped/full command receipts, negative
and passing source-binding checks, CI selection proof, raw accepted artifacts,
per-file identities and independent `proof-summary.json`. Product code, public
DTOs, stored formats, dependencies, runtime/package bytes and design guidance
did not change. Non-test growth is limited to CI execution and binding its owner;
the remaining changes exercise existing behavior on portable fixtures.

A local Windows MSVC test-target probe first stopped before Rust compilation:
cargo-xwin 0.23 could not find x86_64 headers for `Win11SDK_10.0.26100` after its
four CRT setup attempts. Selecting the advertised SDK `10.0.22621` then passed
`cargo xwin check -p crab --locked --no-default-features --features
simd-accel,tier,watch --test workflow_results --target x86_64-pc-windows-msvc
--xwin-sdk-version 10.0.22621` in 805.778 seconds. The terminal receipt and
`windows-proof-summary.json` bind that compile check to the same current source
tree; the initial SDK failure is retained separately. No native Windows runtime
is claimed. Hosted CI has not run, and the packaged Results step still selects
macOS arm64/Linux. Windows checkpoint
fixtures, packaged process/handle observation and producer paths remain to be
qualified. Other provider/managed-service/release evidence, existing desktop
check failures and the historical retrieval timeout stay open. The prior push/
CI authorization request remains pending; no commit, push or dispatch occurred.
Results remains a Data Workflows tab. Whole Plan 020 remains IN PROGRESS.

### Portable checkpoint and resume acceptance

The real checkpoint/apply/reset/resume fixture now uses a tracked Python script
and typed argv with an absolute interpreter. Its scratch repository lives under
the external target. Removing its Unix gates preserves every prior lineage,
failed-reset rollback and source-preservation assertion; successful resume now
also requires a clean JSON envelope identifying the actual new run.

The Windows audit found control-directory ACL setup inheriting stdout before
the structured result. [Rust's `status` contract](https://doc.rust-lang.org/std/process/struct.Command.html#method.status)
inherits child streams, and [Microsoft documents icacls progress output](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/icacls).
That helper's stdout is now routed to stderr through the stable
[`Stdio` conversion](https://doc.rust-lang.org/std/process/struct.Stdio.html#impl-From%3CStderr%3E-for-Stdio),
preserving its diagnostics and existing failure-status check. A real child-process
probe verifies both streams remain visible on stderr, stdout stays valid JSON
and exit status 7 is preserved; Windows metadata compilation also passes.
The shared run owner covers immediate, queued and reproduced runs;
the Unix permission branch is unchanged. This is a contract-backed correction;
actual Windows execution remains required.

At source tree `d75a7e2a4abe40925c746ea62af06e8741e38d4a7365a146884bfe50a2bc3fc7`,
the actual macOS checkpoint journey passes in 97.379 seconds, and Windows MSVC
checks of both experiment and Results test targets pass in 38.138 seconds with
SDK `10.0.22621`. Native CI selects the exact checkpoint test on Windows while
retaining the full POSIX experiment suite on Unix and Results on every runner.
Parsed CI wiring SHA-256:
`84917f8d227da763f84ca759ebf691750967146390f28773ae9efcdfa09df293`.
Rust formatting and diff checks pass. `results-checkpoint-diagnostics/` retains
source binding, real command receipts, logs, stream-contract and CI-selection proof.

The preceding 107-capability package packet remains bound to its earlier source.
The final current-source native package build passes in 284.145 seconds, with
no disk-guard intervention. Both actual CLI hashes are independently retained
in `packaged-source-binding.json`; the agent, app archive and chart runtime
remain byte-identical to the preceding full proof. Fresh complete acceptance
passes in 502.671 seconds: **107 capabilities / 74 required cases**, plus the
separate 10k catalog. Independent verification reconciles all 3,739 source-file
identities, current HEAD/runtime and all four actual CLI/package artifacts with
the pre-run binding. Four native, seven browser and one actual HTTP/browser case
each have one passed attempt, no skipped/unexpected/flaky results and no report
errors. Retained and canonical evidence bytes match at SHA-256
`73611112754ddb0576c18dbc6e5893a56f5927564e6dc8a85c0c2148e3099f41`.

Both twenty-cycle resource envelopes pass after five warmups. Native descriptors
start/finish at 104 main / 12 sidecar, with a sidecar peak of 13; renderer heap
ends 5,581,580 bytes below baseline. All 342 measured reads finish, and all 367
finish after later cancellation, with zero active readers. HTTP descriptors stay
at 71; renderer heap ends 256,032 bytes below baseline and all 1,972 measured
reads finish. Neither trace exceeds its baseline heap. The 10k catalog returns
200 rows / 32 columns / 648,892 bytes; build/cold/warm times are 4.52/4.95/4.97
seconds. Maximum-overlay render/zoom/cursor takes 178.6/45.1/8.1 milliseconds.
These are fixture observations, not retrieval SLAs.

The independent source delta contains exactly the native workflow producer,
checkpoint helper and experiment fixture. Results models, formats, dependencies,
UI and runtime assets are unchanged; no new visual verdict is claimed. The
packet retains accepted raw artifacts, source identities, current summaries and
independent verification. Earlier compilation-cache reclamation touched only
this worktree's unused completed explicit-target cache after an empty open-file
check; retained release archive and executable hashes were unchanged.

Windows packaged process/handle
observation and producer paths, native runtime, other provider/platform and
hosted CI/release gates remain open. UI navigation and design guidance are
unchanged: Results stays under Data Workflows. No commit, push or CI dispatch
occurred; the existing authorization request remains pending.

Read-only GitHub repository-level secret/variable name queries against the
explicit `origin` repository, `crabbuild/crab`, succeeded, but none of the SDK
live-provider fixture names are repository-defined. Organization
and environment inheritance remain unverified; this does not prove those
credentials unavailable. `live-fixture-metadata-explicit-origin.json` records
names and presence only. No live cloud fixture was selected or exercised.

### Windows native qualification harness

The native desktop profile requires all 77 core/fault/desktop capabilities and
64 named cases, plus the separate 10k catalog. Desktop CI now selects that
existing `--desktop` profile on Windows, including main/sidecar OS-handle
observations. It retains the complete 107-capability HTTP/browser profiles on
macOS arm64 and Linux;
this does not claim a Windows HTTP-server distribution. Preserve every resource
budget, case and failure assertion. The producer reuses the existing Windows
RustFS layout and Python AWS CLI owner, native `.exe` names and Node entrypoints.
Package binding selects the current platform and architecture, refusing a stale
foreign package. Real-file regressions caught the former foreign/Intel-Mac
misbinding before the fix; all eight harness/evidence tests now pass.

Windows observation uses PowerShell 5.1's local CIM process identities and
[`Process.HandleCount`](https://learn.microsoft.com/en-us/dotnet/api/system.diagnostics.process.handlecount?view=netframework-4.8.1),
with fresh measurements and parent-PID/canonical-executable matching. Each
resource sample rechecks the actor identity. The existing `fd_counts` envelope
contains POSIX descriptors on Unix and OS handles on Windows, compared only
against each process's own warmed baseline. UTF-16LE encoded commands and UTF-8
JSON preserve paths; the observer hides its console and rejects malformed,
missing, nonpositive or fractional counts. A nine-query mocked boundary probe
passes; it is not native Windows execution.

The real macOS checkpoint symlink-boundary case passes. Windows MSVC checks with
`--no-default-features --lib --tests` pass with 101 test artifacts, including the
library tests, and zero compiler errors. The required boundary case now selects
the native Windows symlink API. All 22 regular CLI Results cases pass; two real
storage cases remain separately selected by full qualification. Focused native
test types, Rust formatting, TypeScript formatting and diff checks pass. YAML
parsing and actual Bash branch evaluation preserve Windows's 77/64 profile and
the full macOS/Linux profiles. Desktop workflow SHA-256:
`fa8cc06dfdcc31dde09c0a3cabb2fa8d0effb2440424cf5673c9864ca4d2b0f6`.

The current source is
`859b9b135a351865b3527d97a51eb198291c22338e718962f482a0b9932bec4b`.
The compiled CLI/app/runtime bytes are unchanged after the successful package
rebuild. `results-windows-harness/` retains the preceding full 107/74 proof;
`results-windows-harness-console/` binds the final observer correction and its
completed full acceptance run. Its producer exits zero in 437.389 seconds, and
independent verification passes all 107 capabilities / 74 required cases plus
the separate 10k catalog. Four native, seven browser and one HTTP/browser cases
each pass on their only attempt, with no skipped, unexpected or flaky results.
The verifier independently reconciles all 3,739 source identities, the bundled
runtime and four actual CLI/application artifacts. Evidence SHA-256:
`e7e8220079c2d3ca70bcc515f93a8fe7fbb8edcae4a8a0450711389663cd9a90`.
Neither packet claims native Windows execution.

Both twenty-cycle resource envelopes pass after five warmups. Native descriptor
counts return to 104 main / 12 sidecar, and all 366 reads finish after
cancellation. HTTP descriptors return to 73, and all 1,972 reads finish. Neither
client shows measured heap growth above its warmed baseline. The native journey
queues two real runs, compares actual metrics/plots, reopens the application and
recovers the comparison after remote retrieval. It explicitly rejects a
standalone Results activity-bar button and enters Data Workflows → Results,
including a bare remote repository with no workflow YAML.

Intel Mac fixture qualification has a concrete dependency gap: the pinned
[RustFS 1.0.3 release](https://github.com/rustfs/rustfs/releases/tag/1.0.3)
publishes only macOS arm64 assets, and the verified local executable is arm64,
not universal. The complete asset list and executable inspection are retained.
No different fixture/version was substituted. An authorized external fixture or
another qualified native fixture is required for that platform.

The preceding `d75a7e2a…` package acceptance remains its retained proof. Native
Windows execution still requires CI. No Windows pass, live-provider qualification
or release readiness is claimed here. Publication approval and live-fixture
identifiers remain pending; Results stays under Data Workflows.

### Live-provider Results contracts

The SDK's existing credentialed S3/GCS/Azure jobs previously tested Git content,
without a Results case. They now select explicit Results-only cases using the
same dedicated fixture credentials. The shared test publishes a sealed scalar/
CSV observation through `RemoteResults`, retries publication, removes source
state, cold-pulls verified bytes and then recovers the report after removing run
discovery. A fresh SDK client discovers the report and reads its frozen body and
blob, with unchanged repository object inventory. Cleanup deletes only objects
inside this test's unique repository prefix; shared bucket/container state is
not cleared. This is transport/SDK proof, not a replacement for actual two-run
packaged acceptance.

The Results-only SDK test passes; all three credentialed cases compile and stay
explicitly ignored outside their selected jobs. The S3 case also passes against
owned RustFS, one selected test with zero ignored/failed tests. Its retained SDK
verifier checks the fixture digest and log hashes. It is not live AWS evidence.
YAML parsing and actual Bash branches with a stub qualifier select all three
named cases, Results-only features, one required test and the actual fixture
source digest. The new SDK workflow is included in Results source binding;
the eight harness/evidence tests cover changes to all three workflow producers.

Current source tree:
`abe0a9f5c81128e13055ebf72d84902635ca929d3e36e6b0e0e2b9eeddd66792`.
Only the SDK Results test, its CI workflow and qualification source-binding
code/test changed. All compiled product sources and four actual CLI/application
artifacts remain unchanged. Scoped receipts and source deltas are retained in
`results-provider-contracts/` under the external target. Refreshed full acceptance
exits zero in 461.764 seconds; independent verification passes 107 capabilities /
74 required cases plus the separate 10k catalog. It reconciles all 3,740 source
identities, including the newly bound SDK workflow, and the runtime/four actual
artifacts. All twelve native/browser/HTTP-browser cases pass on their only
attempt, with no skipped, unexpected or flaky results. Evidence SHA-256:
`74cac8fe96f2ef6b480d61102484455205ccee921d5ea9896f7f00f4096b713b`.

Both twenty-cycle resource envelopes pass after five warmups. Native descriptor
counts return to 104 main / 12 sidecar; all 366 reads finish after cancellation.
HTTP descriptors finish at 70 against a 71 baseline; all 1,974 reads finish.
Neither renderer grows above its warmed heap baseline. The 10k catalog returns
200 rows / 32 columns in 648,892 bytes; debug cold/warm queries take 4.910/4.928
seconds. Maximum-overlay render/zoom/cursor observations are 198.9/47.2/8.6 ms,
not latency guarantees. No dependency, schema, runtime asset or product UI
changed. Results remains under Data Workflows. Live AWS/GCS/Azure,
managed-service, native-platform and hosted CI/release qualification remain open;
pending fixture/publication requests are unchanged.

### SDK documentation and updated crab-v2 integration

The SDK README, quickstart and public API reference now expose the existing
`results` feature and namespace, immutable run/report identities, operation
controls and provider qualification limits. The exact MDX Rust example passes
one doctest against the Results-only SDK build. Web types, ten tests, production
build and link checks pass; lint exits zero with sixteen warnings in untouched
sources. The crawler checks 399 HTML pages and 4,309 fragments. Fresh local
production HTTP responses verify the Results section, anchor, CLI guide links
and Data Workflows → Results text. Source/page hashes and actual check receipts
are retained in `results-sdk-docs/`. This documentation proof does not publish
the site or claim another runtime acceptance run.

A fresh fetch finds `origin/crab-v2` at
`464d943a045638fcb33746c94f473c284902cc72`, one commit ahead of the prior base.
The branch fast-forwards and the tracked Results changes reapply without
conflicts; untracked work stays in place. Independent hashes verify preservation
outside the twelve upstream paths, and the repository page equals an independent
three-way merge. The backup stash is retained and identified in
`results-upstream-ui/stash.json`. Updated browser types, formatting and build pass.

The repository browser suite initially passes 44 cases and fails two. A clean
upstream worktree using the same installed dependencies passes its navigation
case and reproduces the SQL editor's `box-shadow: none` failure exactly.
`packages/ui/src/styles/globals.css:4602` sets an outline and removes that shadow
inside the upstream file workspace; Results does not enable that workspace class.
The unrelated SQL assertion is preserved, with both traces and clean-source
control retained. The Results navigation expectation now requires all seven
links and the Results URL, retaining the existing six links and path/history
journey. That corrected case passes once; a zero-case anchored selection was
rejected and retained separately, not accepted as proof. No whole-repository
suite pass is claimed.

Current source tree:
`2ab4b42a46ce8529b6e163ad77c7df6642c4dc80d545aa777b35b11cddbb4b8a`.
The eleven changed code paths belong to the upstream browser integration,
including the strengthened navigation test. CLI build metadata tracks Git HEAD,
so the dedicated pipeline rebuilds the debug CLI and official native package
before refreshing full Results acceptance. Source, commands, current process
receipts and scoped evidence are retained in `results-upstream-ui/`. The
preceding 107/74 packet keeps its original source/artifact scope; the new full
proof is in progress. Cloud/native CI, release and existing baseline check gaps
remain open. Results remains under Data Workflows, without a native sidebar entry.

### Schema discovery and canonical test artifacts

The refreshed producer passes its full 107/74 journey, but the external
pre-run artifact verifier rejects that packet: the initial plain `cargo build`
CLI digest differs from the CLI rebuilt by the integration-test target.
Packaged CLI/agent/app bytes and source remain stable. Both CLI versions retain
the same Git SHA and build timestamp; the test build recompiles the CLI and
workflow dependency graph. Preserve the rejected pre-binding rather than
rebinding it after execution. The next dedicated pipeline must compile the
exact integration-test target with `cargo test --test workflow_results --no-run`
before freezing its binary digest. The Cargo test target and `CARGO_BIN_EXE`
contract are documented in the
[Cargo test reference](https://doc.rust-lang.org/cargo/commands/cargo-test.html).

A separate real CLI probe finds `crab results list --json` emits
`workflow.results` version `1.0`, while that same binary's `version --json`
registry omits the schema. The public version-info guide documents that registry
as automation's supported-schema preflight. Extend the existing version owner,
add a real CLI contract regression and require it in qualification. This is an
additive discovery correction for Results, not a stored-model/schema migration.
The focused and whole-schema generator entry points already share the correct
Results payload type and remain unchanged. Raw probe, producer and rejected
binding evidence are retained in `results-upstream-ui/`; no full accepted packet
is claimed for this binding yet.

The real CLI regression fails before the correction (`None` versus the actual
`1.0` envelope version), then passes after the Results registry entry is added.
All 23 regular Results cases and six version unit tests pass. The registry keeps
its existing entries and adds Results; stored/wire model versions are unchanged.
Nine qualification tests pass, including rejection of an absent discovery check
or a zero-case selection. SDK reads and the owned-RustFS transfer pass. Web
types/lint/ten tests, fresh production build and link checks also pass.

Qualification now requires 108 capabilities / 75 cases, plus the separate 10k
catalog. The native-only profile requires 78 / 65. The dedicated pipeline builds
and executes the integration-test target before freezing its CLI hash, preserving
the canonical Cargo test dependency graph. A separate pre-freeze CLI probe
confirms `workflow.results: 1.0` and records that exact executable digest.
Current source tree:
`7a7912b09dc27706ca203e8693bbf5bf375e397f2db3d1723930119e46b17003`.
Only the version owner, transport regression and qualification source/tests
change from the preceding integration source. Source, positive/negative checks
and command receipts are retained in `results-schema-discovery/` under the
external target. The rebuilt package passes, but full acceptance fails at a
native resource sample: a read starts after the preceding idle observation.
That packet remains failed; the earlier rejected pre-binding remains separate.
Results stays under Data Workflows.

### Stable idle resource sampling

Bounded fixture tracing reproduces the native failure on the third focused
run: a bucket-list GET starts during heap/process collection, after the prior
idle observation. Synchronous OS queries also block the Node event loop that
owns the fixture proxy. Preserve both failed packets and the credential-free
request trace in `results-schema-discovery/reader-failure/` and
`results-reader-diagnostics/`; passing reruns do not replace that evidence.

Native process commands now yield asynchronously. Both native and HTTP lifecycle
checks require zero active readers and unchanged started counters across the
whole heap/descriptor measurement, using the existing bounded assertion poll.
The [Node 22.20 child-process contract](https://nodejs.org/download/release/v22.20.0/docs/api/child_process.html)
confirms synchronous queries block that shared event loop; the existing
[Playwright poll](https://playwright.dev/docs/test-assertions#expectpoll) retains
its default five-second deadline.
Resource assertions run after sampling and remain unchanged: five warmups,
twenty measured cycles, 16 MiB heap growth and eight additional handles per
owner. A continuously active reader still fails the idle gate. Temporary request
tracing is removed from the fixture after retaining its diagnostic evidence.
This corrects measurement ordering; runtime/product sources are unchanged.

Current source tree:
`8ca8af9952b45a3b219b552fce8ebb21c81dc7bb5de0445483dd0601825c7575`.
Only the two lifecycle test sources differ from the preceding package build;
all four actual CLI/agent/application digests match their pre-run identities.
Focused types, formatting, qualification tests and the nine Windows query
boundary refusal cases pass. Two focused packaged repetitions pass with all
21 idle samples and unchanged main/agent descriptor counts. The third times
out at 360 seconds, followed by worker teardown timeout; it produces no lifecycle
sample. Do not infer the blocked operation from its last screenshot. Retain
this failed repetition and the separate orchestrator wait timeout in
`results-idle-sampling/`. Three subsequent full traced journeys and ten shortened
first-close checks pass. API/process diagnostics use `results-close-diagnostics/`
and `results-close-minimized/`; the timeout's cause remains unresolved.

The refreshed producer then fails before client checks when the real HTTP
fixture cannot reserve local staging capacity. The active owner requires a
2 GiB initial Git receive reservation plus at least 1 GiB disk reserve:
`crates/crab-server/src/git/receive/mod.rs:25` and
`crates/crab-server/src/server/capacity.rs:33`. Free space was below 3 GiB.
Preserve that failed packet in `results-idle-sampling/failed-http-capacity/`.
Reclaim only this checkout's unused Windows and release compiler caches; no
open files reference them. All four bound artifacts and the installed CLI
remain outside the removed caches. Exact real-storage HTTP qualification passes
after reclamation. Product quotas, contracts and tests are unchanged.

A fresh producer and independent verifier pass in `results-capacity-recovered/`:
108 required capabilities / 75 required cases, plus the separate 10,000-run
catalog. Producer 478.109 seconds; independent verification 5.085 seconds. All
twelve native/browser/HTTP-browser cases pass on their only attempt, with zero
skipped, unexpected or flaky results. The verifier matches all 3,741 source
identities, the bundled runtime and all four actual artifacts. Evidence SHA-256:
`c99e85930d4e2209fe6b918eee6d13675ff00059f4a0b7145461f6e54faa417b`.

Both envelopes retain five warmups and twenty measured cycles. Native handles
stay at 104 main / 12 agent; cancellation ends at 367/367 completed readers.
HTTP handles move from 71 to 75, within the unchanged eight-handle budget, and
end with 1,963/1,963 readers completed. Neither envelope grows post-GC heap above
its warmed baseline. The debug catalog returns 200 rows / 32 columns in 648,892
bytes: cold query 6.081 seconds, warm 4.767 seconds. Chart render/zoom/cursor
observations are 167.6/43.3/7.8 ms; these observations do not establish a remote
catalog latency SLA.

The 4 GiB host preflight and 3 GiB execution guard remain active. Source stays
`8ca8af99…`; no package rebuild is needed for the two lifecycle test edits. The
fresh passing packet explicitly records whole-plan completion as false and
retains the earlier untraced native timeout as unresolved.
Native Windows and actual cloud/managed-provider execution remain unverified.
No push or CI dispatch.

### PR delivery and timeout investigation

On 2026-11-19 the user authorizes committing, pushing, opening a PR and running
the existing CI workflows. Keep the PR targeted at `crab-v2` and preserve Results
inside Data Workflows. Exclude local review screenshots and Python caches.

Staging exposes trailing whitespace in the new generated TypeScript declarations.
Normalize line endings at the Results Rust generator, regenerate through its
explicit export test and run the owner/consumer drift check. The generated change
removes trailing whitespace only; both client typechecks pass. The preceding
108/75 packet retains its original `8ca8af99…` identity; CI must qualify the
published head. The prior native timeout remains unresolved pending investigation.

## STOP conditions

Stop the affected slice and report the concrete issue if:

- Persistence/auth/cleanup source differs from the evidence map without a
  corresponding reviewed plan update.
- Existing-format changes lack release-contract proof; a Results namespace
  cannot traverse a supported managed transport; SDK minimal/public ownership
  would require unrelated redesign.
- Offline assets require patched/vendor source, hidden build fallback/runtime
  install, or undeclared network access.
- Lifetime proof uses only test GC helpers or unprotected shared-data closures.
  Do not broaden GC silently.
- Reproduction would run current HEAD/files or unverified live dependencies
  while claiming exactness. Return unavailable until resolved.
- Passing requires zeroing missing values, hiding errors as empty lists,
  substituting current content or retrying denied access with another identity.
- The mounted volume/dependency/native package is unavailable; a meaningful
  verification fails twice after reasonable correction; passing requires
  changing baseline/inventory/ignore/expected-failure files.

Independent slices may proceed with the limitation recorded, but the complete
workstream remains open until every required gate passes.

## Maintenance notes

Review schema, identity, publication order and lifetime together for every new
attachment/report field. Display transforms cannot mutate frozen reports.
Distinguish live names from frozen labels, observed evidence from inferred
semantics, and byte restoration from numerical reproducibility.

Future logger/evaluation/optimizer/tracker features consume this Results owner;
they must not introduce another catalog authority, parser or provider client.
Automatic retention and semantic evaluation stay separate workstreams.
