# Phase 2: Route every LFS transfer through one bounded coordinator

> **Executor instructions**: Delete duplicate transfer policy after callers move. Do not add a compatibility wrapper without a shipped contract. Update the Phase 2 row when complete.
>
> **Drift check (run first)**: `git diff --stat 2cbd0d92..HEAD -- crab/src/lfs/batch.rs crab/src/lfs/transfer_agent.rs crab/src/lfs/publication.rs crab/src/cmd/lfs/fetch.rs crab/src/cmd/lfs/push.rs crab/src/cmd/lfs/filter_process.rs crab/src/git/filter_process.rs crab/src/lfs/config.rs`

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: Phase 1
- **Category**: architecture, perf
- **Planned at**: commit `2cbd0d92`, 2026-08-25

## Why this matters

Batch CLI, custom agent, and publication currently implement different concurrency, retry, integrity, progress, and error behavior. One canonical coordinator makes resource limits enforceable and prevents fixes from landing on only one transfer surface.

## Current state

- `crab/src/lfs/batch.rs:150` spawns one Tokio task per pointer and retains all handles.
- `crab/src/lfs/transfer_agent.rs:141` also retains one handle per event until terminate.
- `crab/src/lfs/batch.rs:251` derives concurrency from a fixed 1 MiB object-size heuristic.
- `crab/src/lfs/config.rs:25` has configured concurrency, retries, retry cap, skip-errors, and bandwidth, but `crab/src/lfs/transfer_agent.rs:129` uses a separate fixed three-attempt policy.
- `crab/src/lfs/config.rs:72` manually traverses selected files, misses Git include/conditional/XDG/worktree behavior, and currently lets `.lfsconfig` override Git config contrary to official Git LFS precedence.
- `crab/src/cmd/lfs/filter_process.rs:32` converts remote/config errors to `None`; required non-lazy smudge then returns a pointer successfully at `crab/src/git/filter_process.rs:1234` and `crab/src/git/filter_process.rs:1320`.
- Current Git LFS custom-agent protocol is serial per process; `concurrent=true` causes multiple agent processes. Internal unbounded task creation does not provide protocol pipelining.
- `crates/crab-storage/src/head_batch.rs:109` is an existing bounded metadata-operation pattern.

## Commands you will need

| Purpose | Command | Expected |
|---------|---------|----------|
| LFS tests | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo test -p crab --lib lfs --locked --no-default-features` | all pass |
| Clippy | `CARGO_TARGET_DIR="/Volumes/Workspace/crabbuild-target/crab-lfs-$(basename "$PWD")" cargo clippy -p crab --all-targets --no-default-features --locked -- -D warnings` | exit 0 |

## Scope

**In scope**:
- a canonical coordinator module under `crab/src/lfs/`
- `crab/src/lfs/batch.rs`
- `crab/src/lfs/transfer_agent.rs`
- `crab/src/lfs/publication.rs`
- `crab/src/lfs/config.rs`
- `crab/src/cmd/lfs/filter_process.rs`
- `crab/src/cmd/lfs/transfer_agent.rs`
- LFS branches of `crab/src/git/filter_process.rs`
- `crab/src/cmd/lfs/standalone.rs`
- `crab/src/cmd/lfs/install.rs`
- focused caller tests

**Out of scope**:
- Git pointer discovery; Phase 3 owns it
- remote receipt format; Phase 4 owns it
- speculative pipelined custom-agent extensions
- provider-specific tuning flags

## Git workflow

- Branch: `advisor/lfs-phase-2-coordinator`
- Commit in canonical-path order: add engine/tests, move callers, delete duplicate helpers.

## Steps

### Step 1: Define transfer requests and outcomes

Use the smallest useful types: direction, OID, expected size, source/destination, and operation policy. Outcomes must distinguish transferred, already valid, skipped by policy, and failed. Do not return payload bytes.

**Verify**: public types are local to `crab` unless a current shared-crate caller needs them; `rg` shows all fields consumed.

### Step 2: Implement bounded scheduling

Use a bounded queue or `FuturesUnordered` that creates at most configured active work plus a small fixed prefetch. Enforce both object concurrency and in-flight byte permits based on declared size, with one permit for an object larger than the budget. On first fatal error, stop admitting new work, cancel safely, drain active work, and return the first causal error.

**Verify**: a 100,000-request synthetic test asserts task/queue count and bytes remain within configured bounds.

### Step 3: Centralize retry and progress policy

Resolve `LfsConfig` once per operation. Apply retryability classifications, `transfer_max_retries`, capped jittered backoff, skip-download-errors, and actual byte-rate limiting in one place. Never retry hash/size mismatch or local configuration errors. Emit structured progress events through a callback/channel so CLI and JSON protocol formatting remain adapters.

**Verify**: deterministic paused-time tests assert retry counts, delay cap, non-retryable errors, and progress monotonicity.

### Step 4: Resolve configuration through Git and fail required smudge closed

Use Git as the canonical config resolver so includes, conditional includes, command overrides, XDG files, and linked-worktree config participate with official precedence. Produce one immutable typed policy. Reject unsupported parsed keys or implement them; do not silently accept inert settings. If the LFS filter is required and lazy/skip/fetch-exclusion/`skipdownloaderrors` is not explicit, missing/auth/config/download errors must return a filter protocol error rather than the pointer.

Change installation so Crab's standalone transfer selection is scoped to the selected Crab repository/URL instead of an unconditional global `lfs.standalonetransferagent`. Provide a doctor/update migration that identifies and removes the old global interception only after installing scoped configuration; preserve unrelated Git LFS repositories.

**Verify**: tests cover system/global/XDG/include/conditional/local/worktree/command precedence, plus missing object, denied auth, malformed remote, explicit skip-smudge, fetch exclusion, and `skipdownloaderrors`.

### Step 5: Move all callers and delete duplicate paths

Make BatchResolver a discovery/thin-adapter surface or remove it if no longer useful. The custom-agent entry point must read and validate the `init` event before resolving operation- and named-remote-specific storage/auth. Make each current-protocol request submit one coordinator operation and wait for one result. Move standalone and process-filter smudge to the same verified cache/streaming path. Make publication call the same upload operation. Remove fixed retry policy, semaphore loops, handle vectors, and duplicate full-object helpers.

**Verify**: `rg -n "Vec<.*JoinHandle|Semaphore::new|TRANSFER_RETRY_POLICY|effective_concurrency" crab/src/lfs` finds no duplicate transfer scheduler outside coordinator tests.

### Step 6: Make the custom-agent session strict and terminable

Implement an explicit state machine: exactly one init; operation must match upload/download events; malformed JSON and unknown/repeated/out-of-order events are protocol errors; reader/output/join failures propagate. The input reader must stop after consuming `terminate` instead of relying on aborting an already-running `spawn_blocking` task. Log bounded event metadata, never raw protocol lines.

**Verify**: transcript tests cover duplicate init, wrong direction, malformed JSON, unknown event, terminate with open stdin, reader error, output error, and task error; every test terminates within a fixed timeout.

### Step 7: Add operation metrics

Record direction, requested/transferred/skipped/failed counts, logical and wire bytes, retry count, queue wait, transfer duration, integrity duration, and peak active objects/bytes. Do not include paths, OIDs, credentials, or signed URLs in default metrics.

**Verify**: unit tests assert metric counters for success, skip, retry-success, and failure.

## Test plan

- Table-drive coordinator tests with a counting fake store and paused Tokio time.
- Cover 100,000 queued requests, one object larger than byte budget, cancellation, retryable/non-retryable failures, skip-errors, rate limiting, and progress/output errors.
- Add process-level transcript fixtures for selected remote, init state, wrong direction, malformed input, open-stdin terminate, and stdout contamination.
- Add config/install fixtures for includes, conditional includes, Linked worktrees, URL scoping, and migration from the old global key.

## Acceptance criteria

- [ ] Every upload/download entry point uses one transfer coordinator.
- [ ] Active tasks and in-flight payload bytes are bounded independently of request count.
- [ ] Git LFS config controls retries/concurrency/bandwidth consistently.
- [ ] Custom-agent output remains valid JSON-lines with one completion per request.
- [ ] Init selects the requested named remote and operation-specific read/write authorization before any transfer.
- [ ] Malformed/out-of-order protocol input fails deterministically and terminate never hangs on open stdin.
- [ ] Required non-lazy smudge fails closed; only explicit lazy/error policy preserves a pointer.
- [ ] Git configuration precedence, includes, and linked-worktree scope match the official contract.
- [ ] Default install/update cannot redirect an unrelated non-Crab Git LFS repository.
- [ ] Cancellation and first-error behavior are deterministic and tested.
- [ ] Duplicate scheduler/retry helpers are deleted.

## STOP conditions

- Phase 1 streaming APIs are absent or still return whole large objects.
- A caller needs materially different integrity semantics; report the owner-boundary conflict before adding a mode flag.
- Git LFS current protocol behavior differs from the cited official custom-transfer documentation.
- Metrics would expose repository paths, OIDs, credentials, or URLs by default.

## Maintenance notes

Concurrency changes must be reviewed at the process level: Git LFS may spawn multiple Crab agents. Per-process limits therefore need a documented operational multiplier.
