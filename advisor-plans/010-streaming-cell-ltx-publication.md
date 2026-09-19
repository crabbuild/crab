# Bounded-memory Cell LTX publication and bundle ingestion

Status: IN PROGRESS — native/bundle bounded sources and shared authenticated inspection pass local and RustFS suites; decoder trailer/index reads are now capped at the 64 KiB inspection chunk and the CellReplica source/scratch/upload path has a measured 8 MiB multipart/1 MiB scratch-transfer test; injected immutable-provider failure and throttled-upload cancellation prove exact retry and scratch cleanup; the documented 5 GiB RustFS native receipt now passes source deletion, exact restore, full compaction, and checksum comparison with timed RSS; broader provider-failure qualification remains
Priority: P0
Effort: XL
Risk: High
Planned against: `4a77b6f1252a` (2026-09-17)
Design authority: `crates/crab-cell-runtime/docs/canonical-ltx-scaling.md`
Dependency: plan 005's coordination kernel; independent of routing plans

## Executor instructions

Implement on `codex/010-streaming-cell-ltx`. Read all of `cell_replica.rs`,
`cell_replica/`, the host/storage traits and implementations, bundle decoder,
publication caller, examples, and LTX tests before editing. Verify locked
`object_store` streaming/multipart contracts from source; do not guess their
buffering or retry behavior. Keep APIs narrow and delete replaced whole-buffer
paths.

## Drift check

```bash
git fetch origin main
git diff --stat 4a77b6f1252a..origin/main -- \
  crates/crab-ltx/src/cell_replica.rs \
  crates/crab-ltx/src/cell_replica \
  crates/crab-ltx/src/bundle* \
  crates/crab-ltx/tests \
  crates/crab-cell-runtime/src/publication.rs
```

Re-audit every `Vec`, `to_vec`, `read_to_end`, and whole-object `get` in the
changed call graph. Stop if the storage contract cannot replay/retry a stream
without violating exact-byte publication.

## Why this plan exists

Native captured publication reads each segment into a `Vec`, `prepare_append`
retains all bodies and builds all indexes, then upload happens later. Bundle
ingestion calls `bundle.segment(index)?.to_vec()`. Peak memory therefore scales
with transaction/bundle size and concurrent Cells, defeating predictable node
admission.

## Required invariants

- Published bytes, hashes, transaction IDs, checksums, sequence, and exact root
  remain identical to current semantics.
- Verification precedes authoritative root publication.
- Retry/reconciliation never substitutes a different body for the expected
  immutable object.
- Scratch files/streams are owned, bounded, and cleaned on every exit path.
- Cancellation cannot leave a published control referencing missing content.

## Scope

- Stream verification/index construction from captured segment files.
- Stream immutable object upload from replayable local scratch/file sources.
- Read bundle headers/ranges without copying full segment bodies.
- Preserve current public entry points where they remain useful; remove private
  body-owning helpers once callers migrate.
- Add memory, failure, cancellation, and exact-recovery evidence.

## Out of scope

- Changing the LTX or bundle wire format.
- New compression or multipart policy knobs.
- Directory-cache persistence.
- Standalone replication removal.

## Implementation steps

1. Measure the current ownership graph. Document every full-body allocation in
   native capture and bundle paths, including retry lifetime.
2. Introduce one internal replayable segment source abstraction backed by an
   owned file/range. It must expose length, bounded reads, cleanup ownership,
   and a fresh reader for retry without renaming fields only.
3. Convert native preparation into a streaming pass that validates header,
   frame/page checksums, digest, and index entries while writing/retaining at
   most the bounded scratch representation. Do not collect all segment bodies
   or indexes simultaneously when they can be emitted incrementally.
4. Upload immutable objects through verified streaming/multipart facilities.
   Inspect and test provider retry semantics. If a retry requires rewind,
   reopen the immutable verified source; never buffer the body as convenience.
5. Replace bundle `to_vec` with bounded range readers and feed the same
   verifier/uploader. One verifier should own native and bundled segment
   integrity rules.
6. Inject failures at read, verify, part upload, completion, root CAS, and
   cancellation. Prove scratch cleanup, immutable idempotency, and exact-root
   reconciliation.
7. Add a deterministic allocation/RSS qualification harness. Unit tests use a
   sufficiently large sparse/generated segment to prove peak working memory is
   bounded independently of body size; dedicated qualification publishes and
   restores a multi-GiB transaction/bundle.
8. Update `SCALABILITY.md` with measured command, body size, concurrency,
   allocator/RSS method, and result. Do not claim a bound from code inspection.

## Verification

```bash
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-010-streaming-ltx \
  cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-010-streaming-ltx \
  cargo test -p crab-cell-runtime publication --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-010-streaming-ltx \
  cargo test -p crab-http-server --locked
CARGO_TARGET_DIR=$HOME/Workspace/crabbuild-target/crab-010-streaming-ltx \
  cargo clippy -p crab-ltx -p crab-cell-runtime -p crab-http-server \
  --all-targets --locked -- -D warnings
cargo fmt --all -- --check
node crates/crab-cell-runtime/docs/validate.mjs
rg -n "bundle\.segment\(.*\)\?\.to_vec|read_to_end" crates/crab-ltx/src
git diff --check
```

The final search must have no whole-segment hit in the canonical CellReplica
publication call graph. Any remaining `read_to_end` must be small, statically
bounded, and justified next to the code and in the PR evidence. The former
standalone `crab-ltx::Replica` bundle path is now historical audit context only;
Plan 017 hard-removed its callable surface after migrating the unique proof to
CellReplica. Cell-scoped bundle preparation is the only supported path.

The local provider-failure qualification uses a fail-first `ObjectStore` wrapper
in `crates/crab-ltx/tests/capabilities.rs`. It rejects one immutable PUT,
verifies that no head was published, retries the same captured source, and
restores the resulting head. This is a deterministic failure seam, not a
substitute for the provider matrix or multi-GiB/RSS receipt.

Before the hard removal of the standalone replication examples, the
`rustfs_replication_scale_load 10m` harness ran against RustFS 1.0.0-rc.1 in a
fresh isolated bucket. It published 10,000,000 rows as 200 immutable segments,
produced an 838,262,784-byte SQLite source, verified 42,234,991,936 logical
object bytes after exact restore, and completed with 46,993 records/second load
throughput (212.797 seconds wall time; 25.619 seconds restore verification).
This retained historical receipt is not a current runnable surface; the
CellReplica scale harness below owns new provider-scale publication evidence.
It does not measure peak RSS or prove the bounded-memory acceptance item, so the
multi-GiB/RSS checkbox remains open for this historical run.

On 2026-09-18 the canonical release `rustfs_cell_replica_scale_load` example ran
against RustFS 1.0.0-rc.1 with a 5,368,709,120-byte incompressible source grown
through 160 bounded captures (320 immutable segments). It deleted the source,
restored the published root, compacted the complete range, restored the compacted
root, and matched the source BLAKE3/length exactly. `/usr/bin/time -l` recorded
1,496.96 seconds wall time and 592,805,888 bytes maximum resident set size
(~565 MiB); the largest observed compaction scratch LTX was about 5.1 GiB on the
external qualification volume. The receipt proves the canonical native path at
the design target; provider matrices beyond this RustFS run remain open.

## Acceptance criteria

- [x] Native and bundled Cell publication share one streaming integrity path.
- [x] Peak working memory is bounded by configured/static chunk concurrency,
      not total transaction or bundle size, with measured evidence from the
      5 GiB native receipt (592,805,888-byte maximum RSS while streaming a
      5.1 GiB compaction output).
- [x] No full segment body is retained in `AppendInput`-like collections or
      copied from a bundle range on the canonical `CellReplica` path. The
      legacy standalone `Replica` bundle-copy path was removed by Plan 017 and
      is not a runtime or qualification surface.
- [x] Retry uses replayable verified sources and preserves exact bytes.
- [x] Every locally injected failure/cancellation cleans owned scratch state
      and never publishes an incomplete root. Filesystem-fault coverage remains
      in `host_hooks.rs`; the
      `cancelled_cell_prepare_releases_scratch_without_publishing_a_root` test
      cancels a throttled immutable upload and verifies scratch permits/files
      are released. Provider-wide fault matrices remain Plan 015 evidence.
- [x] Multi-GiB publish, source deletion, restore, and checksum comparison pass
      in dedicated qualification; the 5 GiB receipt also restores the full
      compacted root and records wall/RSS/scratch measurements above.
- [x] Existing native/bundle recovery and lost-CAS tests pass.
- [x] No wire/storage format or public compatibility fallback is added.

## Stop conditions

- Locked storage APIs cannot provide a bounded replayable upload contract.
- Exact verification would require reading provider content twice without a
  bounded/local source; redesign explicitly rather than hide the cost.
- Memory evidence cannot separate allocator noise from body-size growth.
- The change would combine standalone removal with streaming.

## Maintenance note

Future Cell publication paths must consume the same verified streaming source.
A convenience API that accepts an unbounded `Vec<u8>` must not become the
canonical internal path again.
