# Plan 009: Scale refs without splitting the atomic commit root

> **Executor instructions**: Execute only if Plan 001 evidence shows manifest
> size or CAS conflict rate misses an explicit ref-count/writer-QPS target. Keep
> `{repo}/manifest` as the single CAS linearization point.
>
> **Drift check (run first)**:
> `git diff --stat 1f9dae74..HEAD -- crates/crab-metadata/src/manifests.rs crates/crab-metadata/src/manifest_store.rs crates/crab-read/src/ref_advertisement.rs crab/src/git/push.rs crab/tests/remote_helper_transcript.rs`

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: HIGH
- **Depends on**: Plans 001 and 002
- **Category**: perf, direction
- **Planned at**: commit `1f9dae74`, 2026-08-19

## Why this matters

PB byte capacity does not automatically imply many writers or refs. Today the
complete ref map lives inline and all pushes CAS one manifest, which is simple
and correct but can grow or contend. If measurement proves a problem, immutable
segmented refs can shrink the mutable root while preserving one atomic CAS;
adding a coordination service or split ref CAS is not justified first.

## Current state

- `crates/crab-metadata/src/manifests.rs:16-50` stores complete `refs` and
  `peeled_refs` inline with Git validation digest.
- `crates/crab-storage/src/layout.rs:114-120` names `{repo}/manifest` the single
  CAS target.
- `crab/src/git/push.rs:6467-6475` records that unified manifest CAS replaced
  the old two-phase manifest/ref update; retry/merge follows after `:6516`.
- `crab/src/git/push.rs:10700-10753` holds per-ref locks across upload and commit.
- `crab/scripts/e2e/run_concurrent_push_smoke.py` covers correctness contention,
  not retained writer-throughput capacity.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Metadata tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-metadata -p crab-read --locked` | all pass |
| Transcript | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab --test remote_helper_transcript --locked` | all pass |
| Concurrency smoke | `python3 crab/scripts/e2e/run_concurrent_push_smoke.py --help` | exit 0 and documented run arguments |

## Scope

**In scope**:

- Plan 001 benchmark harness/ref workload
- `crates/crab-metadata/src/manifests.rs`
- `crates/crab-metadata/src/segmented.rs`
- `crates/crab-metadata/src/manifest_store.rs`
- `crates/crab-read/src/ref_advertisement.rs`
- `crab/src/git/push.rs`
- `crab/tests/remote_helper_transcript.rs`
- concurrent-push smoke tests

**Out of scope**:

- A ref/manifest service.
- Separate per-ref visible CAS targets.
- Weaker multi-ref atomicity.
- Implementation without a recorded failed threshold.

## Git workflow

- Branch: `advisor/009-atomic-root-scale`
- Commit benchmark decision first, then format/read/write/test changes.

## Steps

### Step 1: Prove this phase is needed

Measure encoded manifest bytes, GET/PUT bytes, CAS retries, p95/p99 commit time,
and successful writer throughput over a matrix of refs, ref edit counts, and
concurrent disjoint/same-ref writers. Name thresholds in the benchmark contract.
If current behavior meets them, mark this plan `REJECTED (not currently
needed)` and stop.

**Verify**: retained artifact is reproducible and records the first failed
threshold and dominant cost.

### Step 2: Externalize the complete ref state immutably

Reuse the existing segmented immutable metadata pattern. Add a canonical
ref-tree/root object containing complete refs, peeled refs, HEAD, counts, and
digest. The mutable manifest contains that root hash plus the other existing
state roots and a Git validation digest committing to the ref-root identity.
Do not make individual segment updates visible.

**Verify**: format tests reject missing/duplicate/unsorted refs, wrong HEAD,
invalid OIDs, missing segments, and digest mismatch.

### Step 3: Preserve merge/retry and one CAS

Push reads the current ref root, applies the edit set with existing expected-old
semantics, writes immutable ref nodes/root, seals Git validation, and CASes the
single manifest. On CAS conflict it reloads the new root, revalidates edits,
and retries. Multi-ref edits are visible together or not at all.

**Verify**: transcript tests prove atomic multi-ref success/failure, disjoint
writer merge, same-ref rejection/serialization, and no second mutable CAS.

### Step 4: Re-run the failed threshold

Use identical reference hardware/workload. The mutable root size must be
bounded independent of total refs, and the failed target must pass without
regressing small-repo p95 beyond the approved budget.

**Verify**: before/after retained artifact with correctness digest and CAS
request counts.

## Test plan

- Canonical segmented ref-root encoding and malformed-root rejection.
- Atomic multi-ref update and expected-old mismatch.
- Concurrent disjoint-ref merge and same-ref contention.
- CAS conflict replay without lost updates or split visibility.
- Small and high-cardinality ref advertisements from a fresh reader.
- Before/after size, request-count, latency, and throughput benchmark.

## Done criteria

- [ ] A retained failed threshold justified implementation, or plan is rejected.
- [ ] Complete ref state is immutable and content-addressed.
- [ ] `{repo}/manifest` remains the sole mutable CAS root.
- [ ] Multi-ref atomicity and expected-old behavior remain intact.
- [ ] Failed target passes and small-repo regression stays within budget.

## STOP conditions

- No current threshold fails.
- The fix requires independently visible ref CAS operations.
- Ref-root validation needs directory listing.
- A coordination service is proposed before the segmented-root result is
  measured.

## Maintenance notes

Writer QPS is an independent product dimension. Do not claim this phase is
required for PB byte support unless the release profile includes the measured
writer/ref workload.
