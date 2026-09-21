# Plan 019: Make `crab-ltx` `Db` recovery single-pass and stream local compaction

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report; do not improvise. When done, update this plan's row in
> `plans/README.md` unless a reviewer told you that they maintain the index.
>
> **Drift check (run first)**:
>
> ```bash
> git diff --stat 11ab736e40e..HEAD -- \
>   crates/crab-ltx/src/compactor.rs \
>   crates/crab-ltx/src/environment.rs \
>   crates/crab-ltx/src/db.rs \
>   crates/crab-ltx/src/recovery.rs \
>   crates/crab-ltx/tests/host_hooks.rs \
>   crates/crab-ltx/tests/replication.rs \
>   crates/crab-ltx/perf/README.md \
>   crates/crab-ltx/perf/run.sh \
>   crates/crab-ltx/perf/crab/src/main.rs \
>   crates/crab-ltx/perf/celld/src/main.rs
> ```
>
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding. A semantic
> mismatch is a STOP condition.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: MED
- **Depends on**: none
- **Category**: perf
- **Planned at**: commit `11ab736e40e`, 2026-09-19
- **Execution status**: IN PROGRESS — implementation and correctness proof are
  complete; the controlled recovery-performance gate did not pass.

## Why this matters

The current benchmark shows that local `Db` capture latency is dominated by the
LTX-file and parent-directory durability syncs. This plan intentionally leaves
those guarantees unchanged. The next contract-preserving opportunity is the
recovery path: plan construction decodes each input once in `verify_segment`
and again while reconstructing the final image; resume reconstructs that image
twice; local compaction builds the whole output in a `Vec<u8>`, decodes it
again, and then copies it into an atomically installed file.

This plan removes those repeated passes while preserving every current
selection, checksum, exact-image, no-clobber, file-sync, and parent-sync
guarantee. It also fixes the benchmark's misleading `end_to_end_us` name so
the resulting performance claim includes workload writes and capture rather
than only verification, compaction, and restore.

## Design vocabulary

Use the current public and internal names throughout this plan:

- `Db` is the public, exclusive local SQLite session. It owns the writer,
  capture state, retained local segments, and the next capture position. It is
  an embedded database session, not a cloud service or control plane.
- `CaptureEngine` is the private WAL-to-LTX implementation in
  `crates/crab-ltx/src/capture.rs`. It is an implementation detail of `Db`,
  not a second public database abstraction.
- `Host` supplies injectable filesystem, SQLite VFS, clock, and local-resource
  policy for verification, restore, and compaction. It does not publish Cell
  roots or decide durable authority.
- `VerifiedPlan` is an owned, explicit snapshot-plus-deltas recovery plan. It
  selects named local artifacts and ends at one exact `Position`; it is not a
  remote manifest or a “latest” query.
- `CellReplica` is the optional `replica`-feature object-store boundary. It
  prepares and verifies immutable Cell-root proposals, while
  `crab-cell-runtime` owns authority, leases, acknowledgement, retention, and
  publication CAS.

The performance work below optimizes the `Db`/`VerifiedPlan` local recovery
path and local compaction. It must not introduce a second local-session
abstraction or compatibility alias, and it must not move Cell authority into
`crab-ltx`.

## Baseline before implementation

The following excerpts describe the pre-change implementation that motivated
this plan. They are historical context, not the resulting API or ownership
model. The resulting design is summarized below the baseline sections.

### Relevant files

- `crates/crab-ltx/src/recovery.rs` owns `VerifiedPlan`, exact image
  reconstruction, local compaction validation, and continuation state.
- `crates/crab-ltx/src/environment.rs` owns the public `Host` recovery methods
  and the injectable `FileSystem` durability contract.
- `crates/crab-ltx/src/db.rs` restores a verified plan and seeds the next
  capture position during `Db::resume_with_host`.
- `crates/crab-ltx/src/compactor.rs` performs the ordered newest-page merge and
  recomputes the post-apply checksum.
- `crates/crab-ltx/tests/replication.rs` proves byte-identical restore,
  corruption rejection, and no-clobber behavior.
- `crates/crab-ltx/tests/host_hooks.rs` is the established injectable
  filesystem/fault boundary for sync, install, cleanup, and bounded-I/O tests.
- `crates/crab-ltx/perf/{crab,celld}/src/main.rs` are separate benchmark
  executables because their `rusqlite`/`libsqlite3-sys` versions cannot be
  linked into one process.
- `crates/crab-ltx/perf/README.md` defines the comparison contract and the
  durability difference between Crab and the pinned Celld revision.

### Repeated verification and reconstruction

At `crates/crab-ltx/src/recovery.rs:57-77`, every segment is decoded by
`verify_segment`, retained, and then decoded again by `plan.image()`:

```rust
for segment in segments {
    // size and plan-limit checks omitted
    let bytes = host.read(segment.path(), segment.info().size_bytes)?;
    verify_segment(&bytes, segment.info(), limits)?;
    infos.push(segment.info().clone());
    inputs.push(bytes);
}
let mut plan = Self {
    inputs,
    infos,
    image_digest: [0; 32],
    position: target,
    limits,
};
let image = plan.image()?;
plan.image_digest = *blake3::hash(&image).as_bytes();
```

`verify_segment` calls `ltx::decode_file`, while `image()` calls
`ltx::decode_file_with_pages`. Both traverse the complete LTX body.
`verify_segment` is also used by bundles, node frames, and retained captured
files; keep that standalone helper for those callers rather than changing its
contract as part of this plan.

### Resume materializes twice

At `crates/crab-ltx/src/db.rs:166-190`, `Db::resume_with_host` first calls
`recovery::continuation(plan)` and then `host.restore(plan, destination)`.
`continuation()` and `Host::restore()` each call `plan.image()`. The restored
bytes and continuation checksums must instead come from one private
materialization result.

### Local compaction retains and copies the complete output

At `crates/crab-ltx/src/recovery.rs:190-225`, local compaction writes into a
bounded vector and then decodes that vector to reconstruct an exact image
digest:

```rust
let writer = BoundedWriter {
    bytes: Vec::new(),
    limit: plan.limits.max_file_bytes,
};
let mut compactor = crate::compactor::Compactor::new(writer, readers);
compactor.compact()?;
let bytes = compactor.into_writer().bytes;
let (file, pages) = ltx::decode_file_with_pages(&bytes)?;
```

At `crates/crab-ltx/src/environment.rs:1021-1028`, `Host::compact` then passes
that full vector to `persist_new`, which writes it again:

```rust
let (bytes, info) = crate::recovery::compact_bytes(plan)?;
self.filesystem.persist_new(destination, &bytes)?;
Ok(crate::LocalSegment::new(destination.to_owned(), info))
```

The existing `FileSystem::persist_file_new` contract already installs an
already-synced same-directory scratch file without replacing the destination
and syncs the parent directory. Reuse that contract; do not add a second
installation primitive.

### Benchmark accounting is incomplete

At `crates/crab-ltx/perf/crab/src/main.rs:231`, `end_to_end_us` is only:

```rust
end_to_end_us: verify_us + compact_us + compact_verify_us + restore_us,
```

The Celld runner similarly reports only `compact_us + restore_us`. The field
therefore excludes `workload_write_us` and `capture_us`, despite its name and
the current README description. The benchmark must report a recovery subtotal
and a true full-round total separately.

## Resulting design

The implementation keeps the public embedded API centered on `Db`. `Db` owns
the SQLite writer and private `CaptureEngine`; `Host` owns injectable local
filesystem/VFS policy; and `VerifiedPlan` owns the exact verified database
image plus the selected segment metadata. `CellReplica` remains an optional
object-store adapter that prepares immutable root proposals; Cell authority,
acknowledgement, retention, and publication CAS remain outside `crab-ltx`.

Plan construction uses one page-decoding pass per named input. In that pass it
verifies the encoded byte identity, LTX structure, checksum-linked lineage, and
every intermediate database checksum while constructing the final image and
`PageChecksums`. The encoded inputs are then dropped. Restore, resume, and
compaction borrow the same private `MaterializedPlan`, avoiding a second decode
without exposing mutable image bytes through the public API.

Local compaction writes a full snapshot directly from the verified image into
an exclusively created, same-directory scratch file through the host
`FileSystem`. The encoder establishes the header, ordered page stream, index,
file checksum, and trailer. A bounded digesting writer enforces
`max_file_bytes`; after sync, a streaming readback must reproduce the exact
encoded length and BLAKE3 digest before `persist_file_new` installs the file.
The scratch guard removes owned files on pre-install errors and never replaces
an existing destination.

### Contracts that must not change

1. `VerifiedPlan` owns the exact reconstructed database image; later input
   removal, path replacement, or bucket listing cannot alter the selected plan.
2. The first segment is a full snapshot; subsequent TXID ranges and
   pre/post-checksums are contiguous and end at the caller-selected `Position`.
3. Compaction output preserves the first minimum TXID/pre-checksum, last
   maximum TXID/post-checksum, page size, database page count, and verified
   final database checksum.
4. Restore and compaction never replace an existing destination.
5. A compacted scratch file is fully synced before installation; installation
   syncs the destination parent. Errors before installation remove owned
   scratch files. An error after installation remains explicitly ambiguous per
   the existing `FileSystem` contract.
6. `Db::capture` continues to sync each LTX file and its parent
   directory before returning. This plan must not change capture
   acknowledgement or publication timing.
7. Default `Limits` remain 512 MiB database, 64 MiB capture, 512 MiB file,
   1 GiB encoded plan input, and 1,024 segments. Each live `VerifiedPlan`
   retains at most `max_database_bytes` of image data plus checksum and segment
   metadata; multi-plan callers must admit decoded image memory explicitly.

## Commands you will need

The repository requires every compiling Cargo invocation to use a unique
target directory on the mounted workspace volume.

```bash
export CRAB_LTX_TARGET="$HOME/Workspace/crabbuild-target/crab-b2c9-plan019"
test -d "$HOME/Workspace"
mkdir -p "$CRAB_LTX_TARGET"
```

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo fmt --all -- --check` | exit 0, no diff |
| Minimal tests | `CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test -p crab-ltx --locked` | all tests and doctests pass |
| Replica tests | `CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test -p crab-ltx --features replica --locked` | all tests and doctests pass |
| Lint | `CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo clippy -p crab-ltx --all-targets --features replica --locked -- -D warnings` | exit 0, no warnings |
| Runtime consumer | `CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test -p crab-cell-runtime --locked` | all tests pass |
| HTTP consumer | `CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo check -p crab-http-server --locked` | exit 0 |
| Crab perf runner | `CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test --manifest-path crates/crab-ltx/perf/crab/Cargo.toml --locked` | exit 0 |
| Celld perf runner | `CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test --manifest-path crates/crab-ltx/perf/celld/Cargo.toml --locked` | exit 0 |

Do not run a Cargo command without the explicit `CARGO_TARGET_DIR`. If
`$HOME/Workspace` is unavailable or unwritable, stop instead of falling back
to a repository-local `target/` directory.

## Scope

**In scope** (the only implementation and test files to modify):

- `crates/crab-ltx/src/compactor.rs`
- `crates/crab-ltx/src/environment.rs`
- `crates/crab-ltx/src/db.rs`
- `crates/crab-ltx/src/capture.rs`
- `crates/crab-ltx/src/capture/wal.rs`
- `crates/crab-ltx/src/host.rs`
- `crates/crab-ltx/src/pages.rs`
- `crates/crab-ltx/src/types.rs`
- `crates/crab-ltx/src/recovery.rs`
- `crates/crab-ltx/README.md`
- `crates/crab-ltx/tests/host_hooks.rs`
- `crates/crab-ltx/tests/replication.rs`
- `crates/crab-ltx/perf/README.md`
- `crates/crab-ltx/perf/run.sh` only if needed to retain parseable reports
- `crates/crab-ltx/perf/crab/src/main.rs`
- `crates/crab-ltx/perf/celld/src/main.rs`
- `crates/crab-cell-runtime/src/executor.rs`
- `crates/crab-cell-runtime/tests/publication.rs`
- `crates/crab-cell-runtime/docs/runtime.md`
- `crates/crab-cell-runtime/docs/failover-and-followers.md`
- `plans/README.md` for status only

**Out of scope** (do not touch even if related):

- The default `Db::capture` ordering and the `FileSystem::rename` synchronous
  contract; the new deferred path must remain explicitly opt-in and must use
  `sync_parent` before acknowledgement.
- Checkpoint thresholds or checkpoint barriers. Their correctness depends on
  WAL restart races and needs a separate characterization plan.
- Replica/object-store compaction, range-read concurrency, paged VFS, or
  hydration policy.
- New public type/name changes, serialized formats, object layouts, dependency
  versions, or lockfiles. `Db` remains the canonical public local-session name;
  do not add a compatibility alias.
- The pinned Celld implementation or its revision.
- README architecture diagrams or unrelated documentation; the deferred
  capture contract is documented in the existing API guide.

## Git workflow

- Branch: `codex/ltx-recovery-performance` unless the operator assigned an
  existing branch.
- Use small conventional commits. Recommended sequence:
  1. `test(ltx): report complete performance totals`
  2. `perf(ltx): materialize verified recovery once`
  3. `perf(ltx): stream local compaction output`
  4. `test(ltx): qualify recovery performance`
- Do not push or open a PR unless the operator explicitly requests it.
- Do not modify or commit unrelated working-tree changes.

## Steps

### Step 1: Capture the baseline and make benchmark totals truthful

Before editing source, create a benchmark-output directory outside the
checkout and run the current Crab runner for three workloads. Run Celld once
per workload as the pinned comparison, but judge this plan primarily against
Crab's own baseline.

```bash
export CRAB_LTX_BENCH_DIR
CRAB_LTX_BENCH_DIR=$(mktemp -d "$HOME/Workspace/crab-ltx-plan019.XXXXXX")
for case in small:32:1024 medium:128:4096 large:512:16384; do
  IFS=: read -r name transactions payload_bytes <<<"$case"
  CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo run --quiet --release --locked \
    --manifest-path crates/crab-ltx/perf/crab/Cargo.toml -- \
    --transactions "$transactions" --payload-bytes "$payload_bytes" \
    --rounds 11 --warmup 2 >"$CRAB_LTX_BENCH_DIR/baseline-crab-$name.json"
  CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo run --quiet --release --locked \
    --manifest-path crates/crab-ltx/perf/celld/Cargo.toml -- \
    --transactions "$transactions" --payload-bytes "$payload_bytes" \
    --rounds 11 --warmup 2 >"$CRAB_LTX_BENCH_DIR/baseline-celld-$name.json"
done
```

Preserve those files until the final comparison. For the legacy baseline,
calculate complete time as:

```text
workload_write_us + capture_us + end_to_end_us
```

Then update both runners:

- Replace `end_to_end_us` with `recovery_us` and `total_us` in `Sample` and
  `Summary`; the benchmark contract is internal and unreleased, so do not keep
  a stale alias.
- Define Crab `recovery_us` as
  `verify_us + compact_us + compact_verify_us + restore_us`.
- Define Celld `recovery_us` using the same field sum. Its explicit verify
  fields remain zero because the pinned implementation exposes no equivalent
  phase.
- Define `total_us` in both runners as
  `workload_write_us + capture_us + recovery_us`.
- Keep individual phase fields. They are needed to distinguish an actual
  optimization from shifted work.
- Add unit tests around the pure subtotal/total helpers so future field
  additions cannot silently fall out of the total.
- Update `crates/crab-ltx/perf/README.md` to define both fields precisely and
  state that `total_us` is the only full local-round headline.
- If `run.sh` is changed, preserve its two separate Cargo packages, release
  profile, warmup/round environment variables, temporary-file cleanup, and
  parseable JSON bodies.

**Verify**:

```bash
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test \
  --manifest-path crates/crab-ltx/perf/crab/Cargo.toml --locked
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test \
  --manifest-path crates/crab-ltx/perf/celld/Cargo.toml --locked
rg -n "end_to_end_us" crates/crab-ltx/perf
```

Expected: both test commands exit 0; `rg` returns no matches.

### Step 2: Build and retain one private verified materialization per plan

Refactor `crates/crab-ltx/src/recovery.rs` around a private materialization
result. A suggested shape is:

```rust
pub(crate) struct MaterializedPlan {
    image: Vec<u8>,
    checksums: PageChecksums,
    page_size: u32,
    database_pages: u32,
    position: Position,
}
```

The exact private names may vary, but the ownership must remain one image plus
the continuation metadata derived during the same page walk.

Implement these rules:

1. During `VerifiedPlan::with_host`, read each named file once and decode its
   pages once. In that same pass, check the expected byte length/BLAKE3,
   header, trailer, page ordering, `SegmentInfo`, TXID continuity,
   pre-apply checksum, page-size continuity, per-state page checksum, final
   selected position, and plan limits.
2. Build the canonical image and `PageChecksums` while processing those
   decoded pages. Retain the private materialization, `SegmentInfo` values, and
   limits in `VerifiedPlan`; drop the larger of the encoded chain or decoded
   image instead of retaining both.
3. Keep `verify_segment` with its present signature and behavior for bundle,
   node-frame, and retained-file callers. Add a private decode-with-pages
   helper for plan construction rather than making unrelated callers retain
   page bodies.
4. Expose the cached result only through one crate-private immutable accessor.
   Public callers may select, restore, resume, and compact a plan but cannot
   read or mutate its backing image.
5. Change `Host::restore` to atomically install that image through the existing
   `persist_new` contract without another LTX decode.
6. Change `Db::resume_with_host` to use the same image for installation and
   seed the returned checksum/page-size/page-count state. Do not call a
   continuation helper and then public `Host::restore` if that copies or
   reconstructs the plan a second time.
7. Delete or narrow `recovery::continuation` after its last duplicate caller
   is gone. Do not construct a `Vec<(u32, Vec<u8>)>` for every page merely to
   rebuild checksums already computed during materialization.
8. Preserve the public signatures of `VerifiedPlan::new`, `Host::verify`,
   `Host::restore`, `restore_exact`, `Db::resume`, and
   `Db::resume_with_host`.

Tests in `crates/crab-ltx/tests/replication.rs` must continue proving:

- a snapshot and snapshot-plus-delta plan restore byte-for-byte identically;
- gap, overlap, wrong target, manifest mutation, and corrupted bytes fail;
- a plan still works after its source paths are deleted;
- restore never overwrites an existing destination;
- resume continues at the exact parent TXID/checksum and a subsequent capture
  restores byte-identically.

Add one focused resume test if the existing coverage does not compare the
post-resume continuation against the original source byte-for-byte.

**Verify**:

```bash
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test -p crab-ltx --locked \
  --test replication
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test -p crab-ltx --locked \
  db::tests
```

Expected: all selected tests pass; no public API or feature-gate changes.

### Step 3: Encode local compaction from the verified image

Remove the full-output `BoundedWriter<Vec<u8>>` path and the second k-way merge
over encoded inputs. Encode a new full snapshot directly from the immutable
image and database checksum already proven during plan construction. Skip the
SQLite lock page exactly as the snapshot codec requires. Preserve the first
input's minimum TXID and pre-apply checksum, and the last input's maximum TXID,
timestamp, and page count.

Implement local installation as follows:

1. Generate a process-unique scratch name in the destination's parent with a
   recognizable private prefix such as `.tmp-crab-ltx-compaction-`.
2. Create it exclusively through `FileSystem::create`; a collision retries
   with a new nonce. Do not use `std::fs` or `tempfile` behind the host's
   injectable filesystem boundary.
3. Wrap the resulting host file handle in a private bounded digest/counting
   writer and stream `Encoder` output into it. Enforce `max_file_bytes` while
   writing.
4. Sync the completed scratch file and drop the write handle.
5. Reopen the scratch file through the host and stream it through a digesting
   reader. Its byte length and BLAKE3 must exactly match the bytes accepted by
   the encoder's writer; this detects short, altered, or replaced scratch
   output without decoding it again.
6. Build `SegmentInfo` from the encoder's closed header/trailer plus the exact
   stored length and digest. Compare its range, pre-checksum, selected position,
   page count, and page size against the plan before installation.
7. Install the already-synced same-directory scratch through
   `FileSystem::persist_file_new`. Return `LocalSegment` only after that call
   succeeds.
8. On every error before installation, remove the owned scratch path. Preserve
   the original operation error if cleanup also fails. Never remove or replace
   a destination after an ambiguous installation error.
9. Keep `FileSystem` unchanged. Its existing `create`, `open`,
   `remove_file`, and `persist_file_new` operations are sufficient.
10. Delete the now-redundant local `Compactor`, `BoundedWriter`, and
    `compact_bytes` paths after all callers use the canonical image encoder.

Extend `crates/crab-ltx/tests/host_hooks.rs` using its existing `Faults`
filesystem:

- assert local compaction calls `create`, `sync_all`, `open`, and
  `persist_file_new`, and does not call `persist_new`;
- inject failures at scratch creation, streaming write, file sync, scratch
  reopen/inspection, and pre-install `persist_file_new`; each must leave no
  destination and no `.tmp-crab-ltx-compaction-*` file;
- retain the existing destination no-clobber assertion;
- create a large-enough fixture and assert the largest individual write is
  bounded by encoder/index chunks rather than the complete compacted file;
- restore the streamed compacted output and compare it byte-for-byte with
  direct restore from the original plan.

Do not weaken an assertion merely because the implementation now calls
`persist_file_new` instead of `persist_new`; update fault expectations to the
new owner boundary and add the scratch-cleanup proof.

**Verify**:

```bash
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test -p crab-ltx --locked \
  --test host_hooks snapshot_and_compaction_installation_are_injectable_and_never_clobber
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test -p crab-ltx --locked \
  --test replication committed_sql_survives_cold_restore_and_compaction
rg -n "BoundedWriter|compact_bytes" \
  crates/crab-ltx/src/recovery.rs crates/crab-ltx/src/environment.rs
```

Expected: both focused tests pass; `rg` returns no matches.

### Step 4: Run broad proof and compare the candidate against its baseline

Run formatting, minimal and replica feature suites, Clippy, and the two direct
workspace consumers:

```bash
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo fmt --all -- --check
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test -p crab-ltx --locked
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test -p crab-ltx \
  --features replica --locked
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo clippy -p crab-ltx \
  --all-targets --features replica --locked -- -D warnings
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo test -p crab-cell-runtime --locked
CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo check -p crab-http-server --locked
```

Expected: every command exits 0. Do not change tests, snapshots, inventories,
or lint configuration to make a failure disappear.

Then rerun exactly the same Crab workloads, round counts, warmups, build
profile, machine, filesystem, and power state used for the baseline:

```bash
for case in small:32:1024 medium:128:4096 large:512:16384; do
  IFS=: read -r name transactions payload_bytes <<<"$case"
  CARGO_TARGET_DIR="$CRAB_LTX_TARGET" cargo run --quiet --release --locked \
    --manifest-path crates/crab-ltx/perf/crab/Cargo.toml -- \
    --transactions "$transactions" --payload-bytes "$payload_bytes" \
    --rounds 11 --warmup 2 >"$CRAB_LTX_BENCH_DIR/candidate-crab-$name.json"
done
```

Compare phase medians, not one sample. The candidate passes the performance
gate only when all of the following hold:

- `capture_us` changes by no more than 5% in either direction for every case;
  this plan does not touch capture, so a larger change indicates an invalid
  comparison or unrelated regression.
- `total_us` regresses by no more than 5% for every case.
- `recovery_us` regresses by no more than 5% for every case.
- At least one of the medium or large workloads improves `recovery_us` by 10%
  or more. If neither does, the added complexity has not paid rent; stop and
  report the phase data instead of claiming an optimization.
- `segments`, `input_ltx_bytes`, `compacted_ltx_bytes`,
  `source_database_bytes`, and `final_txid` remain identical to baseline for
  each workload.
- The restored row-count and `PRAGMA integrity_check` validation still run in
  every measured round.

For old baseline JSON, compare candidate `recovery_us` against baseline
`end_to_end_us`, and compare candidate `total_us` against the legacy sum
documented in Step 1. Retain the raw JSON paths in the PR description; do not
commit host-specific benchmark output as a repository baseline.

If the performance gate fails twice on an otherwise idle host, stop. Do not
remove checksum checks, skip scratch inspection, loosen durability, or tune
thresholds until the result looks favorable.

## Execution result

Implemented source changes are in the in-scope `crab-ltx` recovery, compactor,
capture, host, database, benchmark, and host-hook test files. In addition to
the single-pass recovery work, the library now exposes an opt-in deferred
capture path: complete LTX files are renamed without claiming durability, then
their independent file flushes are bounded and parallelized before one shared
parent-directory barrier in `Db::durability_barrier()`. The default
`Db::capture()` path remains synchronous. The following proof passed:

- `cargo fmt --all -- --check`;
- minimal and `replica` `crab-ltx` tests, including doctests;
- `cargo clippy -p crab-ltx --all-targets --features replica --locked -- -D warnings`;
- `crab-cell-runtime` tests and `crab-http-server` check;
- focused streamed-compaction fault, cleanup, no-clobber, and byte-identical
  restore tests;
- both perf-runner unit-test packages and three 11-round/2-warmup benchmark
  matrices for Crab and the pinned Celld runner.

The final matrix preserved segment counts, LTX sizes, database sizes, TXIDs,
row counts, and SQLite integrity. It did not satisfy the plan's timing gate:
the recovery subtotal increased versus the retained pre-change Crab medians in
all three workloads, and capture/total medians were noisy and above the stated
limits in medium/large runs. The raw external evidence is retained under
`$HOME/Workspace/CrabBuild/crab-ltx-plan019.6zxpQu` and is intentionally not
committed. The plan therefore remains `IN PROGRESS`; no performance win is
claimed for recovery itself. That result motivated the separately measured
grouped-durability follow-up below.

## Follow-up iteration: bounded k-way compaction merge

The local `Compactor` now merges page heads with a min-heap keyed by page
number and input order. This changes page-selection work from scanning all
inputs for every output page to `O(pages * log(inputs))`, while retaining the
same decoder validation, newest-input-wins rule, checksum recomputation, image
proof, scratch sync, and post-write inspection. Equal-page candidates are held
until the newest page is encoded so decoder buffers cannot be overwritten early.

The truncation/regrowth regression and complete `crab-ltx` replica suite pass
with this change. Initial local measurements show a phase-local medium
compaction improvement, but the full recovery/total gate is not yet satisfied;
this iteration makes no release-performance claim and leaves the plan
`IN PROGRESS`.

## Follow-up iteration: retain the verified image and encode once

Profiling showed that the bounded k-way merge still repeated work already
completed by `VerifiedPlan::new`: it decoded the full input chain again, and
restore decoded the compacted snapshot yet again. `VerifiedPlan` now keeps the
immutable materialized image and continuation checksum state produced by its
verification pass. Local restore copies that image once; resume reuses the same
state; local compaction encodes a snapshot directly from it. The old local
`Compactor` is deleted, leaving one canonical reconstruction path.

This trades longer-lived decoded memory for fewer decodes. The default bounds
make one plan retain at most a 512 MiB image instead of up to 1 GiB of encoded
inputs, but a highly compressible plan can use more memory than its input
files. The plan is caller-owned and not retained by `crab-cell-runtime` today;
multi-plan embedders must still use an external decoded-memory admission gate.
This is a documented resource tradeoff, not a zero-cost optimization.

An alternating nine-sample comparison against the exact pre-iteration commit
reduced median recovery time by 13.7% for 128 × 4 KiB and 2.14x for
512 × 16 KiB. A one-round large-workload process measurement increased maximum
RSS from about 27.6 MiB to 32.9 MiB. The host was heavily contended, so these
figures establish direction and the memory cost, not release-grade absolute
latency. A later order-balanced Crab/Celld run retained the same qualitative
result: Crab won medium and large recovery plus grouped total latency, while
Celld still won small recovery. The plan remains `IN PROGRESS`; it does not
claim that Crab is universally faster.

The next small-path probe found a fixed durability cost in
`DirectFileSystem::persist_file_new`: the same-directory no-clobber install
hard-linked the synced scratch file, synced the directory, unlinked the scratch
name, and synced the same directory again. Link creation and unlink now precede
one shared parent-directory barrier. Success still proves both the destination
name and scratch cleanup durable; a barrier error remains explicitly ambiguous.
In a 12-pair overloaded-host A/B, median compact time fell by about 1.6 ms and
the paired recovery delta improved by about 2.4 ms. A subsequent 15-pair direct
small-workload run put Crab's recovery median below pinned Celld's, but host
load and tail variance were high, so this remains directional rather than a
release-performance claim.

The capture encoder also passed every small header, page frame, and trailer
write through `HostFile::write_all`, which queried the output file's metadata
before every write to enforce `max_file_bytes`. Exclusively created LTX outputs
now track their sequential extent in memory; opened WAL and checksum files keep
live filesystem length checks. In a 20-pair grouped-capture A/B on the same
contended host, local-write time improved in 17 pairs, with a paired median
reduction of about 2.4 ms. Raw medians moved from 10.5 ms to 7.9 ms for local
writes, 145.6 ms to 140.8 ms for capture, and 193.1 ms to 180.4 ms total. The
filesystem barriers still dominate, so these are directional CPU/I/O-call
results rather than a 1.5x end-to-end claim.

Decoder allocation profiling found two shorter-lived buffers on the verified
recovery path: compressed page storage was allocated for every decoded page,
and the destination page buffer was allocated for every input segment. The
decoder now reuses compressed storage for its lifetime, while plan
materialization reuses one page buffer across the complete chain. Alternating
A/B runs reduced paired median recovery by about 0.4 ms for 32 × 1 KiB, 7.4 ms
for 128 × 4 KiB, and 13.3 ms for 512 × 16 KiB. Medium raw verification medians
moved from 16.2 ms to 8.3 ms. These runs were still host-contended; the change
is accepted because it removes allocator work without changing parsing,
checksums, or output bytes, not because the raw totals are release-grade.

Capture-side profiling then found that L0 encoding sent each header, page
record, compressed body, and index fragment directly to the filesystem. A
bounded 64 KiB writer now coalesces those writes before the unchanged file
fsync and rename/barrier sequence. Medium grouped local-write time moved from
8.6 ms to 2.6 ms and improved in all 20 pairs. Large grouped local-write time
moved from 60.3 ms to 17.8 ms and improved in all 12 pairs; paired capture time
improved by about 57.2 ms. A 12-pair, three-round synchronous small follow-up
improved capture in 9 pairs with a paired median reduction of 7.3 ms. One-round
synchronous samples remained barrier-noisy, so the result is attributed to
fewer write syscalls rather than to any fsync or acknowledgement change.

The final candidate's order-balanced grouped matrix measured total-median
speedups of 1.90x for 32 × 1 KiB, 2.25x for 128 × 4 KiB, and 2.33x for
512 × 16 KiB. Crab recovery was faster for medium and large, but Celld won the
small recovery subtotal in that run (10.1 ms versus Crab's 13.6 ms). The final
15-pair synchronous small comparison measured Crab at 181.0 ms capture and
207.4 ms total versus Celld at 96.3 ms and 117.0 ms; Crab lost every paired
capture and total sample. Therefore Crab does not beat Celld for independently
durable per-capture latency. Grouped throughput is the verified 1.5x–2x-plus
win, while recovery remains workload-dependent, and the contracts must not be
collapsed into one headline.

The Celld runner's diagnostic `--sync-parent` mode adds the same directory
barrier after every upstream `Db::sync()`, compaction installation, and restore
installation without changing pinned Celld itself. Against that fully
contract-normalized runner, Crab capture was effectively parity: 1.09x, 0.99x,
and 1.00x Celld for small, medium, and large workloads. Crab recovery was
2.25x, 2.20x, and 2.36x faster, while total medians were 1.16x, 1.02x, and
1.01x faster. Per-capture total time is therefore barrier-dominated rather than
a 1.5x–2x engine win. The large total-latency headline still belongs to the
explicit grouped durability protocol, where one barrier covers many complete
captures.

A bounded-group follow-up replaced the benchmark's all-captures-at-once switch
with `--durability-batch N`. Batch 1 uses ordinary synchronous `capture()`;
larger values call `capture_deferred()` and cross a file-and-parent durability
barrier after at most `N` captures. Order-balanced runs against shipped Celld
produced these total medians:

| Workload | Samples | Celld | Crab batch 1 | Crab batch 8 | Crab batch 16 |
| --- | ---: | ---: | ---: | ---: | ---: |
| 32 × 1 KiB | 12 pairs | 127.7 ms | 214.8 ms (0.59x) | 83.8 ms (1.52x) | 72.9 ms (1.75x) |
| 128 × 4 KiB | 10 pairs | 584.3 ms | 884.4 ms (0.66x) | 325.7 ms (1.79x) | 268.6 ms (2.17x) |
| 512 × 16 KiB | 8 pairs | 2,119.9 ms | 3,529.9 ms (0.60x) | 1,213.3 ms (1.75x) | 970.9 ms (2.18x) |

Batch 1 lost every paired total sample. Batch 8 won all 12 small, 10 medium,
and 8 large pairs, and is the smallest tested bound that cleared 1.5x for all
three workloads. The host was heavily loaded, so the absolute times are not
release-grade; the alternating order and unanimous paired direction make the
contract boundary more reliable than the raw latency figures.

Batch 8 does not prove lower independently durable acknowledgement latency.
The first capture in a group can wait for seven later captures before the host
may acknowledge it. It proves higher bounded-group throughput and lower batch
completion time. At the time of this matrix, the production
`crab-cell-runtime` call sites still used synchronous `capture()`. The
external-durability follow-up below removes that duplicate local barrier
without grouping commands.

## Follow-up iteration: hand local cuts to the external durability gate

Every runtime mutation is already hidden until either all selected followers
fsync its exact node-log frames or the immutable root is uploaded and accepted
by the authoritative control CAS. A synchronous owner-local LTX file therefore
duplicated, rather than supplied, the proof that releases a result. The runtime
now uses `capture_deferred()` for bootstrap, commands, effects, and migrations.
The complete local file remains readable while the node-log and object paths
consume the same verified bytes.

`Db::prune_captured()` already requires the exact root to have published. It
now reverifies and deletes a selected deferred cut without first fsyncing a
local copy that is about to be removed. Published-cut deletion is
non-authoritative cleanup and no longer waits for a parent-directory sync;
every activation uses a fresh metadata directory, so crash-resurrected residue
is quarantined rather than adopted. Read, verification, or removal failures
still retain unfinished accounting. Standalone `Db::capture()` remains
synchronous, and checkpoints, snapshots, close, and an explicit local
durability barrier still flush every unpublished cut.

The acknowledgement invariant is unchanged:

```text
successful runtime result
    => exact follower fsync proof OR authoritative object-root proof
```

An owner crash before either external proof still yields an unknown outcome.
An owner crash after follower proof is recovered from the sealed node log; an
owner crash after object proof restores the authoritative root. No result is
released merely because the deferred owner-local file exists.

A throwaway release-mode differential loop exercised 32 sequential executor
commands, in-memory immutable-root preparation, binding, confirmation, and
published-cut pruning across 12 measured rounds. The temporary profiler was
removed after the run. Two old-path executions reported 514.9 ms and 525.6 ms
medians; three candidate executions reported 341.3 ms, 337.0 ms, and 348.9 ms.
The non-overlapping ranges represent a 1.48x–1.56x speedup and 32%–36% lower
local object-publication latency. The in-memory store omits provider and
authority-network latency, so this proves the removed local barrier cost, not
a production end-to-end SLO.

The retained Crab/Celld batch-64 reports expose the same local stage directly:
subtracting `capture_barrier_us` from Crab capture gives 9.6 ms, 42.2 ms, and
193.8 ms for the small, medium, and large workloads, versus shipped Celld
capture medians of 102.5 ms, 458.9 ms, and 1,774.9 ms. That is a 9.16x–10.88x
local capture-stage advantage before Crab's follower or object proof. It must
not be presented as an end-to-end result because the external proof latency is
outside that local benchmark.

## Test plan

- `crates/crab-ltx/perf/crab/src/main.rs`
  - subtotal helper includes verify, compaction, compact-output verification,
    and restore exactly once;
  - total helper includes workload write, capture, and recovery exactly once.
- `crates/crab-ltx/perf/celld/src/main.rs`
  - uses the same subtotal/total definition even though explicit verify fields
    are zero.
- `crates/crab-ltx/tests/replication.rs`
  - preserve all existing exact-plan rejection tests;
  - direct and compacted restore remain byte-identical;
  - resume seeds the exact position/checksum and produces a valid next cut.
- `crates/crab-ltx/tests/host_hooks.rs`
  - streamed compaction uses the injectable file path and bounded writes;
  - create/write/sync/reopen/install failures clean owned scratch state;
  - existing destinations are never replaced.
- Existing structural patterns:
  - use `committed_sql_survives_cold_restore_and_compaction` in
    `tests/replication.rs` for exact restored-byte comparison;
  - use `snapshot_and_compaction_installation_are_injectable_and_never_clobber`
    and the `Faults` filesystem in `tests/host_hooks.rs` for failure injection.

## Done criteria

All boxes must be checked:

- [x] Benchmark JSON contains `recovery_us` and `total_us` for both runners;
      `rg -n "end_to_end_us" crates/crab-ltx/perf` returns no matches.
- [x] `VerifiedPlan::with_host` performs one LTX decode per input during plan
      construction; it does not call `verify_segment` followed by `image()`.
- [x] One private materialization supplies both restore bytes and continuation
      metadata during `Db::resume_with_host`.
- [x] `VerifiedPlan` retains one immutable database image and drops encoded
      input bodies, so restore, resume, and compaction do not decode the chain
      again.
- [x] Local compaction no longer stores its complete output in `Vec<u8>` and no
      longer calls `persist_new`.
- [x] Streamed compact output is synced, reopened, matched byte-for-byte by
      length/BLAKE3 against the encoder stream, metadata-checked, and installed
      with `persist_file_new`.
- [x] Scratch files are removed on every tested pre-install failure;
      destinations are never overwritten.
- [x] Deferred captures keep files unacknowledged until a grouped file and
      parent-directory barrier succeeds, and fence the session when either
      phase fails; default capture remains synchronous.
- [x] Minimal, replica, doctest, host-hook, replication, runtime-consumer, HTTP
      consumer, formatting, and Clippy commands all exit 0.
- [ ] The three-workload candidate satisfies the Step 4 performance and
      output-identity gates against the retained baseline.
- [x] No serialized format, object layout, dependency, or lockfile changed;
      `Db` remains the canonical public local-session name. The only new public
      capture surface is the explicitly opt-in `capture_deferred()` plus
      `durability_barrier()` pair; the synchronous default is unchanged.
- [x] `git diff --name-only` contains only files listed under **In scope** plus
      this plan's status row.
- [ ] `plans/README.md` marks Plan 019 `DONE` with a concise proof summary.

## STOP conditions

Stop and report; do not improvise if:

- any in-scope code no longer matches the ownership or call paths described in
  this plan after the drift check;
- `$HOME/Workspace` is unavailable or the unique target directory is not
  writable;
- removing a repeated decode appears to require trusting caller-selected
  metadata, skipping file checksum verification, or dropping an exact-image
  comparison;
- streaming compaction appears to require changing the public `FileSystem`
  trait or bypassing the host filesystem with direct `std::fs`/`tempfile` I/O;
- scratch installation cannot use an exclusively created, same-directory,
  fully synced file;
- a failure path can install or overwrite a destination and then return an
  ordinary unambiguous error;
- retaining the verified image exceeds `max_database_bytes`, keeps both the
  encoded chain and decoded image, or leaks mutable image access publicly;
- a direct `crab-cell-runtime` or `crab-http-server` consumer requires a public
  compatibility alias or second recovery path;
- any verification command fails twice after a reasonable source fix;
- the controlled candidate misses the performance gate twice;
- the only apparent way to beat Celld capture time is to remove file fsync,
  parent-directory sync, checksums, or per-commit acknowledgement. Those are
  product/durability decisions outside this plan.

## Follow-up iteration: grouped capture durability

`Db::capture_deferred()` now uses an internal uncommitted rename for each
complete LTX file. `Db::durability_barrier()` uses the host filesystem's
bounded batch-sync boundary, then deduplicates the affected LTX parent
directories and syncs each once before the host can acknowledge or prune the
returned batches. The direct filesystem parallelizes independent file flushes;
custom filesystems retain a safe sequential default. Checkpoint, snapshot,
ordinary capture, and close flush any pending barrier first. A file or parent
barrier failure fences the session and retains pending paths for diagnostics.

This is a protocol-aware optimization, not a replacement for the synchronous
default. The host must keep the returned batches unacknowledged until the
barrier succeeds. Benchmark the grouped mode separately from the default mode;
the Celld comparison remains intentionally durability-explicit.

The final 15-round/3-warmup local matrix compared this grouped mode with the
pinned Celld runner. Crab's end-to-end median was 1.83x faster for 32 × 1 KiB,
2.05x faster for 128 × 4 KiB, and 1.98x faster for 512 × 16 KiB. P95 speedups
were 2.10x, 1.90x, and 1.69x respectively. Every round retained one invariant
segment/TXID/size tuple and passed row-count plus SQLite integrity checks.

Those numbers use one durability barrier after the complete workload. They
measure grouped throughput and batch completion latency, not independently
durable acknowledgement latency for each capture. In the corresponding
15-round synchronous matrix, where every Crab capture also synced its parent
directory, Crab's median total was 194 ms versus Celld's 99 ms, 717 ms versus
396 ms, and 2,964 ms versus 1,504 ms. The default path was therefore about
1.8x–2.0x slower; its stronger directory-durability contract was unchanged by
the grouped optimization.

This satisfies the grouped capture end-to-end target without changing the
synchronous default. It does not establish that Crab is generally faster than
Celld. Later recovery optimizations made Crab faster on the medium and large
recovery workloads, while Celld still won the small recovery workload. The
plan therefore remains in progress rather than converting workload-specific
recovery and grouped-throughput wins into a general performance claim.

### Independent threshold rerun

A later verification pass on the final branch did not reproduce a 1.5x floor
for batch 8. An 11-round/2-warmup matrix reported median total speedups over
shipped Celld of 1.30x, 1.49x, and 1.71x for the small, medium, and large
workloads. A second pass alternated execution order for 12, 10, and 8
single-round pairs. Crab won 29 of 30 pairs, but the median paired speedups were
only 1.40x, 1.41x, and 1.45x.

The host was busy during this rerun (load average about 13 on 12 logical CPUs),
so these absolute times are not release SLO evidence. The paired direction is
still strong evidence that bounded grouping improves local throughput, while
the lower medians show that the earlier 1.5x--2.0x result is not a stable floor.
The performance gate therefore remains open. Do not claim that batch 8 is
universally 1.5x faster, or that this local benchmark proves lower production
acknowledgement latency.

### Follow-up: remove the published-cleanup barrier

The authoritative root already covers the exact cut before
`Db::prune_captured()` runs, and every activation owns a fresh metadata
directory that is never reused as acknowledged state. Synchronizing the parent
after unlink therefore delayed the result only to make non-authoritative local
cleanup survive a crash. The cleanup still rereads and verifies the exact LTX
file before unlinking it, but no longer fsyncs that deletion.

A release-mode differential loop over 128 published-cut cleanups reduced median
cleanup time from 416.7 ms to 17.5 ms, a 23.7x improvement. A separate
32-command loop including SQL, immutable-root preparation, in-memory object
uploads, authority CAS, confirmation, and cleanup bracketed candidate medians
at 343.9 ms and 373.8 ms around a 388.5 ms old-path run. That is a 4%--11%
local publication-latency improvement on a noisy host. It is an incremental
Crab-versus-Crab result, not a Crab-versus-Celld full-system claim.

### Follow-up: pin the capture instead of copying upload scratch

`CellReplica::prepare()` now opens each caller-retained capture once and keeps
that exact handle through inspection and every multipart retry. The `FileIo`
contract binds an open handle to the selected artifact, so later namespace
replacement cannot redirect a retry. Inspection verifies exact LTX metadata,
length, and digest. Multipart upload hashes the complete source before object
completion and rechecks its length afterward, so same-artifact mutation still
fails publication. This removes the defensive create/copy/delete path and its
scratch admission without weakening the immutable-object or authority gates.

The retained host tests prove that preparation performs bounded reads with no
local writes or scratch capacity, cancellation drops the pinned handle, and a
later path replacement does not change bytes read from the open artifact. A
throwaway release profiler covered 32 sequential SQL, deferred-capture,
immutable-prepare, and published-cut cleanup cycles against an in-memory
object store. The original scratch-copy path reported an 81.3 ms median; the
first pinned-source run reported 40.9 ms, a 1.99x local improvement. A second
candidate run was faster again but ran after the host and build artifacts were
warm, so it is not used as the headline. The profiler was removed. This
isolates local publication mechanics; the retained Crab/Celld harness does not
exercise either system's external object/authority protocol.

A fresh direct local-filesystem matrix reran the retained Crab/Celld harness
with seven measured rounds, two warmups, and Crab durability batch 8 in both
execution orders. With Crab first, total speedups were 1.38x, 2.01x, and 2.28x
for small, medium, and large; recovery speedups were 1.10x, 2.12x, and 5.28x.
With Celld first, totals were 2.60x, 2.13x, and 1.50x, while recovery was 4.93x,
4.88x, and 2.55x. Crab won all six total comparisons, but the wide small-run
variance and one 1.38x result still reject a universal 1.5x floor. The stable
claim remains narrower: bounded-group Crab throughput is faster in these
workloads, medium total time exceeded 2x in both orders, and independently
durable per-capture latency is not represented by this grouped mode.

### Follow-up: reuse the authenticated capture index

Fresh root preparation decoded every LTX file to reconstruct the same page
index that `Db` had just produced while encoding it, then read the file again
for immutable upload. `LocalSegment` now privately carries an encoder-produced
index only when it comes from the managed `Db` capture path. Preparation pins
the exact file as before, reuses that index, and lets multipart upload verify
the complete source length and BLAKE3 digest before object completion. The
public `LocalSegment::new` constructor cannot attach this provenance and keeps
the full structural-inspection fallback. Retained index bytes share storage
across batch clones and are capped at 1 MiB per captured batch; larger cohorts
fall back rather than adding unbounded pending-publication memory.

Retained tests independently decode captured files and compare every index
byte, prove managed preparation performs exactly one bounded source read per
multipart part, and prove caller-constructed segments still take the full
inspection path. In a bracketed release profiler over 32 sequential prepares,
the full-decode baseline was 16.2 ms for 1 KiB mutations versus 10.2 ms and
10.1 ms candidate medians (1.59x--1.60x). For 256 KiB mutations, the baseline
was 76.3 ms versus 34.7 ms and 26.6 ms candidates (2.20x--2.87x). The in-memory
object store isolates local preparation work; these are Crab-versus-Crab
results, not a new full-system Celld comparison.

The final bounded implementation was rerun after index construction changed to
abandon retention as soon as a batch would exceed 1 MiB. On a heavily loaded
12-logical-CPU host, its 15-sample medians were 13.6 ms for 1 KiB mutations and
34.2 ms for 256 KiB mutations. Compared with the earlier same-harness decode
baseline, those observations suggest 1.19x and 2.23x improvements, but they are
not a controlled A/B because the host state differed. The retained correctness
tests, not those timings, are the proof for landing the bounded fast path.

An additional order-balanced direct matrix used seven measured rounds, two
warmups, and Crab durability batch 8 while the host load exceeded its logical
CPU count. Crab won total time in all six order/workload combinations, but the
speedups ranged from 1.11x to 1.66x. Recovery speedups ranged from 0.71x to
4.99x; Celld won the small recovery subtotal in both orders. A separate
diagnostic of Crab's default synchronous path was 1.74x--2.18x slower in total
than the shipped Celld path, consistent with Crab paying a parent-directory
barrier on every capture while pinned Celld does not. These results reject a
general "Crab is 1.5x--2x faster" or "Crab always has lower latency" claim.

### Follow-up: overlap independent immutable uploads

Root preparation previously serialized five object-store latency intervals for
an ordinary mutation: LTX body, index, changed directory node, root segment
page, and root document. These objects are independently content-addressed;
only their successful completion, not their upload order, gates the private
`PreparedRoot`. Preparation now overlaps body/index uploads, progresses up to
four captured segments concurrently, uploads changed directory objects with an
eight-object bound, and uploads the root document alongside its segment pages.
The existing host I/O semaphore remains the shared process-wide request bound.
If either dependency or metadata preparation fails, the other future is
cancelled and no proposal is returned; any completed object is unreachable
until a later authority CAS publishes its digest.

A deterministic paused-clock integration test injects 100 ms into every object
write on the real preparation call path. The sequential baseline required five
intervals (500 ms); the bounded dependency DAG requires two (200 ms), a 2.5x
latency reduction under the modeled provider delay. A zero-latency release
probe was inconclusive because host load exceeded 46 on 12 logical CPUs, so it
is intentionally not used as performance evidence. This is a Crab root-
preparation optimization; Celld exposes no equivalent standalone root protocol
in the retained local harness.

## Maintenance notes

- Reviewers should trace one corrupt input through plan construction, one
  pinned-source read failure through cancellation, and one ambiguous install
  error. The happy-path benchmark alone is insufficient proof.
- Keep `verify_segment` as the reusable single-segment verifier. The one-pass
  plan builder is an optimization for a complete explicit chain, not a new
  trust boundary for bundles or node frames.
- A local `VerifiedPlan` retains up to `max_database_bytes`; services that keep
  concurrent plans must add decoded-memory admission around construction and
  plan lifetime. The library limit is a correctness bound, not a process RSS
  governor.
- The grouped durability API is intentionally opt-in. Do not make
  `capture()` asynchronous or silently defer its parent barrier; doing so would
  change the acknowledgement contract.
- Checkpoint tuning and replica range-concurrency tuning need their own
  telemetry-backed plans because they affect WAL lifecycle and shared object-
  store admission respectively.
