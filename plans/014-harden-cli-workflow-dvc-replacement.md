# Plan 014: Harden Crab CLI workflows into a safe DVC replacement

> **Executor instructions**: This is a roadmap, not one pull request. Execute
> each numbered phase as one or more independently reviewable pull requests.
> Run every phase gate before starting a dependent phase. Never claim DVC
> replacement parity, default-enable workflows, or suggest deleting `.dvc/`
> before the corresponding gate in this plan passes. Crab Desktop is excluded
> from every phase.
>
> **Drift check (run first)**:
>
> ```bash
> git diff --stat 9c39e6e8..HEAD -- \
>   crab/src/main.rs crab/src/cmd crab/src/core/config.rs crab/tests \
>   crab-web/content/docs crab/docs \
>   crates/crab-workflow Cargo.toml Cargo.lock \
>   .github/workflows crab/scripts/e2e
> ```
>
> Re-read `AGENTS.md`, `crates/AGENTS.md`, and the nearest README or repository
> workflow guidance at execution time. The current repository-local workflow
> guidance is in `.codex/skills/crab-workflow/SKILL.md` and CLI-contract guidance
> is in `.codex/skills/crab-cli-core/SKILL.md`; use both when their surfaces are
> touched. If an in-scope surface or rule has changed, reconcile this evidence
> map against current code before editing. A semantic mismatch is a STOP condition.

## Status

- **Priority**: P0–P2 roadmap; execute in the order below
- **Effort**: XL, approximately 14–24 engineer-weeks plus provider qualification
- **Risk**: HIGH
- **Depends on**: none; plan 013 is independent
- **Category**: correctness / migration / architecture / performance / release / tests / docs
- **Planned at**: commit `9c39e6e8`, 2026-08-16

The planning checkout was dirty with unrelated work in `Cargo.toml`,
`Cargo.lock`, `crates/crab-git/`, `crates/crab-metadata/`, `crates/crab-read/`,
`crates/crab-remote-git/`, and `crates/crab-storage/`, plus unrelated untracked
directories. Preserve that work. Execute each phase in a clean dedicated
worktree or coordinate explicitly before touching an overlapping contract.

### Execution status in this checkout (2026-08-17)

This checkout contains the local contract work for G0 and portions of G1, G3,
G5, and G6, including fail-closed cache/artifact integrity hardening, but it is
not a release-qualified completion of this roadmap. The
following evidence is intentionally separated from the phase gates:

| Gate | Local state | Remaining proof/blocker |
|------|-------------|------------------------|
| G0 | Checkpoint/artifact semantic loss is rejected or preserved; focused migration, YAML, and dispatch tests exist. `cargo test -p crab --lib cmd::migrate::tests --locked` (16), `cargo test -p crab-workflow dvc_inventory --locked` (15), and `cargo test -p crab-workflow artifact --locked` (16) pass locally. Unfinished history-rewrite commands now fail explicitly instead of returning success. | Run the complete migration fixture matrix and CI evidence audit |
| G1 | One native shell adapter, platform-aware hashes, and native workflow jobs are authored. `cargo check -p crab --locked` and `cargo clippy -p crab-workflow --all-targets --no-deps --locked -- -D warnings` pass locally. | Native release-binary execution and descendant-cleanup evidence on all three OSes |
| G2 | Inventory, redacted journal, source precedence, atomic publication, and rollback scaffolding exist; pointer-tree publication now has a durable swap marker and restart recovery; local reconstruction is explicitly not treated as clean-clone proof. Migration tests cover cache-only, redaction, source mutation, directory mode, publication rollback, and interrupted pointer swaps. | Live remote transfer plus fresh Crab clone/hydrate byte/mode verification; the current command keeps `dvc_remote_clean_clone_unverified` blocking until that verifier exists |
| G3 | Authenticated checkpoint protocol, lineage, apply/reset/resume, local transport, checkpoint-aware local live-set validation, and conservative remote workflow root registration exist. Local cache hits now remain at `LockfileUpdated` until callers finish atomic output materialization, and that state resumes through the cache probe instead of being treated as terminal. `cargo test -p crab --test workflow_exp --locked` (36), checkpoint command tests (5), workflow checkpoint tests (8), workflow GC tests (7), and the cache-hit/materialization resume tests pass locally, including staged-reset publication failure rollback and checkpoint push/pull round-trip coverage. | Three-checkpoint crash/resume, push/pull, and GC E2E evidence against the release remote; directory-swap crash recovery is still unproven |
| G4 | RustFS smoke/verifier and native release-gate workflows are authored; workflow default is enabled with explicit opt-out. YAML parses locally and the smoke verifier is present. | An actual exact-SHA/run/attempt release run with retained Linux/macOS/Windows/RustFS artifacts |
| G5 | Bounded streaming/hash-index paths and honest unsupported-provider preflight exist; artifact declarations now have a primary-remote manifest/payload/stage/history boundary with CAS promotion, verified file/directory downloads, and GC reachability protection for refs, manifests, payloads, and history. Reachability now fails closed when a referenced payload object is missing. Untrusted stage cache manifests are validated for repository-safe paths, canonical hashes, tree metadata, duplicate paths, and byte sizes before materialization. Artifact, cache-validator, and materialization suites pass, and a RustFS file-artifact create/promote/list/history/get run passed. | Live SSH/SFTP/WebDAV/HDFS/Drive/OSS qualification remains outstanding |
| G6 | `crab data` contracts and local/file/HTTP/object-store transactional paths exist; `import-url` and `update` stream object-store bodies through temp files with size/validator verification, and the bundled read-only SQLite connector materializes deterministic JSONL transactionally. Data unit tests (13), including object-store streaming, and the SQLite import test pass locally. | Level-3 repository/remote and broader database connector E2E; unsupported connectors must remain documented as unsupported |

Until every row has the required external evidence, `plans/README.md` must
remain `IN PROGRESS` and no documentation may recommend deleting `.dvc/` or
claim general DVC replacement parity.

## Why this matters

The current workflow surface can execute useful DAGs, but its migration and
release claims are unsafe: checkpoint meaning is lost, artifacts disappear,
large external inputs are buffered, and Windows/release/provider paths lack
live proof. This roadmap makes every unsupported construct fail closed,
preserves source state until byte-identical verification succeeds, and adds
one tested ownership path for execution, migration, artifacts, providers, and
data commands. It deliberately excludes Crab Desktop and does not expand into
a hosted service.

## Outcome

Crab CLI becomes a safe, explicit, cross-platform workflow and data-management
alternative for the supported DVC contract. The result must have:

1. A resumable DVC migration protocol that inventories and verifies `.dvc`
   files, caches, remotes, materialized data, `dvc.lock`, and run/checkpoint
   state before declaring cutover safe.
2. Real experiment checkpoint lineage, apply, resume, reset, transport, and GC
   semantics. `checkpoint: true` is never silently treated as `persist: true`.
3. Native Unix and Windows command execution with native CI proof.
4. First-class artifact declarations and an immutable version/promotion
   lifecycle with `list`, `show`, `get`, versioning, and promotion commands.
5. Bounded-memory external dependency hashing and validator-aware reuse that
   avoids downloading unchanged large objects.
6. Live, capability-advertised remote providers for SSH/SFTP, WebDAV,
   HDFS/WebHDFS, Google Drive, and Aliyun OSS.
7. A release gate that runs workflows on Linux, macOS, and Windows and retains
   RustFS command-level evidence for the exact release commit.
8. The existing `crab get` equivalent plus a coherent `crab data` surface for
   list/import/import-url/import-db, update, and data status, integrated with
   the artifact registry.

This plan does not require identical DVC command spelling or support every DVC
flag. It requires honest, documented Crab semantics and fail-closed behavior
where parity is absent.

## Product safety gates

These gates are cumulative:

| Gate | Meaning | User-facing claim allowed after it passes |
|------|---------|-------------------------------------------|
| G0 | Silent semantic loss is removed | “Unsupported DVC constructs are detected before migration.” |
| G1 | Windows execution is native and tested | “Workflow stages execute on supported Unix and Windows releases.” |
| G2 | Migration verifies data, cache, remotes, and lock state | “Supported non-checkpoint DVC projects can be migrated safely.” |
| G3 | Checkpoint lineage and resume pass E2E | “Supported legacy DVC checkpoint projects can be migrated safely.” |
| G4 | Native CI and RustFS release evidence are mandatory | “Crab workflows are release-qualified and enabled by default.” |
| G5 | Artifacts, external data, and provider matrix pass live tests | “Crab covers the documented artifact and remote workflows.” |
| G6 | Data ecosystem commands pass end-to-end tests | “Crab is a general replacement for the documented DVC profile.” |

No phase may weaken an earlier gate to make a later phase pass.

## User-priority coverage

| User priority | Planned owner | Required proof |
|---------------|---------------|----------------|
| P0 safe DVC migration | Phases 0, 2, and 3 | Inventory of `.dvc` files/cache/remotes/data/locks, resumable transfer journal, byte-identical clean clone, `safe_to_remove_dvc` only after all records are accounted for |
| P0 checkpoint semantics | Phases 0 and 3 | Acknowledged parent-linked checkpoints, apply/resume/reset, push/pull, crash recovery, metrics association, and GC E2E |
| P0 Windows execution | Phase 1, release qualification in Phase 4 | Native Windows argv/shell/list/hook/timeout/process-tree tests on the release binary |
| P1 artifacts/models | Phase 5 | `artifacts list/show/get`, immutable versions, CAS promotion/history, clean-clone get, and GC reachability |
| P1 external dependency performance | Phase 6A | Streaming bounded memory, validator reuse, changed-object transfer counters, and peak-memory benchmark |
| P1 remote compatibility | Phase 6B | Capability matrix plus live service evidence for SSH/SFTP, WebDAV, HDFS/WebHDFS, Drive, and OSS |
| P1 release qualification | Phase 4 | Native OS matrix, RustFS smoke, exact SHA/run/attempt evidence, and mandatory release dependency |
| P2 CLI ecosystem | Phase 7 | Existing `crab get` plus data list/import/import-url/import-db/update/status E2E and rollback proof |

## Baseline state and evidence map

The following map records behavior at the planning commit above. It is retained
for traceability; the execution-status table is the authoritative description
of what has changed in this checkout and what still lacks gate evidence.

| Surface | Current behavior | Evidence |
|---------|------------------|----------|
| DVC migration entry | Reads one `dvc.yaml`, converts it, and writes `crab.yaml`; no `.dvc` file, cache, config, data, or lock inventory | `crab/src/cmd/migrate.rs:212`, `crab/src/cmd/migrate.rs:217` |
| Checkpoint conversion | Reads `checkpoint`, drops the field, and writes `persist: true` without a warning | `crates/crab-workflow/src/dvc_migration.rs:435`, `crates/crab-workflow/src/dvc_migration.rs:449` |
| Checkpoint CLI | `stage add --checkpoints` writes ordinary persistent cached outputs | `crab/src/cmd/stage/add.rs:53`, `crab/src/cmd/stage/add.rs:172` |
| Checkpoint model | `Out` has `persist` but no checkpoint identity or lifecycle | `crates/crab-workflow/src/stage_out.rs:9` |
| Experiment snapshot | A successful experiment captures one terminal working-tree snapshot | `crab/src/cmd/exp.rs:1442`, `crab/src/cmd/exp.rs:1448` |
| Shell execution | `Cmd::Shell` and each shell-list entry execute through `/bin/sh -c`; empty argv also calls the Unix shell | `crates/crab-workflow/src/executor.rs:936`, `crates/crab-workflow/src/executor.rs:995` |
| Artifact parsing | Top-level `artifacts` is deserialized and intentionally discarded; `Workflow` has no artifact field | `crates/crab-workflow/src/yaml.rs:53`, `crates/crab-workflow/src/workflow_doc.rs:8` |
| External dependency hashing | HTTP streams the whole response; object-store objects and every prefix member use `.bytes()` and are fully buffered | `crates/crab-workflow/src/stage_runtime.rs:192`, `crates/crab-workflow/src/stage_runtime.rs:303`, `crates/crab-workflow/src/executor.rs:1338` |
| Remote schemes | Parsers accept SSH/SFTP/HDFS/WebHDFS, but runtime rejects them; WebDAV, Drive, and OSS are not modeled | `crates/crab-workflow/src/stage_dep.rs:41`, `crates/crab-workflow/src/stage_runtime.rs:192`, `crates/crab-workflow/src/executor.rs:1290` |
| Workflow rollout | `[workflow] enabled` defaults to false and command boundaries gate on it | `crab/src/core/config.rs:5555`, `crab/src/cmd/run.rs:467`, `crab/src/cmd/workflow.rs:64` |
| CI | The general Rust workflow runs only on Ubuntu | `.github/workflows/rust.yml:12` |
| Windows release | Windows binaries are cross-built on macOS, not executed on Windows | `.github/workflows/release.yml:527`, `.github/workflows/release.yml:552` |
| Workflow smoke | A substantial RustFS command-level workflow smoke exists but no GitHub workflow invokes it; it currently writes a local report and explicitly enables workflow | `crab/scripts/e2e/run_dvc_workflow_smoke.py:1`, `crab/scripts/e2e/run_dvc_workflow_smoke.py:104`, `crab/scripts/e2e/run_dvc_workflow_smoke.py:226` |
| Public docs | Fumadocs source is under `crab-web/content/docs`; older CLI/design Markdown remains under `crab/docs` and must not contradict it | `AGENTS.md`, `crab-web/content/docs/cli/guides/migrating-from-dvc.mdx:1`, `crab-web/content/docs/cli/workflow/dvc-migration.mdx:1` |
| Existing get equivalent | `crab download`, visible as `get`, already reads selected files without cloning | `crab/src/main.rs:207` |
| Existing import/update names | Top-level `import` means raw object-store prefix ingestion; top-level `update` updates the Crab binary | `crab/src/main.rs:894`, `crab/src/cmd/update.rs:69` |

### Current excerpts that must disappear or change

```rust
// crates/crab-workflow/src/dvc_migration.rs:449
"checkpoint" => {
    checkpoint_persist = sv.as_bool().unwrap_or(false);
}
// ...
if checkpoint_persist {
    filtered.insert(Value::String("persist".to_owned()), Value::Bool(true));
}
```

```rust
// crates/crab-workflow/src/executor.rs:995
let args = vec!["-c".to_owned(), shell.to_owned()];
let mut command = Command::new("/bin/sh");
```

```rust
// crates/crab-workflow/src/yaml.rs:53
let raw: RawWorkflow = serde_yaml::from_str(text)?;
let _ = &raw.artifacts;
```

```rust
// crates/crab-workflow/src/stage_runtime.rs:307
let bytes = result.bytes().await?;
// The prefix path repeats get(...).bytes() for every object.
```

### Contracts and vocabulary to preserve

- Use `worktree`, `working tree`, `pointer file`, `hydration`, `dehydration`,
  `prefetch`, `worktree identity`, and `per-worktree state` as defined in
  `CONTEXT.md:5`. Do not call a working tree a workspace in new CLI text.
- `crates/crab-workflow` owns workflow documents, graph/stage semantics, cache,
  experiments, queues, resume, lockfiles, templates, and DVC migration.
  `crab-storage` owns provider-neutral object-store transport and identity;
  `crab` owns CLI parsing, repository discovery, credentials, and product
  composition. Do not put reusable migration or provider policy in commands.
- Follow the CLI contract: parse args before opening repositories or credentials,
  resolve `OutputMode` once, keep `--json` as one stdout envelope and JSONL as
  one event per line, preserve typed source errors, and make cancellation close
  journals/locks/SlateDB and clean temporary state.
- Every new or changed command variant must update the `crab/src/main.rs`
  `output_mode` and `schema_name` matches, add/update its `crab/schemas/*.json`
  contract, register stable error codes where user-visible, and add one public
  dispatch test. Never emit logs onto a JSON/JSONL stdout stream.
- Serialized workflow, lockfile, experiment, migration journal, artifact
  manifest, and JSON/JSONL shapes are cross-version contracts. Version them,
  reject newer schemas, and test deterministic serialization.
- Keep workflow-private state in `crab-workflow`; if a checkpoint record,
  artifact manifest, or source descriptor crosses into `crab-types`,
  `crab-metadata`, `crab-storage`, or the CLI, define one shared versioned
  contract there and have every consumer use it. Do not duplicate look-alike
  serializers in commands or provider adapters.
- Preserve the existing strengths: parallel DAG execution; retry, timeout,
  keep-going, crash recovery; cache-miss explanations and cache-only runs;
  content-defined chunk deduplication; one Git/large-data remote; random-access
  SDK/fsspec reads and prefetch; experiment/queue/Hydra/metrics/plots commands;
  and structured JSON/JSONL output.
- Never use a DVC MD5 as a Crab content hash. Recompute and verify bytes through
  the canonical Crab add/staging/storage path.
- Never print or persist credentials found in `.dvc/config.local`, remote URLs,
  environment variables, OpenSSH configuration, database DSNs, or OAuth state.

## Dependency contracts to verify before implementation

1. DVC project structures:
   - `.dvc` file and output metadata: <https://dvc.org/doc/user-guide/project-structure/dvc-files>
   - `.dvc/config`, cache, run cache, lock files, and `.dir` objects:
     <https://dvc.org/doc/user-guide/project-structure/internal-files>
   - configuration precedence and external cache directories:
     <https://dvc.org/doc/user-guide/project-structure/configuration>
2. Legacy checkpoint behavior must be specified from a versioned DVC 2.x
   source or fixture (use the pinned DVC 2.x behavior represented by the
   official <https://github.com/iterative/checkpoints-tutorial> fixture and its
   matching command behavior as the starting reference). Current DVC 3
   removed checkpoints; do not imply current DVC supports them. Record the
   exact source/version and fixture digest in the design document.
3. `object_store` 0.14.1 exposes `ObjectMeta` with size/e-tag/version,
   `GetOptions` conditional/range/head fields, and a streaming `GetResult`.
   Verify these types in the locked dependency source before relying on them.
   An ETag is a validator, never a Crab content hash.
4. Before adding SSH, WebDAV, HDFS, Drive, or OSS crates, read upstream source,
   types, authentication behavior, maintenance status, license, native-library
   requirements, and security advisories. Dependency patches, overrides, or
   vendoring require explicit approval.

## Architecture decisions

### One migration protocol, not a YAML converter plus ad hoc scripts

`crab migrate from-dvc` becomes a five-state protocol:

```text
inventory -> preflight -> transfer -> verify -> cutover report
                 |            |          |
                 +------ resumable journal +------+
```

The durable journal records source identities, decisions, completed transfers,
computed Crab hashes, and verification results. It contains no secrets. Running
the command again resumes or proves idempotence. Crab never deletes `.dvc/`.

The cutover report has `safe_to_remove_dvc: true` only when every discovered
tracked output, directory manifest, required cache object, configured remote,
lock entry, import provenance record, and checkpoint/run-cache record is either
successfully represented in Crab or explicitly classified as irrelevant with a
machine-readable reason. Any unknown or unsupported record forces false.

### One checkpoint lineage owned by experiments

`checkpoint: true` is an output lifecycle attribute, not an alias for
`persist`. Checkpoints are supported under `crab exp run`, where each acknowledged
checkpoint records an immutable parent-linked snapshot of all checkpoint
outputs plus declared metrics. `crab run`/`repro` must reject checkpoint stages
with an actionable “use `crab exp run`” error until a separate non-experiment
lifecycle is deliberately designed.

A running stage signals a checkpoint through one canonical, cross-platform
internal `crab workflow checkpoint` subcommand authenticated by per-run
inherited state. Do not parse stdout or watch arbitrary files. The stage writes
and flushes its output, calls the control command, and receives success only
after Crab has hashed/cached the snapshot and durably appended the lineage
record. The token/control path must be local, private, and redacted from logs.

### One platform command adapter

`Cmd::Argv` stays shell-free. `Cmd::Shell` and shell lists use a single
platform adapter: `/bin/sh -c` on Unix and `cmd.exe /D /S /C` on Windows.
Shell family and target platform participate in the stage hash, preventing a
cache entry created with one command language from replaying on another.
Hermetic sandboxing remains explicitly macOS-only until separately implemented.

### One external data provider boundary

Add the reusable provider-neutral `ExternalDataStore` contract and adapters in
`crates/crab-storage`, because workflow, migration, artifact retrieval, and
data import all consume it. `crates/crab-workflow` owns URL resolution,
capability policy, hashing, and migration decisions; `crab` owns credential
resolution and CLI composition. The contract needs `stat`, `list`,
`open_stream`, and only where sound, conditional/atomic write. `open_stream`
returns an incremental byte stream plus metadata; it must not expose a helper
that eagerly returns an entire object. Existing HTTP/file/object-store behavior
becomes adapters behind that boundary. All consumers call the same resolver
instead of growing sibling implementations.

Read-only dependency support and writable output support are separate
capabilities. A provider is advertised only for operations proven live. Parser
acceptance without an available runtime provider is a configuration error at
preflight, not a late execution failure.

### One Git-native artifact lifecycle

`artifacts:` is catalog metadata in `crab.yaml`. Immutable artifact versions
are manifests associated with exact Crab pointer/content identities and source
Git commits. Mutable lifecycle stages such as `candidate`, `staging`, and
`production` are compare-and-swap refs to immutable versions. Promotion moves
metadata only; it never copies artifact bytes.

Use this canonical ref namespace, subject to the Phase 5 ref-validation test:

```text
refs/crab/artifacts/<percent-encoded-name>/versions/<b3-version-id>
refs/crab/artifacts/<percent-encoded-name>/stages/<normalized-stage>
```

Encode the UTF-8 artifact name as uppercase percent-encoded path bytes, reject
empty names and Git-ref-invalid decoded names, lowercase the stage after
validation against `[a-z0-9][a-z0-9._-]*`, and use the lowercase Blake3
manifest identity as the immutable version ID. Phase 5 must prove collision
handling, Git ref validation, remote-helper visibility, and GC reachability
before publishing this namespace. Reuse the existing Crab reader/hydration
path for `get`; do not add a second downloader.

## Commands used throughout execution

The main disk is limited. Before any Cargo command that can compile, verify the
external volume and set a checkout-specific target directory on that exact
invocation:

```bash
test -d /Volumes/Workspace && test -w /Volumes/Workspace
mkdir -p /Volumes/Workspace/crabbuild-target/crab-main
```

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format check | `cargo fmt --all -- --check` | exit 0, no diff |
| Workflow unit tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-workflow --locked` | all pass |
| CLI workflow tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --tests --locked` | all CLI unit/integration tests pass |
| Targeted lint | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo clippy -p crab-workflow -p crab --all-targets --locked -- -D warnings` | exit 0, no warnings |
| Public docs typecheck | `cd crab-web && npm run typecheck` | exit 0, no TypeScript errors |
| Public docs lint/links | `cd crab-web && npm run lint && npm run check:links` | exit 0; no lint or broken-doc-link errors |
| Source mutation audit | `git diff --stat && git status --short` | only files for the active phase plus this plan/index |

Use direct Cargo commands for local proof. Broad/full suites and live provider
qualification belong in CI or a dedicated environment. Do not run a Make target
until its artifact lookup is proven to honor `CARGO_TARGET_DIR`.
If `crab-web/node_modules` is absent, run `npm install` once in `crab-web`,
retry the failed command once, and report the first actionable installation
error; never edit `node_modules` or commit a lockfile change without review.

## Scope

**In scope**:

- `crates/crab-workflow/` — contracts, parsing, hashing, execution, cache,
  experiments, GC, migration, and external-provider policy.
- `crates/crab-storage/` — provider-neutral external stream/stat/list/write
  adapters and capability contracts; only after dependency review.
- `crab/src/main.rs`, workflow/data/artifact command modules, and
  `crab/src/core/config.rs` — user-facing CLI and composition.
- `crab/tests/workflow_*.rs`, new focused integration tests, and provider test
  harnesses.
- `crab/scripts/e2e/run_dvc_workflow_smoke.py` and new portable fixtures.
- `.github/workflows/` and release evidence verification.
- `crab-web/content/docs/` — canonical public Fumadocs pages, CLI reference,
  migration guide, provider matrix, and release-facing workflow guidance.
- `crab/docs/workflow/`, `crab/docs/design/`, and `crab/docs/guides/` — legacy
  or internal Markdown that must be updated, redirected, or explicitly marked
  non-canonical so it cannot contradict public docs.
- `Cargo.toml`/`Cargo.lock` only after dependency contract review and approval.

**Out of scope**:

- Crab Desktop, Electron, desktop IPC, UI, and desktop tests.
- Plan 013 Git protocol v2/partial-clone work.
- Replacing Crab's canonical object-storage layout or pointer format.
- A hosted registry service, hosted workflow scheduler, or new mandatory Crab
  server. The CLI and object store remain sufficient.
- Automatic deletion, renaming, or mutation of `.dvc/` or DVC remotes.
- Compatibility aliases, fallback readers, or parallel legacy implementations
  unless a shipped tagged contract is cited and a removal plan is approved.
- Bucket-wide GC in any test.

## Git workflow

- Use a dedicated branch per numbered phase or bounded subphase, for example
  `agent/014a-workflow-contract-containment`.
- Commit each independently green slice with concise conventional-ish messages,
  for example `fix(workflow): reject unsupported checkpoint migration`.
- Run `cargo fmt` before each commit. Do not push or open a PR unless asked.
- Before every PR verdict, build the required evidence map: changed surface,
  entry point, owner boundary, caller, callee, sibling surfaces sharing the
  invariant, tests, docs, and current tagged behavior. Explicitly answer:
  “Is this the best fix, or merely a plausible fix?”

## Phase 0 — Contain silent data and semantic loss (P0, 2–4 days)

### Delivery context

This phase is a safe precursor and should land first. It intentionally removes
misleading acceptance before adding features. Users get actionable preflight
errors rather than a successful-looking conversion that discards meaning.

### Actionable items

1. Add a structured migration preflight report in
   `crates/crab-workflow/src/dvc_migration.rs` with fatal findings, warnings,
   discovered feature names, and stable codes. Reserve codes for at least
   `dvc_checkpoint_unsupported`, `artifact_lifecycle_pending`,
   `dvc_schema_unsupported`, `dvc_remote_unmapped`, `dvc_source_missing`,
   `dvc_source_corrupt`, `dvc_lock_mismatch`, and `dvc_secret_redacted`;
   map each user-visible finding to the repository's `CRAB-E####` error
   catalog before release; keep presentation and file mutation policy in
   `crab/src/cmd/migrate.rs`.
2. Make `checkpoint: true` a fatal unsupported finding. Delete the conversion
   to `persist: true` and replace the existing test at
   `crates/crab-workflow/src/dvc_migration.rs:1034` with a rejection test.
   Add the same explicit field-presence rejection to Crab workflow YAML
   validation before Phase 3; serde must not silently ignore a `checkpoint`
   key in a hand-authored `crab.yaml`.
3. Stop discarding top-level `artifacts:` in `crates/crab-workflow/src/yaml.rs`.
   Add `Workflow.artifacts: ArtifactMetadata` (an empty catalog when absent)
   where the versioned metadata stores the canonical, sorted raw declarations and
   `schema_version: 1`; it is parseable metadata only, not executable lifecycle
   state. Validate that the value is structurally representable, but do not
   resolve artifact behavior before Phase 5. Remove `let _ = &raw.artifacts`
   and every silent-drop path. The migration command must report
   `artifact_lifecycle_pending` as fatal for a cutover-safe migration until
   Phase 5 validates `ArtifactDecl` fully. This preserves unsupported semantics
   while preventing a false-safe conversion.
4. Before Phase 3 lands, make `stage add --checkpoints` fail with the same
   stable diagnostic. Once the checkpoint protocol is available, serialize an
   explicit `checkpoint: true` field and keep checkpoint execution behind the
   experiment lifecycle gate. At no point map it to `persist: true` or claim
   that a persistent output is a checkpoint.
5. Update the canonical pages
   `crab-web/content/docs/cli/guides/migrating-from-dvc.mdx` and
   `crab-web/content/docs/cli/workflow/dvc-migration.mdx`, plus contradictory
   `crab/docs/workflow/migration-from-dvc.md` and
   `crab/docs/design/vs-dvc-workflow.md`: current migration is a preflighted
   pipeline conversion, never authorizes `.dvc/` removal, preserves but blocks
   artifact lifecycle metadata, and rejects checkpoint conversion.
6. Add JSON and text tests proving the same finding codes and no partial output
   file is written after a fatal preflight.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-workflow dvc_migration --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab --test workflow_migration --locked
rg -n 'checkpoint_(persist|.*persist)|checkpoint.*(becomes|converted|stored).*persist|persistent cached outs|ignored by workflow execution|let _ = &raw\.artifacts' \
  crates/crab-workflow crab/src crab-web/content/docs crab/docs
```

Expected: tests pass; the final search finds no production or documentation
claim equating checkpoint with persist, saying artifacts are accepted but
ignored, or discarding the raw artifact field. Fixture text that asserts the
typed pending error is allowed.

### Phase exit gate G0

- `checkpoint: true`, unknown `.dvc` constructs, and unsupported
  remote/provider records cannot produce a cutover-safe migration. Artifact
  metadata is preserved or rejected with a typed error, never discarded.
- Fatal preflight leaves the target YAML and Crab data unchanged.
- Text and JSON identify exact source files/fields without exposing secrets.

## Phase 1 — Make command execution native on Windows (P0, 1–2 weeks)

### Delivery context

This phase owns process creation, command hashing, timeout/cancellation, and
native platform proof. It does not attempt to translate POSIX shell syntax to
Windows. A shell-string stage is interpreted by the native default shell;
portable workflows should use argv commands or author platform-appropriate
commands.

### Actionable items

1. Introduce one internal platform shell descriptor in
   `crates/crab-workflow/src/executor.rs` (extract a module only if it reduces
   repeated policy). Route `Cmd::Shell`, `Cmd::ShellList`, hooks, and tests
   through it. Preserve `Cmd::Argv` as direct execution. Make the existing
   `crates/crab-workflow/src/sandbox.rs` policy consume the same platform
   descriptor for command construction and report its macOS-only capability;
   there must be no second shell-selection path.
2. Unix default: `/bin/sh -c`. Windows default: `cmd.exe /D /S /C`. Reject an
   empty argv as invalid rather than turning it into the POSIX `:` command.
3. Include OS, architecture, and shell family in the execution fingerprint or
   stage hash. Add a serialized hash version if required; never replay a Unix
   shell cache entry on Windows.
4. Audit timeout and cancellation in `crates/crab-workflow/src/signals.rs`.
   Ensure stage descendants are terminated, not only the immediate shell.
   Prefer a native Windows Job Object. Any new `windows-sys` feature/dependency
   requires source/type/license review and lockfile inspection.
5. Keep hermetic sandbox behavior explicit: supported on macOS only; requested
   sandboxing on Windows must fail before spawning a command.
6. Add unit tests for exact program/argument construction, sandbox capability
   rejection, and integration tests
   for argv, shell string, shell list, hook, environment, working directory,
   retry, timeout, descendant cleanup, cache replay, JSONL, and file/directory
   output materialization/atomic replacement on each OS. Unix termination must
   use a process-group policy; Windows termination must use a Job Object or a
   reviewed equivalent so descendants cannot survive a timeout.
7. Add a native `windows-latest` workflow job that runs the produced binary.
   Cross-compilation is not execution proof. Add macOS workflow execution too.
8. Document shell portability in a new canonical
   `crab-web/content/docs/cli/workflow/platform-shells.mdx` (and its nearest
   `meta.json`), update the legacy workflow guide, and recommend argv form for
   cross-platform DAGs.

**Verify**:

```bash
rg -n 'Command::new\("/bin/sh"\)|wrap_command\("/bin/sh"' \
  crates/crab-workflow/src
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-workflow --locked
```

Expected locally: only the Unix platform-adapter branch and macOS sandbox may
name `/bin/sh`; tests pass. CI must additionally show native Linux, macOS, and
Windows workflow execution, including timeout descendant cleanup.

### Phase exit gate G1

- A release-built Windows CLI executes argv and native shell stages.
- Timeout/cancel cannot leave a tested child process running.
- Cache keys cannot cross incompatible shell/platform semantics.
- Docs do not imply POSIX shell syntax is portable to Windows.

## Phase 2 — Implement safe, resumable DVC migration (P0, 3–5 weeks)

### Delivery context

Migration imports state; it is not format conversion. Build the inventory,
source-resolution, and journal contracts in `crates/crab-workflow`; keep
repository discovery, credential resolution, canonical pointer/staging writes,
and CLI rendering in `crab/src/cmd/migrate.rs`. All mutations must be journaled
and idempotent.

### Actionable items

1. Write `crab/docs/design/dvc-migration-contract.md` before implementation.
   Define source precedence, trust boundaries, journal schema, resume rules,
   cutover criteria, rollback behavior, and every supported/unsupported DVC
   record. Cite the exact DVC versions/fixtures used.
2. Add a recursive inventory that discovers:
   - every `dvc.yaml`, `dvc.lock`, standalone `*.dvc`, and `.dvcignore` file;
   - `.dvc/config` and `.dvc/config.local` with DVC precedence;
   - local or externally configured DVC cache roots;
   - file objects, `.dir` manifests, output flags, named remotes, `repo`/`path`/
     `rev`/checksum import provenance in `.dvc` files, run cache, repo lock,
     read/write lock state, and DVC ignore rules that affect the tracked tree;
   - materialized output paths and their current byte/checksum status.
   Record the versioned cache locator exactly as found (including
   `.dvc/cache/files/md5/<two-hex>/<remainder>` and configured `cache.dir`
   roots), and parse `.dir` JSON member paths, sizes, and object identities as
   a directory manifest. Treat DVC MD5/hash/etag values as source locators and
   verification inputs only; never promote them to Crab content hashes. Record
   the config source file and precedence for each remote/cache setting so a
   later resume cannot silently select a different cache.
3. Never serialize credentials. Report only remote name, scheme, credential
   source category, and whether resolution succeeded. Add redaction tests for
   URL userinfo, access keys, tokens, database fields, and local config.
   Require an explicit CLI-only mapping for an ambiguous DVC remote (for
   example `--remote-map NAME=crab://...` or a named Crab remote); never infer a
   write target from a secret-bearing URL. A resolved remote must record its
   capability and provenance without storing the credential. For every named
   DVC remote, write a redacted remote descriptor containing its name, source
   config path, credential-free canonical source identity, resolved capability,
   and explicit
   Crab destination mapping; an unmapped or ambiguous remote remains a fatal
   cutover finding rather than disappearing from the report.
4. Add a versioned, canonical migration journal under Crab-owned per-worktree
   state. Each record needs source path/type, DVC identity, chosen source
   (working tree/cache/remote), Crab hash, transfer/verification state, and
   error code. Include a stable inventory key, source locator (cache relative
   path, remote object key, or materialized path), byte count, and journal
   schema/version. Atomic append/update and crash recovery are mandatory; a
   resume must reject a changed source/config fingerprint instead of reusing a
   stale transfer.
5. Resolve bytes in this exact order: verified materialized output, verified
   local DVC cache object, then a supported live remote. Any proposed order
   change requires a reviewed design update and a new fixture matrix. A
   checksum mismatch is fatal. Missing bytes are fatal. Directory `.dir`
   manifests must cover every child and reject path traversal, duplicates,
   wrong sizes, and missing members.
6. Feed resolved bytes through Crab's canonical add/staging/flush/push path.
   Do not implement a second pointer format or storage layout. Ensure staged
   xorbs flush before refs/bundles publish.
7. Convert standalone `.dvc` tracked outputs into Crab pointer files and Git
   tracking. Preserve import provenance in a versioned source descriptor used
   by the later `crab data update` surface, not in a runtime compatibility
   reader. The descriptor records source type, canonical locator, locked
   revision/checksum/query identity, and the verified Crab content identity;
   the locator is credential-free and it does not promise update support before
   Phase 7.
8. Build `crab.lock` from recomputed Crab hashes and the converted workflow.
   Cross-check each output against the materialized/imported bytes. Do not copy
   DVC MD5/etag values into Crab hash fields.
9. Record DVC run-cache and checkpoint records. Until Phase 3, any checkpoint
   lineage keeps `safe_to_remove_dvc` false. Unknown lock/cache versions also
   keep it false. A DVC `import`, `import-url`, or `import-db` record must be
   represented by the versioned source descriptor (or explicitly remain
   unsafe); it is not enough to copy only the resulting bytes. A descriptor
   whose provider/connector cannot be revalidated keeps cutover unsafe until
   Phase 7 supplies the tested update contract.
10. Add `--plan`/dry-run, `--resume`, text, JSON, and JSONL output. Dry-run
    performs no Crab data, Git index, YAML, lockfile, or journal mutation.
    `--stdout` remains conversion-only and cannot claim cutover safety; a
    cutover report is emitted only by the repository-aware path.
11. Write generated YAML, lockfile, pointers, and Git index changes through
    the repository's existing atomic/transactional primitives. If any local
    mutation fails, leave the prior tracked state intact, retain the journal
    and immutable transferred data for resume, and report the exact recovery
    action; never fabricate a rollback by deleting published Crab objects.
12. Produce a final cutover report with counts and stable reasons. Never offer
    a delete flag. Print a manual-removal suggestion only when the boolean is
    true and a clean-clone/hydration verification has passed.
13. Add fixture matrices generated by real pinned DVC versions: standalone
    file, directory `.dir`, multiple pipelines, external cache, cache-only,
    remote-only, `dvc import`, `dvc import-url`, `dvc import-db` metadata,
    duplicate content, missing object, corrupt object, dirty output, config
    precedence, secret redaction, crash/resume, and an unsupported remote.
    Keep generated fixtures and DVC environments under a dedicated mounted
    workspace directory (for example
    `/Volumes/Workspace/crab-workflow-fixtures`); put any external qualification
    repository under `/Volumes/Workspace/Github/<owner>/<repository>`, record
    the DVC version and fixture digest, and treat those repositories as
    read-only inputs.
14. Add a real E2E: migrate, push, clone into a new worktree, hydrate, and
    compare file bytes/tree shape/mode to the DVC source. Capture a recursive
    path/type/mode/size/content digest of `.dvc/` before each run and assert
    the same digest and file set afterward; the source `.dvc/` must still exist
    after every scenario, including failure and resume.
15. Update the canonical public migration guide and its nearest `meta.json`,
    plus the legacy/internal migration docs, with the exact supported fixture
    profile, source precedence, journal/resume behavior, remote mapping rules,
    and the manual-only cutover report. Run the public-docs checks before the
    phase gate.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-workflow dvc_migration --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab --test workflow_migration --locked
git diff --exit-code -- .dvc
git status --short --untracked-files=all -- .dvc
```

Expected: all unit/integration fixtures pass; no test or command mutates the
source `.dvc/` (the E2E digest is unchanged); RustFS E2E proves byte-identical
clean-clone hydration for the supported non-checkpoint matrix.

### Phase exit gate G2

- Every inventory item appears in the cutover report exactly once.
- Dry-run has zero mutations; interrupted transfer resumes without duplicates.
- Supported projects clone/hydrate byte-identically from the Crab remote.
- Missing/corrupt/unknown/checkpoint state forces `safe_to_remove_dvc: false`.

## Phase 3 — Implement real checkpoint semantics (P0, 3–4 weeks)

### Delivery context

Legacy DVC checkpoints are experiment lineage. The design must distinguish
checkpoint identity, persistence, and final output. A persisted output merely
survives cleanup; a checkpoint is an immutable acknowledged point that can be
listed, applied, transported, resumed, reset, and garbage-collected safely.

### Actionable items

1. Write `crab/docs/design/workflow-checkpoints.md` and the public authoring
   page `crab-web/content/docs/cli/workflow/checkpoints.mdx`; update its nearest
   `meta.json`. Define legacy DVC version compatibility, event protocol,
   snapshot atomicity, IDs/sequence numbers,
   parent links, final checkpoint behavior, crash/cancel behavior, resume and
   reset, metrics association, push/pull, and GC reachability.
2. Add `checkpoint: bool` to the validated `Out` contract and parser. Validate
   that checkpoint outputs are cached local file/directory outputs. Checkpoint
   may imply preservation during execution but must remain a distinct field.
   Include it in stage hashing and version any serialized contract.
3. Implement one hidden `crab workflow checkpoint` command in
   `crab/src/main.rs` plus a focused `crab/src/cmd/workflow_checkpoint.rs`.
   The executor supplies `CRAB_WORKFLOW_CONTROL_DIR`, `CRAB_WORKFLOW_RUN_ID`,
   `CRAB_WORKFLOW_STAGE`, `CRAB_WORKFLOW_TOKEN`, and
   `CRAB_WORKFLOW_EXECUTABLE` only to the stage process. The command accepts no
   user-provided identity, writes a canonical JSON request through a private
   directory (Unix mode 0700; Windows owner-only ACL) and atomic rename, and
   waits for an acknowledgement. Authenticate
   the request with a keyed Blake3 MAC over run/stage/nonce/payload; never write
   the token to the request, journal, cache key, logs, or stage hash. The
   supervisor accepts only regular files created in that private directory,
   rejects symlinks, stale/replayed/wrong-stage/oversized requests, fsyncs the
   request and acknowledgement where the platform permits, removes control
   files on every exit path, and acknowledges only after the snapshot and
   lineage record are durable. This file protocol is the single cross-platform
   mechanism; do not add stdout parsing, arbitrary sentinel files, or another
   IPC path. Register it as a `WorkflowCmd` variant with a dedicated
   `workflow.checkpoint` schema/output-mode arm, schema JSON, stable error
   codes, and a public dispatch test; success is an exit-status acknowledgement
   to the stage and must not leak control metadata onto a JSON/JSONL stdout
   stream.
4. Add an immutable `CheckpointRecord` with schema version, experiment/stage,
   sequence/ID, parent, timestamp, stage hash, output hashes/manifests, metrics,
   and terminal/resumable status. Use canonical JSON and deterministic maps.
5. Extend experiment metadata from schema 3 with a migration ladder. Older
   metadata remains readable; newer unknown schemas fail. Do not add a fallback
   parser. Update the stage-ref/live-set sidecar so every reachable checkpoint
   cache object survives GC.
6. Make `crab exp show` display checkpoint chains in text/JSON, `exp apply`
   accept a checkpoint selector, and rerun resume the latest acknowledged
   checkpoint by default. Add an explicit `ExpCmd::Reset` exposed as
   `crab exp reset <exp-id>` with its own schema/error codes; it starts from the
   base and records the decision. Define one selector syntax (checkpoint ID or
   sequence, with ambiguity rejected) and use it consistently for `show`,
   `apply`, and resume. Do not overload the unrelated top-level or mount
   `reset` commands.
7. Ensure cancellation/crash leaves the latest acknowledged checkpoint usable
   and ignores a partially written next record. A successful stage creates a
   terminal checkpoint when final output differs from the latest event.
8. Include checkpoint lineage in experiment push/pull and conflict handling.
   Remote publication must be immutable/CAS and publish bytes before refs.
9. Teach DVC migration to convert compatible checkpoint/run-cache chains only
   after recomputing and verifying every snapshot. Unknown legacy shapes remain
   fatal and keep cutover unsafe.
10. Replace the Phase 0 checkpoint rejection and update `stage add
    --checkpoints` to emit the real field. Keep `crab run`/`repro` rejection;
    only `crab exp run` owns checkpoint lineage in this roadmap. Exclude the
    five internal control variables from environment capture and stage hashing,
    while including the checkpoint protocol/schema version in the stage hash.
11. Add an E2E stage that emits three checkpoints: interrupt after two, show
    both, apply the first and verify its bytes, explicitly resume from the
    second, finish the third, push/pull in a clean clone, and verify output
    bytes and per-checkpoint metrics. Add GC proof before and after deleting
    experiment refs.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-workflow --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab --test workflow_exp --locked
rg -n 'checkpoint_(persist|.*persist)|checkpoint.*(becomes|converted|stored).*persist|checkpoints.*persistent cached' \
  crates/crab-workflow crab/src crab-web/content/docs crab/docs
```

Expected: all checkpoint/experiment/GC tests pass; no code or docs equate
checkpoint with persist; clean-clone E2E can list/apply/resume the lineage.

### Phase exit gate G3

- Every acknowledged checkpoint is immutable, addressable, and transportable.
- Crash/resume starts at the latest acknowledged point and never at a partial
  point.
- `exp apply`, reset, GC, push, and pull preserve lineage invariants.
- Compatible DVC checkpoint projects can receive a true cutover-safe report.

## Phase 4 — Make workflow qualification a release gate (P1, 1–2 weeks)

### Delivery context

Default enablement is the last action in this phase, not the first. First make
the CI evidence reusable, exact-commit-bound, and required by release. Keep the
RustFS smoke command-level and preserve its existing breadth.

### Actionable items

1. Add a dedicated workflow CI file with native Ubuntu, macOS, and Windows jobs
   for `crab-workflow` and CLI workflow tests. Split OS-specific cases only when
   behavior is intentionally platform-specific; keep shared assertions shared.
   The Windows job must use the release feature contract currently declared in
   `.github/workflows/release.yml` (`--no-default-features --features
   simd-accel,tier,watch,nfs`) and run the resulting binary, while the minimal
   `crab-workflow` feature set is checked separately.
2. Turn `crab/scripts/e2e/run_dvc_workflow_smoke.py` into a CI-invoked RustFS
   job. Extend it with migration cutover, checkpoint, artifacts as phases land,
   cache-only, explain-miss, retry/timeout/keep-going, crash recovery, queue,
   Hydra, metrics, plots, JSON, and JSONL assertions.
3. Emit a versioned evidence artifact containing source SHA, workflow run ID and
   attempt, Crab version, OS, RustFS image/version, scenario IDs, start/end,
   result, and redacted diagnostics. Add a verifier rather than trusting a
   filename or successful producer job.
4. Add a release job that downloads and validates workflow evidence for the
   exact tag commit. Make release builds depend on it in
   `.github/workflows/release.yml`; no skipped-success loophole for a normal
   release. The smoke must execute the release candidate binary selected by an
   explicit path/PATH entry, not an ambient installed `crab`.
5. Add native Windows execution of the release candidate binary, not merely a
   cross-build. Smoke argv/shell/list, workflow parsing, run/cache, timeout, and
   structured output.
6. The existing tagged/docs contract already exposes `workflow.enabled` as a
   user-settable key. Change its default to `true`, retain explicit
   `enabled = false` as the shipped opt-out, and keep one executor path. Update
   the config reference, every workflow page, tests, environment overrides,
   smoke setup, and error text. Do not remove the key or silently reinterpret an
   explicit false value. Use `git show` on the release tags as a final contract
   check; if a serialized/config shape differs, stop before changing it.
7. Update docs and release notes only after the required CI/release gate is
   green. Default enablement must not imply G5/G6 parity.

**Verify**:

```bash
rg -n 'run_dvc_workflow_smoke.py' .github/workflows
rg -n 'workflow.enabled|DEFAULT_WORKFLOW_ENABLED' \
  crab/src crab/tests crab-web/content/docs crab/docs
```

Expected: the smoke is invoked by CI and its verifier gates release; default
configuration enables workflows while explicit `enabled = false` still blocks
them. Native Windows evidence uses the release candidate binary.

### Phase exit gate G4

- Linux, macOS, Windows, and RustFS evidence are required and exact-SHA-bound.
- Release cannot proceed with missing, stale, malformed, or failed workflow
  evidence.
- Workflows are default-on through one documented canonical path.

## Phase 5 — Make artifacts and models first-class (P1, 2–3 weeks)

### Delivery context

Artifact declaration, immutable version identity, and mutable promotion labels
must be separate concepts. Reuse Git refs, Crab content identities, remote CAS,
reader/hydration, and GC. Do not build a hosted registry or duplicate experiment
promotion internals merely because names are similar.

### Actionable items

1. Write `crab/docs/design/artifact-registry.md` and the public pages
   `crab-web/content/docs/cli/workflow/artifacts.mdx` and
   `crab-web/content/docs/cli/workflow/artifact-promotions.mdx` (and update the
   nearest `meta.json`), defining
   declaration schema,
   version identity, ref namespace/encoding, promotion CAS, history/audit,
   offline behavior, dirty-output rejection, JSON contracts, remote behavior,
   and GC ownership.
2. Add a strict `ArtifactDecl` to `Workflow`: name, repo-relative path, type,
   description, labels, and bounded metadata. Validate unique normalized names
   and paths; require the path to be a declared output or tracked pointer.
   Use a canonical artifact name plus an immutable, opaque content-addressed
   version ID (never reused); keep creation sequence/time only as manifest
   metadata. The first lifecycle does not infer SemVer ordering or silently
   overwrite a version. Human release labels are optional annotations only and
   duplicate labels are rejected rather than made the identity.
3. Add a schema-versioned immutable artifact manifest containing declaration
   identity, source Git commit, source stage/experiment when applicable,
   pointer/content/tree hash, size, creation time, and annotations. No mutable
   stage lives inside an immutable version. Reuse `crab-metadata` ref/index and
   GC publication primitives where they exist; the checkout-local registry is
   scaffolding only and must be integrated with those primitives before G5. Do
   not ship a second persistent ref/index implementation in the CLI or
   workflow crate.
4. Implement `crab artifacts list`, `show`, `get`, `version create`, `history`,
   and `promote`. `version create` returns the immutable version ID and
   manifest identity; `promote` requires that ID plus an explicit stage. `get`
   must select exactly one immutable `--version` or
   mutable `--stage` (defaulting only when the declaration defines one), and
   must use the canonical reader/hydration path. `show` returns declaration,
   available immutable versions, and stage pointers without hydrating bytes.
   Every command supports stable text and JSON; long transfers support JSONL
   progress.
5. Refuse version creation when the working output is dirty, not represented by
   the current lockfile, absent from local/remote cache, or unpushed when remote
   publication is requested.
6. Make versions immutable and promotion compare-and-swap. Promotion copies no
   data; conflicts return current/expected identities. Store an immutable audit
   event or derive history from protected ref transitions.
7. Add artifact refs/manifests to GC live-set enumeration and remote push/pull.
   Protect every referenced xorb/shard and observe grace periods. Publish
   manifest/content objects before immutable version refs, and update stage
   refs only with CAS; verify the Git remote helper exposes the refs and a
   clean clone can enumerate them without the originating worktree's local
   index.
8. Teach DVC migration to retain supported top-level artifact declarations and
   classify unsupported metadata. Do not infer promoted stages from DVC metadata
   that lacks that contract.
9. Add E2E: produce a model, create v1, list/show, promote candidate→staging→
   production, get it into a clean directory, verify bytes, reject dirty/stale
   creation, reject CAS conflict, clone/pull, and prove GC retention.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-workflow --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab --locked --test workflow_artifacts
rg -n 'let _ = &raw.artifacts|ignored by workflow execution' \
  crates/crab-workflow crab/src crab-web/content/docs crab/docs
```

Expected: tests pass; the final search returns no matches; clean-clone artifact
get is byte-identical and promotion conflicts are deterministic.

## Phase 6 — Scale and broaden external dependencies (P1, 3–6+ weeks)

### Phase 6A: bounded-memory, validator-aware hashing

1. Centralize URL/object-store dependency and external-output resolution behind
   the `crab-storage` `ExternalDataStore` contract. Remove duplicated
   full-fetch/hash policy from `stage_runtime.rs`, `executor.rs`, status,
   migration, and future data import; `crab-workflow` remains the owner of
   stage identity and policy, not provider construction.
2. Replace object `.bytes()` calls with incremental Blake3 over
   `GetResult::into_stream()`. Stream HTTP and local files through the same
   bounded hashing sink. External-output ingestion must hash while writing a
   bounded temporary file/tree and atomically replace the destination only
   after verification; it must not buffer a whole response in the executor.
   Bound prefix concurrency and sort entries before tree hashing.
3. Add a versioned external-hash index in Crab-owned per-worktree state. Key by
   canonical resource identity with secret-bearing URL fields removed plus a
   non-secret credential scope/tenant
   fingerprint; if the provider cannot supply a stable scope, never reuse the
   entry across credential contexts. Record provider, size, strong
   validator/version/last-modified as applicable, Crab hash, and observation
   time. Do not add a user-facing cache-concurrency config before measuring the
   fixed bounded implementation.
4. For HTTP, use conditional requests. Reuse only after a trustworthy 304 or
   equivalent validator match. Handle redirects, weak/missing ETags, `Vary`,
   auth changes, and servers that reject HEAD by falling back to a streamed GET.
5. For object stores, compare version plus size or another provider-defined
   strong identity. Never use ETag as content hash. For prefixes, persist a
   deterministic per-member metadata/hash manifest (not only a root marker),
   stream only new/changed members, and recompute the tree so deletion alters
   the tree hash. If a provider returns only a weak or absent validator, stream
   the object and update the index after verification.
6. Keep a pinned `b3:` dependency as the zero-network path. Missing trustworthy
   validators require a full streamed hash; correctness wins over bandwidth.
7. Add instrumented mock tests asserting GET count and transferred bytes, plus
   benchmarks for peak memory and one-changed-object prefixes. Record baseline
   and target: peak memory bounded independently of object size; unchanged
   object transfers zero body bytes after validation.

### Phase 6B: live remote providers

Deliver providers independently in this order: SSH/SFTP, WebDAV, HDFS/WebHDFS,
Google Drive, Aliyun OSS. Register
canonical URL schemes explicitly: `ssh://` and `sftp://`; `webdav://` and
`webdavs://`; `hdfs://` and `webhdfs://`; `gdrive://`; and `oss://`. Do not
accept a scheme merely because a parser recognizes its spelling.

For each provider:

1. Complete the dependency/security review and record the chosen crate/API and
   rejected alternatives in a short ADR/design section. Gate each provider as a
   separate feature in `crab-storage`/`crab-workflow`; library default features
   stay minimal, while release artifacts explicitly list the vetted provider
   feature set that they ship. Move the currently explicit object-store
   transport features behind those named flags without changing the release
   artifact's selected capabilities, and update every downstream feature
   forwarding declaration together.
   Qualify the already-shipped filesystem, HTTP, S3, GCS, and Azure adapters in
   the same matrix before adding new providers; a new provider cannot weaken
   their existing read/write/atomic guarantees.
2. Implement read capabilities first: stat, list, and streaming open. Advertise
   the scheme only when compiled and configured. Unsupported operations fail at
   preflight with provider/capability detail. Keep URL parsing and capability
   discovery in one registry shared by workflow hashing, status, migration,
   artifacts, and `crab data`.
3. Add writable external outputs only if the provider supports a reviewed
   temporary-upload plus atomic/conditional publication contract. Otherwise
   document read-only support; do not emulate atomicity with overwrite.
4. Integrate native credential sources without YAML secrets:
   - SSH/SFTP: OpenSSH config, agent, and strict known-host verification.
   - WebDAV: HTTPS validation and standard credential injection.
   - HDFS/WebHDFS: explicitly state simple/Kerberos support; do not silently
     downgrade auth.
   - Drive: OAuth lifecycle and scoped token storage/redaction.
   - OSS: determine whether S3 compatibility is contract-sufficient or a
     native signer is required; test the selected service live.
5. Add container/emulator/live service tests where faithful; a mocked protocol
   is not the release gate. Retain redacted evidence with operation capability,
   object size/hash, and provider version.
6. Update a generated capability matrix for dependency read, directory list,
   conditional validation, output write, atomic publish, and tested platforms;
   publish it at `crab-web/content/docs/cli/reference/external-providers.mdx`
   with the nearest `meta.json` and reject documentation changes that are not
   generated from the runtime registry/test evidence.

**Verify Phase 6**:

```bash
rg -n '\.bytes\(\)' \
  crates/crab-workflow/src/stage_runtime.rs \
  crates/crab-workflow/src/executor.rs
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-workflow --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-storage external_provider --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo check -p crab-workflow --no-default-features --locked
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo check -p crab-storage --no-default-features --locked
```

Expected: every remaining `.bytes()` call is either removed from a large
external-data path or explicitly documented as a bounded control-plane read;
the focused tests prove streaming and validator reuse. Each advertised provider
has live retained evidence for every advertised capability. Repeat the
`cargo check`/`cargo test` pair for the exact provider feature combinations
shipped in each release artifact; both the minimal and each changed feature
set must compile without changing serialized formats.

### Phase exit gate G5

- Peak hashing memory does not scale with object size.
- Unchanged validated objects do not transfer their body again.
- Every accepted scheme resolves to a live provider or fails at preflight.
- Docs distinguish read/list/write/atomic capabilities; no blanket “supported”
  claim hides an unsupported operation.

## Phase 7 — Complete the CLI data ecosystem (P2, 3–5 weeks)

### Delivery context

Avoid collisions with existing `import` (raw object-store ingestion) and
`update` (CLI self-update). Add one coherent `crab data` namespace for new
source-management commands. Keep the already-shipped `crab download` command
and its visible `crab get` spelling as the canonical DVC-get equivalent; extend
that implementation only where its selector/revision contract is insufficient.
Reuse the existing reader, external provider boundary, migration source
descriptor, artifact registry, pointers, lockfile, and status logic.

### Actionable items

1. Write `crab/docs/design/data-commands.md` and
   `crab-web/content/docs/cli/data/data-commands.mdx` (and update the nearest
   `meta.json`), defining source descriptors,
   locking/update semantics, command mapping, output schemas, transaction and
   rollback behavior, credentials, and differences from DVC.
2. Extend the existing `crab download`/`crab get` contract only as needed for
   DVC-get parity; do not add a duplicate `crab data get` wrapper.
3. Add `crab data list <repo> [path] [--rev] [--recursive]`: read Git trees,
   Crab pointer metadata, and managed source descriptors without hydrating
   content; support stable JSON.
4. Add `crab data import` for another Git/Crab repository and path. Store a
   versioned source descriptor with repository identity, requested ref, locked
   commit, path, and verified content identity; materialize/track via canonical
   paths.
5. Add `crab data import-url` using `ExternalDataStore`. Lock final canonical
   URL/resource identity, validator, and Crab digest. Updating must detect
   changed content and preserve provenance.
6. Add `crab data update <target>`: transactionally resolve the descriptor,
   fetch/verify new bytes, update the pointer and lock/provenance record, and
   leave the previous state intact on failure. Support dry-run and JSON.
7. Add `crab data status`: compare working-tree materialization, pointer/Git
   state, local cache, remote availability, source freshness, and lock state.
   Avoid network unless requested or clearly marked; include per-dimension
   status in JSON rather than collapsing everything to one boolean.
8. Design `crab data import-db` as a connector SPI before shipping a database.
   Canonical query text plus non-secret parameter names/types and source
   schema/version form the provenance and update fingerprint; secret parameter
   values are excluded or represented only by a one-way digest. Rows stream to
   the declared output format. Credentials come from standard external
   providers and are never recorded. Ship the first
   connector only with a real database E2E and documented snapshot/isolation
   semantics. A PostgreSQL connector may reuse the existing workspace `sqlx`
   dependency only after checking feature ownership; do not pull database
   runtime dependencies into `crab-workflow` for a CLI-only shortcut.
9. Integrate imported data/models with artifact version creation and promotion;
   do not introduce a second registry.
10. Add command-level tests: remote get/list without clone, repo import/update,
    URL conditional update, stale/offline/corrupt source behavior, transaction
    rollback, data status matrix, database snapshot repeatability, JSON/JSONL,
    and credential redaction.

**Verify**:

```bash
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab --locked --test data_commands
CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main \
  cargo test -p crab-workflow source_descriptor --locked
```

Expected: all commands pass real side-effect E2E at validation level 3 or
higher; failed updates leave pointers, lockfiles, and provenance byte-identical
to their pre-command state.

### Phase exit gate G6

- The documented DVC get/list/import/import-url/import-db/update/data-status
  profile has a tested Crab command or an explicit unsupported table entry.
- Registry workflows use the artifact lifecycle from Phase 5.
- Docs may recommend Crab as a general DVC replacement only for this exact
  supported profile and provider matrix.

## Cross-phase test plan

Use existing tests as structural patterns:

- `crab/tests/workflow_migration.rs` for CLI migration fixtures.
- `crab/tests/workflow_exp.rs` for experiment metadata and command behavior.
- `crab/tests/workflow_crash_safety.rs` and
  `crab/tests/workflow_journal.rs` for durable recovery.
- `crab/tests/workflow_run_retry.rs`,
  `crab/tests/workflow_run_keep_going.rs`, and
  `crab/tests/workflow_run_failure_paths.rs` for scheduler invariants.
- `crab/tests/workflow_run_explain_miss.rs` and
  `crab/tests/workflow_dag_jsonl.rs` for automation contracts.
- `crab/tests/tier_workflow_s3.rs` and
  `crab/scripts/e2e/run_dvc_workflow_smoke.py` for real object-store wiring.
- `crates/crab-storage` transport/conditional-write tests for the shared
  external-provider contract; exercise both its minimal features and every
  release feature combination that the provider matrix advertises.

Required test categories across the roadmap:

1. Deterministic schema serialization and newer-schema rejection.
2. Crash at every journal/publication boundary, then resume.
3. Malformed/path-traversing/duplicate/oversized directory manifests.
4. Secret redaction in text, JSON, JSONL, errors, logs, and retained evidence.
5. Native Windows/Unix command construction and process-tree termination.
6. Concurrent migration, checkpoint, promotion, and update CAS conflicts.
7. GC reachability for migrated data, checkpoints, and artifact versions.
8. Working tree/cache/remote corruption and byte-identical reconstruction.
9. No regressions in the existing workflow strengths listed above.
10. Configuration default/opt-out behavior: a fresh config runs workflows by
    default, an explicit `workflow.enabled = false` blocks every workflow
    command, and `CRAB_WORKFLOW_ENABLED` remains a deliberate test/operator
    override rather than a second execution path.

Do not modify baseline, snapshot, expected-failure, or ignore files merely to
make a gate pass. Fix the source contract.

## Documentation deliverables

Each behavior/API phase updates documentation in the same PR:

- Migration guide: inventory coverage, source precedence, unsupported matrix,
  resume, verification, cutover report, and explicit non-deletion policy.
- Checkpoint guide: event protocol for stage authors, exp-only lifecycle,
  show/apply/resume/reset, remote transport, and crash semantics.
- Platform guide: argv versus native shell behavior and tested OS matrix.
- Artifact guide: declarations, immutable versions, promotion CAS, get/history,
  GC, and JSON examples.
- Remote matrix: scheme by read/list/write/atomic/validator/auth/live-test status.
- Data command guide: Crab command mapping and semantic differences from DVC.
- Comparison page: claims are generated from passed gates, not projected work.

For every new Fumadocs page, update the nearest `meta.json` navigation file and
run the public-docs typecheck, lint, and link checks. Do not hand-edit
`crab-web/.source/`.

Where applicable, final implementation handoffs should include the matching
`https://crab.build/docs/...` URLs after deployment.

## Done criteria

All must hold before this roadmap is marked DONE:

- [ ] G0 through G6 are recorded as passed with exact CI run/attempt/SHA.
- [ ] No production path silently converts checkpoint to persist or discards
      artifacts.
- [ ] Migration never deletes `.dvc/` and only reports cutover-safe after
      byte-identical clean-clone verification and full inventory accounting.
- [ ] Native Linux, macOS, and Windows workflow execution is release-gated.
- [ ] Checkpoint show/apply/resume/reset/push/pull/GC E2E passes.
- [ ] Artifact list/show/get/version/promote/history E2E passes.
- [ ] External hashing is bounded-memory and validator-aware; unchanged
      validated bodies are not transferred.
- [ ] Every advertised provider capability has live retained evidence.
- [ ] Data commands reach validation level 3+ and preserve state on failure.
- [ ] Existing workflow, experiment, metric, plot, queue, Hydra, caching, and
      JSON/JSONL suites remain green.
- [ ] `cargo fmt --all -- --check` and targeted clippy exit 0.
- [ ] Docs state the exact supported DVC profile and do not overclaim parity.
- [ ] No Crab Desktop file changed.
- [ ] `plans/README.md` status is changed to `DONE` only after G0–G6 and all
      evidence artifacts are recorded; otherwise it stays `IN PROGRESS` with
      the blocking gate named.

## STOP conditions

Stop and report; do not improvise if:

- Current code or scoped steering conflicts with the architecture decisions in
  this plan.
- A migration cannot account for a discovered DVC record, cache version,
  directory member, import source, lock entry, or run/checkpoint state. Keep
  cutover unsafe.
- Implementing a feature would require deleting/mutating `.dvc/`, trusting a
  DVC MD5/ETag as Crab content identity, or storing credentials.
- Checkpoint snapshot acknowledgement cannot be made durable before returning
  success to the stage.
- Windows descendant termination requires an unreviewed dependency or unsafe
  block. Obtain dependency/API approval first.
- A provider lacks strict host/TLS verification, required auth semantics,
  streaming reads, or a sound publication primitive for a claimed capability.
- An object validator is weak/ambiguous. Fall back to streaming; do not reuse a
  cached hash.
- Tagged release history proves removal/default-enable would break a shipped
  config or serialized contract without a reviewed migration.
- A phase requires touching Crab Desktop, changing Crab's storage layout, or
  adding a hosted service.
- A verification fails twice after a reasonable source fix, the external
  workspace volume is unavailable, or a live service cannot provide faithful
  qualification evidence.

## Maintenance and review notes

- Review migration changes as data-loss/security code: every successful path
  needs source, transfer, verification, and cutover evidence; every failure path
  must leave resumable state.
- Review checkpoint, artifact, and provider schemas as public contracts. Check
  canonical serialization, schema bumps, older-reader behavior, and GC before
  approval.
- Review one-sided provider fixes against all sibling consumers: run hashing,
  status, external outs, migration, artifact get, and data import/update.
- Review performance claims with transferred-byte and peak-memory evidence, not
  elapsed time alone.
- Keep capability matrices generated or tested from the same registry used at
  runtime so parser/docs cannot drift from live support.
- After G6, any new provider or registry stage must add live qualification and
  release evidence before documentation advertises it.
