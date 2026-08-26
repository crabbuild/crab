# Phase 7: Establish LFS production qualification and release gates

> **Executor instructions**: Qualification changes tests, CI, docs, and observability; it must not weaken expected results to make a run pass. Never use bucket-wide GC. Update the Phase 7 row when complete.
>
> **Drift check (run first)**: `git diff --stat 2cbd0d92..HEAD -- crab/scripts/e2e .github/workflows crab/src/lfs crab/src/cmd/lfs crab/docs packages/web/content/docs`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: Phases 0–4; Phase 5 for migration; Phase 6 for managed/HTTP profiles
- **Category**: tests, operations, release
- **Planned at**: commit `2cbd0d92`, 2026-08-25

## Why this matters

Unit tests and one successful live scale run do not prove failure recovery, bounded resources, provider portability, or standard-client behavior. This phase turns the user’s requested 100-commit/1,000-file/1–10 MiB workload into repeatable evidence and makes regressions block releases.

## Current state

- Focused baseline at `2cbd0d92`: 22 `crab-lfs` tests and 397 filtered CLI LFS tests passed.
- Live standalone-agent evidence: 100 commits, 1,000 current paths, 10,900 distinct 1 MiB objects, 11,429,478,400 bytes, successful push/clone/fetch/checkout/fsck at about 53 MiB/s LFS transfer.
- The remaining native publication phase dominated total push time.
- A literal 100 commits × 1,000 fresh 1 MiB objects is about 100,000 objects and 97.7 GiB before protocol/storage overhead. It was not run in the capacity-constrained environment.
- No dedicated LFS RustFS workflow exists; other subsystems provide RustFS workflow exemplars.
- `crab/src/lfs/lifecycle.rs::run_prune` and `run_fsck` are uncalled duplicates; canonical commands are `crab/src/lfs/prune.rs` and `crab/src/cmd/lfs/fsck.rs`.

## Commands you will need

| Purpose | Command | Expected |
|---------|---------|----------|
| Full Rust | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test --workspace --locked` | all pass |
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo clippy -p crab --all-targets --locked -- -D warnings` | exit 0 |
| Qualification | new RustFS qualification workflow/script | evidence artifact passes schema |

## Scope

**In scope**:
- Phase 0 harness expansion under `crab/scripts/e2e/`
- dedicated `.github/workflows/lfs-qualification.yml`
- metrics/evidence schema and release workflow gate
- failure-injection tests
- LFS compatibility and operations docs
- deletion/narrowing of uncalled duplicate lifecycle code after caller proof

**Out of scope**:
- running expensive profiles on every pull request
- committing credentials or generated large objects
- modifying external qualification repositories
- `crab gc --scope=bucket` or any bucket-wide deletion

## Git workflow

- Branch: `advisor/lfs-phase-6-qualification`
- Separate harness, workflow, release gate, and docs/lifecycle cleanup commits.

## Steps

### Step 1: Define qualification profiles and budgets

Create `smoke`, `scale-safe`, `full-scale`, `large-object`, `failure`, `locking`, `migration`, `managed-standalone`, and optional `http` profiles. Pin seed, Git/Git LFS/Crab/RustFS versions, object count, unique OIDs, logical bytes, wire bytes, commit/path counts, and host capacity. Refuse to start if estimated free disk/object-store space is below a safe margin.

Recommended profiles:

- `smoke`: 3 commits, 10 paths, 1–2 MiB.
- `scale-safe`: 100 commits, 1,000 current paths, about 10,000–12,000 unique 1–10 MiB objects within declared capacity.
- `full-scale`: 100 commits × 1,000 fresh objects, deterministic 1–10 MiB distribution; dedicated environment sized for roughly 0.5 TiB logical data plus safety margin.
- `large-object`: boundary sizes plus at least one multi-gigabyte object.

**Verify**: dry-run prints deterministic estimates and aborts a deliberately under-capacity configuration before generating data.

### Step 2: Verify the complete user workflow

For each applicable profile: initialize, install mode, track, commit, push, verify remote ref and object counts, skip-smudge clone, fetch include/exclude, checkout, pull, status, ls-files, Crab fsck, Git LFS fsck, prune dry-run, and byte-compare sampled/all content. Record every command exit status and duration.

**Verify**: evidence schema has no unchecked step and all content/ref comparisons pass.

### Step 3: Add failure and concurrency qualification

Inject interruption during multipart upload/download, process kill and resume, corrupt local/remote object, missing object, wrong size, throttling, timeout, transient 5xx, denied auth, cancellation, simultaneous linked-worktree same-OID fetch, multiple custom-agent processes, conflicting lock, expiry/relock, and stale unlock. Assert error text identifies the failed boundary and no ref publishes after dependency failure.

**Verify**: each scenario has a named expected failure and a successful recovery or explicit non-retryable result.

### Step 4: Establish SLOs and regression thresholds

Measure throughput, time to first progress, p50/p95 object latency, retries, remote GET/HEAD/PUT counts and bytes, peak RSS, peak active tasks/bytes, scan count, and ref-publication latency. Set initial thresholds from three clean runs with variance margin. Hard gates: memory does not scale with object size/count; already-published push performs zero payload GET bytes; one reachability scan per push.

**Verify**: a deliberately regressed fixture breaches each hard gate and fails the harness.

### Step 5: Add CI and release evidence

Run smoke/failure on relevant pull requests with ephemeral pinned RustFS. Run scale-safe on schedule/manual dispatch. Run full-scale and provider matrix in a dedicated environment before a release candidate. Upload redacted logs, environment manifest, metrics, and checksums; require a passing evidence artifact in release workflow.

**Verify**: workflow validates artifact schema and fails closed when the artifact is missing, stale, from another commit, or incomplete.

### Step 6: Consolidate maintenance paths and finalize docs

Prove callers with `rg`; remove uncalled duplicate lifecycle prune/fsck logic or narrow the module to policy generation. Sanitize `crab lfs env` URLs/userinfo/query values and ensure protocol errors never log raw event bodies. Document canonical local prune/fsck, remote receipt audit, recovery, capacity planning, metrics, and support matrix. Promote a compatibility profile only if its release evidence passes.

**Verify**: `rg -n "lifecycle::run_prune|lifecycle::run_fsck" crab/src` returns no matches and no dead duplicate implementation remains; docs links pass.

## Test plan

- PR: deterministic smoke and representative failure cases on pinned ephemeral RustFS.
- Scheduled: scale-safe, large-object, linked-Worktree, named-remote, locking, migration, and managed profiles.
- Release: full-scale capacity-qualified soak, supported OS binaries, provider matrix where credentials exist, and HTTP profile when shipped.
- Every profile validates refs, SHA-256/size, request/byte counters, peak resources, recovery outcome, and secret redaction in one schema-bound artifact.

## Acceptance criteria

- [ ] Smoke, scale-safe, large-object, failure, and locking profiles produce validated evidence.
- [ ] Full 100,000-object/1–10 MiB profile passes in a capacity-qualified dedicated environment before production replacement claim.
- [ ] Peak transfer memory is bounded; no one-task-per-object or full-payload retention appears in metrics.
- [ ] Already-published push performs one pointer scan and zero payload GET bytes.
- [ ] Push dependency failures never publish refs.
- [ ] Linux, macOS, and Windows supported profiles pass; managed and HTTP profiles use official Git LFS when Phase 6 ships.
- [ ] Release workflow requires evidence bound to the exact commit.
- [ ] Documentation claims exactly the profiles certified by evidence.
- [ ] Diagnostic and qualification artifacts contain no URL credentials, authorization headers, action URLs, raw malformed protocol bodies, or local secret paths.

## STOP conditions

- Estimated local or object-store capacity is below the profile safety margin.
- A test would touch a shared bucket prefix or invoke bucket-wide GC.
- Credentials would be printed or included in artifacts.
- The release gate cannot bind evidence to exact source commit and dependency versions.
- Failures are “fixed” by loosening expected results instead of correcting source behavior.

## Maintenance notes

Pin RustFS images and record provider versions. Rebaseline soft performance thresholds only with multiple clean runs and reviewer approval; never weaken integrity, memory, scan-count, or ref-publication hard gates.
