# Plan 035: Harden crab-ltx as the Cell durability foundation

Status: IN PROGRESS — local Slices 1–3 pass; physical power-cut and protected proof gates remain; Slice 4 no-go pending production profile
Priority: P0 correctness; P1 measured performance
Effort: L, split into four reviewable changes
Risk: High at the filesystem and recovery boundaries
Planned at: `3b8d3b3614c` (2026-09-25)

## Executor contract

Execute the numbered slices in order, one change and review at a time. This
plan is self-contained; do not infer a stronger durability contract from a
passing unit test or from the word `sync`. Before each slice, compare its cited
code with `git diff 3b8d3b3614c..HEAD -- crates/crab-ltx crates/crab-cell-runtime`.
If ownership, recovery, or the cited call path has changed, stop and revise the
plan before editing. Use a `codex/` branch and conventional commit messages.
Do not push or open a PR unless the operator requests it.

The goal is a mechanically trustworthy `crab-ltx`: SQLite commit observation,
WAL capture, exact LTX verification, local file durability, and immutable Cell
root preparation live here. `crab-cell-runtime` owns authority, follower
membership and proofs, leases, response release, retention, and scheduling.
No node-level acknowledgement scheduler or second publication head belongs in
`crab-ltx` (`crates/crab-ltx/UPSTREAM.md:82`, `:125`; `crates/AGENTS.md`).

## Current state and evidence

- `Db::transaction_with` records the committed WAL boundary; capture checks it
  before checkpoint maintenance (`crates/crab-ltx/src/db.rs:360`, `:388`,
  `crates/crab-ltx/src/capture.rs:512`).
- Immediate `capture()` syncs the LTX file, renames it, and syncs its parent;
  deferred capture leaves the LTX file and name uncommitted until
  `durability_barrier()` or an external proof (`crates/crab-ltx/src/capture/wal.rs:605`,
  `crates/crab-ltx/src/db.rs:435`, `:454`, `:488`). The runtime uses deferred
  capture and requires fleet or exact-root proof before exposing a result
  (`crates/crab-cell-runtime/src/cell/executor.rs:1169`,
  `crates/crab-cell-runtime/docs/runtime.md:108`).
- Sparse activation builds a file-backed checksum base through
  `CellPagedDatabase::prepare_writable` and `directory::load_checksums`
  (`crates/crab-ltx/src/replica.rs:375`,
  `crates/crab-ltx/src/replica/directory.rs:548`).
  `PageChecksums::persist()` currently calls `file.sync_all()` for that base
  on every successful cut (`crates/crab-ltx/src/pages.rs:184`). Thus deferred
  **LTX** capture avoids the LTX-file and parent barriers, but a sparse Cell
  may still pay a checksum-sidecar barrier. The current local harness does not
  isolate this path; its fresh database uses a memory checksum base
  (`crates/crab-ltx/perf/README.md:84`, `crates/crab-ltx/src/pages.rs:62`).
- `Db::persist_continuation()` writes a newly synced dense checksum sidecar and
  a continuation for clean warm reuse; it requires a drained, fully hydrated
  database and does not grant authority itself (`crates/crab-ltx/src/db.rs:656`,
  `crates/crab-ltx/src/resume.rs:130`). A crashed session is not reopened by
  `Db::open()`; the old directory is quarantined (`crates/crab-ltx/src/db.rs:275`,
  `crates/crab-ltx/README.md:595`).
- WAL capture already reads a valid sparse tail on ordinary incremental cuts;
  full-image and mismatch paths still read the whole bounded WAL
  (`crates/crab-ltx/src/capture/wal.rs:225`). The timing ledger reports
  transferred bytes, full/fallback reads, peak WAL-image allocation, file
  sync, and parent sync (`crates/crab-ltx/src/types.rs:205`). Do not replace
  this path on intuition alone.
- Existing evidence covers ordered filesystem faults, process kill, exact
  restore, upstream vectors, and decoder fuzzing
  (`crates/crab-ltx/tests/host/hooks/matrix.rs`,
  `crates/crab-ltx/tests/ltx/crash.rs`,
  `crates/crab-ltx/tests/ltx/vectors.rs`). The README explicitly leaves broader
  filesystem and power-loss faults and provider/platform qualification open
  (`crates/crab-ltx/README.md:674`). Process kill is not power-loss proof.

Existing work that this plan must not duplicate: `advisor-plans/010` owns
bounded streaming publication; `advisor-plans/015` owns protected Cell/provider
qualification receipts; `advisor-plans/016` and `017` removed the standalone
epoch-head implementation. Use their current code and gates as dependencies,
not a reason to restore retired APIs.

## Required invariants

1. A successful standalone local `capture()` or `durability_barrier()` returns
   only after every returned cut's bytes and name are durable. An error cannot
   authorize a batch or leave the writer able to commit again when its outcome
   is ambiguous.
2. A successful Cell response is covered by the exact authoritative root or
   the selected followers' durable node-log tail. Local SQLite, LTX, and
   checksum sidecars alone never authorize it.
3. A missing, corrupt, or stale local sidecar is never used to select a root,
   extend a lineage, or accept a warm resume. A warm resume requires an
   authority match and an exact, complete local database image.
4. Every reconstruction is byte-identical to the selected endpoint or errors.
   No checksum-disabled LTX, weak fallback reader, legacy head, or silent
   downgrade is added.
5. Failures preserve source errors, release permits/scratch, and keep all
   SQLite handles closed on teardown. No new config switch is added merely to
   select a faster path.

## Common commands and boundaries

Before any Cargo build, verify that `$HOME/Workspace` is mounted and create a
**unique** target directory for the executing checkout. Set `CARGO_TARGET_DIR`
on every Cargo invocation; never use local `target/` or another worktree's
directory. Run focused tests locally and use CI/dedicated hosts for broad,
live, cross-platform, and power-cut evidence. `crates/crab-ltx/AGENTS.md`
defines the test layout and `UPSTREAM.md` defines the format lineage.

```bash
test -d "$HOME/Workspace/crabbuild-target" && test -w "$HOME/Workspace/crabbuild-target"
mkdir -p "$HOME/Workspace/crabbuild-target/crab-<this-worktree>"
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-<this-worktree>" cargo test -p crab-ltx --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-<this-worktree>" cargo test -p crab-ltx --features replica --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-<this-worktree>" cargo test -p crab-cell-runtime publication --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-<this-worktree>" cargo clippy -p crab-ltx --all-targets --features replica --locked -- -D warnings
cargo fmt -p crab-ltx -- --check
python3 crab/scripts/check-cell-ltx-layout.py
git diff --check
```

Replace `<this-worktree>` with its checkout-specific name. A missing volume is
a STOP condition, not permission to build locally. Run `cargo fmt` before each
commit. Do not edit baseline, snapshot, inventory, or ignore files to silence a
failure. A changed public type or format requires consumer search, format
vectors, and the affected runtime tests.

## Slice 1 — Freeze the proof boundaries and a comparable baseline

Scope: `crates/crab-ltx/README.md`, `perf/README.md`, `perf/crab/`, focused
`crab-ltx` tests, and a small `crab-ltx` contract document if needed. No
production durability change.

1. Trace immediate and deferred capture, sparse activation, checksum writes,
   continuation, root preparation, pruning, and runtime response release.
   Record a table of *which file is synced, what that sync proves, and which
   acknowledgement it may authorize*. Include `pages.rs:184` explicitly;
   correct any documentation that says deferred capture does no local syncs.
2. Extend the local harness with a **real sparse activation** from a prepared
   root so it reports SQLite commit, LTX capture, checksum-sidecar sync, and
   immutable-root preparation separately. Reuse `CaptureTiming`; add a narrow
   sidecar timing/call-count seam only if the current telemetry cannot
   distinguish it. Keep the existing fresh-DB benchmark unchanged as a
   separate workload. Report response proof latency from the existing runtime
   qualification/telemetry, clearly separated from this local harness.
3. Measure 4 KiB, 16 KiB, and a large full-image-triggering transaction on
   the same machine/filesystem/profile. Run at least seven measured rounds per
   mode, retain raw per-round JSON outside tracked source, and report p50/p95,
   operation counts, peak RSS, and WAL bytes read. Record device, OS, Rust,
   SQLite, and provider versions. The local harness does not claim fleet or
   cloud latency.

Verify: the two crate feature test commands above pass; the benchmark emits
separate rows for fresh and sparse activation; no root is counted durable
before the runtime's proof; `git diff --check` passes. Deliver a baseline table
and exact reproduction commands in `perf/README.md`.

## Slice 2 — Remove only a proven redundant hot-path barrier

Scope: `crates/crab-ltx/src/pages.rs`, `src/db.rs`, `src/resume.rs` only if the
contract requires it, `tests/host/hooks/`, and the Slice 1 docs/harness.
Runtime changes are limited to an integration assertion at the caller boundary.

1. Prove from all callers of `PageChecksums::persist`, `Db::persist_continuation`,
   `Db::open_resumed`, and `Db::close` that the mutable checksum base is either
   session-local scratch or is re-established durably at a named handoff.
   Check both memory and file bases. If any supported caller relies on the
   per-cut sidecar sync after process death, STOP and document that contract.
2. If the proof holds, remove the per-cut `sync_all` from the file-base
   `persist()` path. Keep checked writes, lengths, checksums, and fencing on
   write failures. Keep the clean continuation's fresh sidecar/file and parent
   syncs; do not weaken `capture()`'s LTX file/name barrier. Avoid adding a
   second public mode or compatibility shim.
3. Add one focused fault sequence that writes a sparse cut, loses the local
   checksum sidecar before any proof, and verifies recovery from the **previous**
   authority-pinned root with the unproved cut absent; another that completes
   a clean continuation and resumes the exact next TXID/checksum; and one
   injected sidecar write failure
   that fences the session. Follow `tests/host/hooks/matrix.rs` and the current
   warm-resume test in `src/db/tests.rs:533`. Remove or update only tests whose
   old assertion was the sync being removed.
4. Re-run the sparse workload from Slice 1. Report the reduction in sync count
   and p50/p95 latency, plus any regression in capture bytes or RSS. A speed
   claim requires the same acknowledgement contract and a repeated result;
   otherwise keep the safety proof and report no established performance win.

Verify: focused host, LTX, and runtime publication tests pass; both feature
sets, Clippy, format, layout, and `git diff --check` pass. A warm-resume proof
must still show no authority bypass or stale-local adoption.

## Slice 3 — Strengthen crash and exactness qualification

Scope: the existing `crab-ltx` host/ltx/cell test suites and their harnesses,
the dedicated-host probe under `examples/`, plus docs/workflow wiring if the
resulting gate is stable. No new storage format or fallback reader.

1. Build a deterministic volatile-filesystem model around the existing
   `FileSystem` hook. Model LTX/sidecar file-data flush, rename, and
   parent-directory flush separately; crash by discarding only unflushed
   operations. Leave SQLite's own files on the real test filesystem, since
   that hook does not intercept the SQLite VFS. Test cuts before and after
   each LTX barrier, deferred batches, sidecar updates, checkpoint restart,
   and pruning. Assert exact restore or explicit error for every surviving
   acknowledged LTX endpoint. Test clean resume separately with the existing
   real-SQLite process fixture. This is a model of the LTX filesystem contract,
   not physical power-loss evidence for SQLite.
2. Extend the process-kill smoke so the checked endpoint includes an active
   sparse Cell with deferred capture and one of the two real proofs. On restart,
   delete the former owner-local database and sidecars, restore the pinned root
   or follower tail, and verify a visible SQL value plus the next successful
   commit. Reuse the runtime's existing source-loss qualification path rather
   than building a parallel authority test in `crab-ltx`.
3. On a dedicated test host, run a real power-cut or block-device fault harness
   for the local `capture()` contract and clean continuation. Record filesystem,
   mount mode, device cache mode, cut point, expected durable endpoint, and
   observed endpoint. Do not label process kill or mocked faults as power loss.

Verify: every deterministic cut point has a named expected outcome; local test
commands pass; dedicated power-cut evidence is archived with its environment.
Any acknowledged endpoint that disappears or restores with different bytes is
a release blocker, not a benchmark outlier.

## Slice 4 — Optimize the next measured bottleneck

Scope is decided from Slice 1 and 2 profiles, then written as a small follow-up
change before implementation. The first candidate is full-image or fallback
capture's whole-WAL allocation (`capture/wal.rs:225`); the other is immutable
root object preparation measured by `perf/replica-cost/`. Choose only the
largest remaining *production* term, not the largest isolated microbenchmark.

- If whole-WAL allocation dominates, replace only that path with bounded
  validated frame iteration/spooling while preserving the committed-frame
  boundary, page map, checksum order, checkpoint behavior, and exact LTX bytes.
  Test a large WAL, stale suffix, corrupt committed frame, checkpoint restart,
  and exact restore. Keep the ordinary sparse-tail path unless its profile also
  justifies change.
- If object preparation dominates, extend `advisor-plans/010` rather than add a
  second upload path. Preserve pinned file handles, immutable bytes on retry,
  and root CAS ordering; validate against a real provider before claiming an
  end-to-end win.
- A fixed-name local append log is **not** preapproved by this plan. It changes
  the current whole-file `LocalSegment` contract and a per-Cell file cannot
  share one sync across Cells. Require a separate format/API and crash-recovery
  design with measured end-to-end benefit before considering it. Likewise, do
  not rebuild lost local cuts from WAL after a crash without a new authority,
  retention, and continuation proof.

Verify: repeat the same baseline matrix and require exactness/fault gates plus
at least 20% improvement in the selected p95 phase in three independent
same-host runs, with no more than 5% p95 regression in the other measured
workloads. If variance prevents a clear result, report it and do not land
performance-only complexity. Provider/Cell response latency needs its own
CI or dedicated-environment proof; local capture timing cannot stand in for it.

## Review and delivery gates

- Each slice has a diff limited to its stated scope, a before/after evidence
  table, and a short explanation of why sibling surfaces are unaffected.
- Before a verdict, review the changed function/module, one caller and callee,
  the sibling immediate/deferred path, adjacent tests, current `main`, and the
  SQLite/Rust/filesystem contract used by the change. Ask whether this is the
  best fix, not merely a plausible one.
- Run the narrow gates after each slice. Run the full `crab-ltx` tests for both
  feature sets and affected `crab-cell-runtime` tests before handoff; use the
  existing protected Cell/provider receipt matrix for release qualification.
- `git diff --numstat` must be reviewed for non-test growth. Delete displaced
  policy and duplicate paths. Update `README.md`, `UPSTREAM.md`, and API prelude
  only when their actual contracts change.

## STOP conditions

- The baseline cannot distinguish checksum-sidecar sync from LTX sync, or a
  current caller needs the sidecar durable before clean continuation.
- A change would weaken standalone local durability or let a Cell response
  escape before its fleet/object proof.
- Exact byte identity, checksum-linked position, or cold/warm recovery cannot
  be proved across the new failure cut points.
- The required target volume or isolated qualification environment is missing.
  Report the missing gate; do not substitute a weaker test and mark it done.
- A dependency or upstream filesystem behavior is needed but its source/docs
  have not been checked.

## Completion

The plan is complete only when Slices 1–3 have landed with the stated proof,
Slice 4 has either landed a measured improvement or recorded a justified no-go,
the protected Cell qualification remains green, and the durability contract
table accurately describes both standalone and Cell execution. Update the
status of this plan in `advisor-plans/README.md` after each slice.

## Execution record (2026-09-25)

- Slice 1: documented local and runtime proof boundaries in
  `crates/crab-ltx/README.md`; extended `perf/replica-cost` with a real sparse
  activation and isolated checksum-sync timing; retained seven raw independent
  process rounds for 4 KiB, 16 KiB, and full-WAL 4 MiB cases under the mounted
  target volume. The baseline and reproduction commands are in `perf/README.md`.
- Slice 2: removed the active file-backed checksum base's per-cut sync. Its
  writes and write-failure fencing remain; clean continuation still writes and
  syncs a fresh dense sidecar. Warm resume now verifies every local database
  page against that sidecar before reuse, closing a same-length corruption gap.
  Focused sparse source-loss, write-failure, and warm-resume tests pass. Sparse
  4 KiB capture moved from 2,465 / 3,203 to 285 / 335 µs p50 / p95 in the
  first paired local matrix; fresh 16 KiB variance prevents a global regression
  bound from that matrix alone.
- Gates run: both `crab-ltx` feature suites, runtime publication tests, Clippy
  for the crate and benchmark, format, layout, and diff checks pass. The
  isolated RustFS runtime source-loss takeover test passed against a disposable
  server and bucket. This is provider source-loss proof after orderly drain,
  not process-kill or power-cut proof. The protected qualification receipt and
  physical power-cut gate remain.
- Slice 3 local model: `tests/host/hooks/volatile.rs` snapshots host-file bytes
  only at file sync and namespace entries only at parent sync. It models an
  immediate LTX return, deferred file/name cut points, a failed parent barrier,
  a shared deferred-batch barrier, checkpoint restart, mutable sidecar edits,
  and unsynced pruning. SQLite's VFS is deliberately outside the model. A
  separate-process clean writer now leaves a continuation that a successor
  moves, verifies at its exact TXID/checksum, and extends by one cut. An active
  sparse writer was process-killed after an
  unproved deferred cut; after deleting its local source, the test selected the
  previous immutable root, saw the old SQL value, and committed its next cut.
  That fixture does not run runtime authority or follower proof; the dedicated
  physical power-cut/block-device run remains open.
- The existing `crab-http-server` filesystem process-fault smoke passed after
  building its UI prerequisite: it killed an owner after three acknowledged
  settlements, recovered through a successor, and checked an independent
  observer. It exercises runtime acknowledgement proof, but its killed owner
  is a bootstrap owner rather than the sparse activation above. The combined
  sparse-owner plus runtime-proof case was then added under
  `crates/crab-cell-runtime/tests/runtime/lifecycle/ownership/sparse_process.rs`.
  It passed against an isolated RustFS bucket: a child process activated a
  sparse writer with its file-backed checksum sidecar, received success after
  exact-root publication, was killed, and lost its local files. The successor
  restored the published root, read the acknowledged SQL value, and published
  the next commit. The test uses a real provider root CAS; it does not claim
  physical power-cut evidence.
- Slice 4 no-go for now: after the sidecar sync change, sparse 4 KiB and 16 KiB
  local captures are about 0.3 ms p50, while the 4 MiB full-WAL case is about
  41 ms p50 and reads about 24 MiB of WAL per measured round. Existing RustFS
  loopback root preparation was about 95–146 ms p95 depending on payload, but
  the runtime races object publication with follower proof. There is no
  protected response-phase receipt on this source revision to show which term
  is actually on the production acknowledgement path. A bounded full-WAL
  rewrite or additional object path now would be speculative; choose and
  benchmark one only after that receipt identifies the dominant term.
- The production `crab_cell_ltx_phase_seconds{phase="root_preparation"}`
  histogram now times one `CellReplica::prepare` attempt, including admission,
  immutable uploads, and verification. The existing capture phase and
  durability-proof wait histograms can be compared in the protected run. Root
  preparation may overlap follower proof, so its isolated duration alone does
  not establish the response's critical path.
- A dedicated-host probe now lives at `crates/crab-ltx/examples/power_cut_probe.rs`.
  Its capture and continuation writers emit exact off-device checkpoints for
  an external fault controller; their post-restart verifiers check LTX digest,
  exact restore, database digest, clean resume, and next-cut continuity. Both
  modes passed a local process-kill smoke. No physical power-cut or block-device
  receipt has been produced on the required dedicated host.
- Directory-aware crash modeling exposed a real first-cut hole: file sync and
  `ltx/0` sync did not persist the newly created `0`, `ltx`, and session
  directory names. Immediate capture and the deferred local barrier now sync
  those ancestors leaf-to-root once per session; failure fences the writer.
  The model failed before this fix and passes with it. This remains modeled
  evidence until the dedicated-host fault run confirms the actual filesystem.
- After that fix, the ignored
  `rustfs_process_killed_sparse_owner_restores_published_root_and_continues`
  runtime test passed on `588b653510d` against a disposable, isolated RustFS
  1.0.0-rc.1 bucket and prefix on Darwin 25.5.0 arm64. The sparse owner
  published the exact root, was killed, lost its local files, and a successor
  read the acknowledged value and published the next commit. This is local
  provider/process-loss evidence, not a physical power-cut or protected
  qualification receipt.
- The local `replica-cost` runner now reports first-cut capture and parent-sync
  separately. Seven release-process 4 KiB runs on the current build measured
  first-cut capture at 6,665 / 9,591 µs p50 / p95 and parent sync at
  3,003 / 6,024 µs. Raw JSON and reproduction details are in `perf/README.md`.
  These figures quantify the one-time local barrier cost, not Cell response
  latency or an end-to-end performance win.
