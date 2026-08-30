# Plan 007: Give the cache service a PB classification profile

> **Executor instructions**: Optimize the optional service after serverless
> correctness exists. Preserve tenant/dedup scope and proof validation. Service
> outage must fall back to Plan 006's origin path.
>
> **Drift check (run first)**:
> `git diff --stat 1f9dae74..HEAD -- crates/crab-cache-server/src crates/crab-cache/src crab/src/cmd/add_push_plan.rs`

## Status

- **Priority**: P2
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: Plans 003 and 006
- **Category**: perf
- **Planned at**: commit `1f9dae74`, 2026-08-19

## Why this matters

The endpoint already accepts large batches, but its backing index is one
mutex-protected SQLite connection and performs one query per hash. That cannot
meet multi-million-chunk classification targets or keep async request threads
healthy. A partitioned, truly batched local index and generation-aware negative
cache can accelerate PB clients without becoming authoritative.

## Current state

- `crates/crab-cache-server/src/handlers.rs:781-842` enforces auth/dedup scope
  and a 100,000-hash request limit.
- `crates/crab-cache-server/src/chunk_index.rs:26-89` owns
  `Mutex<Connection>` and prepares one `SELECT` per chunk hash.
- `crates/crab-cache-server/src/db.rs` defines the current SQLite schema.
- Plan 006 owns safe fallback: service errors classify bytes as unknown and pack
  locally; the service never writes authoritative origin metadata.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Cache tests | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo test -p crab-cache-server -p crab-cache --all-features --locked` | all pass |
| Lint | `CARGO_TARGET_DIR=/Volumes/Workspace/crabbuild-target/crab-main cargo clippy -p crab-cache-server -p crab-cache --all-features --locked -- -D warnings` | exit 0 |

## Scope

**In scope**:

- `crates/crab-cache-server/src/chunk_index.rs`
- `crates/crab-cache-server/src/db.rs`
- `crates/crab-cache-server/src/handlers.rs`
- `crates/crab-cache/src/cache_client.rs`
- `crates/crab-cache/src/service.rs`
- cache metrics/config tests and benchmark harness

**Out of scope**:

- Origin metadata writes or ref commit.
- Raising limits without a measured memory budget.
- Cross-tenant existence leakage.
- Making cache availability a push requirement.

## Git workflow

- Branch: `advisor/007-cache-pb-profile`
- Separate storage/query, protocol, and performance-evidence commits.

## Steps

### Step 1: Partition the local index and remove async blocking

Partition by the same hash bits as the remote layout/dedup scope. Use a bounded
connection pool or per-partition worker; execute SQLite work in
`spawn_blocking`. Batch queries with temporary tables or bounded `IN` groups,
not one prepared query per hash. Preserve input order and duplicates.

**Verify**: concurrency test keeps Tokio heartbeat latency below the documented
test bound while 100,000-hash queries run; query instrumentation shows
O(partitions × bounded groups), not O(hashes), statements.

### Step 2: Add generation-aware negatives and proof-bearing hits

Key negative entries by partition generation/GC generation and dedup scope.
Invalidate on generation change. Hits include enough origin/cache proof for
Plan 006 validation; a cache-local proof is accepted only in `cache+dedup` mode
under existing policy. Unknown/stale proof returns unknown, not an invented hit.

**Verify**: tests cover generation bump, stale negative, scope mismatch, proof
rejection, and service restart/rebuild.

### Step 3: Stream bounded responses

Keep request maximum 100,000 unless benchmarks justify change. Stream response
frames with indexes/ranges so clients reconstruct caller order without holding
the full encoded response. Enforce decoded byte/count limits before allocation
and cancellation/backpressure during send.

**Verify**: protocol tests cover partial frames, disconnect, malformed index,
oversize request, duplicates, and exact ordered reconstruction.

### Step 4: Retain the performance gate

Benchmark cold/warm index, hit ratios, 1/16/100 concurrent clients, and origin
verification fallback on reference hardware. Required release target from the
PB contract: at least 5 million classifications/minute for the 10 TB synthetic
dataset, with peak RSS and p95/p99 latency retained.

**Verify**: retained artifact meets the target or the plan remains BLOCKED with
the measured bottleneck; never tune by hiding failed/error results.

## Test plan

- Ordered 100,000-hash batches with duplicates across many partitions.
- SQL statement-count and Tokio heartbeat instrumentation.
- Tenant/dedup-scope rejection and proof-mode matrix.
- Negative-cache invalidation on partition/GC generation changes.
- Streaming frame truncation, reordering, oversize, disconnect, and retry.
- Service outage proving the client falls back to local packing.

## Done criteria

- [ ] No per-hash SQL loop or blocking SQLite mutex runs on async workers.
- [ ] Tenant/dedup scope and proof policy have negative tests.
- [ ] Negative results invalidate by partition generation.
- [ ] 100,000-result response is bounded/streamed and order-correct.
- [ ] Service outage still completes add/push through serverless fallback.
- [ ] Retained benchmark meets or explicitly blocks the throughput gate.

## STOP conditions

- The service needs origin write credentials to answer classification.
- Partition generation cannot be observed without changing the origin contract.
- Optimizing requires weakening dedup-scope enforcement.
- Benchmark target cannot be reproduced from the recorded command/environment.

## Maintenance notes

The throughput SLO may make this service operationally required for PB
performance, but never for correctness. Dashboards must separate cache hits,
origin-verified hits, unknown fallback, and rejected proofs.
