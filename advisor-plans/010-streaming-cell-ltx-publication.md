# Bounded-memory Cell LTX publication and bundle ingestion

Status: IN PROGRESS — native/bundle bounded sources and shared authenticated inspection pass local and RustFS suites; measured multi-GiB RSS and injected provider-failure qualification remain
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

The final search must have no whole-segment hit in Cell publication. Any
remaining `read_to_end` must be small, statically bounded, and justified next
to the code and in the PR evidence.

## Acceptance criteria

- [x] Native and bundled Cell publication share one streaming integrity path.
- [ ] Peak working memory is bounded by configured/static chunk concurrency,
      not total transaction or bundle size, with measured evidence.
- [x] No full segment body is retained in `AppendInput`-like collections or
      copied from a bundle range.
- [x] Retry uses replayable verified sources and preserves exact bytes.
- [ ] Every injected failure/cancellation cleans owned scratch state and never
      publishes an incomplete root.
- [ ] Multi-GiB publish, source deletion, restore, and checksum comparison pass
      in dedicated qualification.
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
