# `crab-ltx` versus `celld-ltx`

This directory contains a small, reproducible local-filesystem comparison of
the two in-process LTX implementations. It is intentionally outside the Crab
Cargo workspace: `crab-ltx` uses `rusqlite` 0.34 while the pinned Celld source
uses `rusqlite` 0.31, and Cargo cannot link two `libsqlite3-sys` versions in
one process. Each runner is therefore its own package and emits JSON.

The Celld runner is pinned to the revision documented in
[`UPSTREAM.md`](../UPSTREAM.md):

`10cb1303dac710dcb3b557e318e08c855261f68b`

## Current conclusion

This harness does **not** establish that Crab is universally faster than
Celld. Crab's default capture pays a parent-directory durability barrier that
the pinned Celld capture does not, and is slower in the direct comparison.
Crab's opt-in grouped barrier improves total throughput in the retained local
workloads, but it measures batch completion rather than independently durable
per-transaction latency; repeated runs have not established a universal 1.5x
speedup. Recovery is also workload-dependent, with Celld still able to win the
small case. Treat the phase data and durability contract as part of every
performance claim.

On 2026-09-21, a release diagnostic on the same macOS host ran 128
transactions with 4 KiB payloads, one warmup and three measured rounds per
process. In three alternating Crab/Celld pairs, Crab default total time was
1.51x, 1.49x, and 1.52x the pinned Celld default total time (slower). With
the runner-only Celld `--sync-parent` option, the Crab/Celld ratios were
0.965, 1.008, and 0.999: near parity when both pay directory barriers.
Crab batch-8 completed a separate three-round diagnostic at a 0.424 s
median versus the pinned Celld default at 0.583 s, but batch completion is
not independently durable per-capture latency. These small local samples
confirm the durability-cost explanation; they are not production SLOs or
evidence of a universal Crab win.

### Current PR comparison (2026-09-25)

The current `crab-ltx` release runner and Celld pinned at
`10cb1303dac710dcb3b557e318e08c855261f68b` ran on the same macOS 25.5
external APFS SSD. Each mode ran seven alternating independent processes;
each process warmed one 128-transaction round and measured three more. The
table shows the median of each process's median, followed by the nearest-rank
p95 across the seven process medians. Times are for the whole 128-transaction
round, in milliseconds.

| Payload | Mode | Capture p50 | Recovery p50 | Full round p50 / p95 |
| --- | --- | ---: | ---: | ---: |
| 4 KiB | Crab immediate | 744 | 25 | 813 / 965 |
| 4 KiB | Celld default | 403 | 13 | 461 / 773 |
| 4 KiB | Celld with diagnostic directory syncs | 727 | 26 | 786 / 937 |
| 16 KiB | Crab immediate | 805 | 28 | 919 / 946 |
| 16 KiB | Celld default | 420 | 24 | 503 / 542 |
| 16 KiB | Celld with diagnostic directory syncs | 784 | 29 | 875 / 939 |

Celld's default syncs completed LTX file bytes but does not sync each renamed
file's directory entry. The runner-only diagnostic syncs L0 after every cut,
the new L0 ancestor names after the first cut, the new L1 name after
compaction, and the restored file's parent. With that diagnostic, Crab's full
round median is 3% slower at 4 KiB and 5% slower at 16 KiB. Against Celld's
unchanged default it is 76% and 82% slower, respectively. The 4 KiB p95 has
substantial run-to-run variance. The full-round comparison also includes
different SQLite versions and different recovery verification work, so it is
not an isolated capture-algorithm comparison.

Crab `--durability-batch 8` measured 325 / 337 ms p50 / p95 at 4 KiB and
309 / 328 ms at 16 KiB for the same round. Those medians are 29% and 39%
below Celld's default full-round medians, but each group of eight cuts waits
for one shared barrier before local durability can be acknowledged. They do
not represent independent per-transaction durable latency. Raw JSON for all
five modes is at
`$HOME/Workspace/crabbuild-target/crab-1bab/ltx-celld-current-20260925/`.
Reproduce each mode with `--transactions 128 --rounds 3 --warmup 1` and
`--payload-bytes 4096` or `16384`, repeating in seven alternating processes;
add `--sync-parent` for the Celld diagnostic or `--durability-batch 8` for
Crab's grouped mode.

## Run it

The script uses release builds, one warmup round, and five measured rounds by
default. It stores build output under the mounted workspace volume when
`CARGO_TARGET_DIR` is not supplied.

```bash
crates/crab-ltx/perf/run.sh
```

Override the workload without editing the harness:

```bash
LTX_TRANSACTIONS=512 \
LTX_PAYLOAD_BYTES=16384 \
LTX_ROUNDS=7 \
LTX_WARMUP=2 \
crates/crab-ltx/perf/run.sh
```

To measure the opt-in grouped durability path on the Crab runner, invoke it
directly with `--durability-batch N`. It completes and renames each LTX file,
then uses a bounded parallel file flush followed by one shared parent-directory
sync whenever `N` captures are ready. The final partial batch is also flushed:

```bash
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ltx-perf" \
  cargo run --release \
  --manifest-path crates/crab-ltx/perf/crab/Cargo.toml -- \
  --transactions 128 --payload-bytes 4096 --rounds 5 --warmup 1 \
  --durability-batch 8
```

`--durability-batch 1` is the default synchronous path. Values greater than
one bound each durability group to at most `N` captures; they do not make an
individual capture durable before that group's barrier succeeds.

The binaries also run directly when a single side is useful:

```bash
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ltx-perf" \
  cargo run --release \
  --manifest-path crates/crab-ltx/perf/crab/Cargo.toml -- \
  --transactions 128 --payload-bytes 4096 --rounds 5 --warmup 1
```

## Workload and measurements

Every measured round creates a fresh temporary SQLite database and performs the
same sequence:

1. Create `payloads(id INTEGER PRIMARY KEY, value BLOB NOT NULL)`.
2. Capture/sync the schema transaction.
3. Commit `N` one-row transactions. Each row contains a deterministic BLOB of
   `--payload-bytes` bytes, followed by one capture/sync call.
4. Compact all captured L0 files into one local output.
5. Restore the compacted state into a fresh SQLite file and run
   `PRAGMA integrity_check` plus a row-count check.

The JSON reports median wall-clock microseconds across measured rounds. The
important fields are:

- `workload_write_us`: SQLite commit time for schema plus the `N` inserts.
- `capture_us`: local WAL-to-LTX capture time, including local file syncs.
- `capture_*_us`: Crab's capture phase ledger. `capture_fsync_us` is the LTX
  file sync; `capture_parent_sync_us` includes the rename's parent sync and,
  on the first cut, the new directory-chain syncs. The other fields split
  position resolution, WAL reads, page collection, encoding, and local writes.
- `capture_barrier_us`: only populated for the Crab deferred mode; it includes
  the grouped file flush and final parent-directory barrier and is included in
  `capture_us`.
- `verify_us`: Crab's explicit owned-input plan verification. Celld reports
  zero because its compactor does not expose an equivalent call.
- `compact_us`: local LTX compaction, including source listing, reads, merge,
  and output fsync for Celld; Crab's `compact_exact` merge and output install.
- `compact_verify_us`: Crab's verification of the compacted output. Celld's
  destination-level continuity check is included in `compact_us`.
- `restore_us`: end-to-end restore wall time. Celld additionally reports its
  plan, download, and apply sub-timings.
- `recovery_us`: the recovery subtotal. Crab defines it as
  `verify_us + compact_us + compact_verify_us + restore_us`. Celld uses the
  same formula, with its explicit verification fields set to zero.
- `total_us`: the full local-round headline:
  `workload_write_us + capture_us + recovery_us`.
- `input_ltx_bytes` and `compacted_ltx_bytes`: storage amplification evidence.

The harness checks the restored row count and SQLite integrity in every round;
it does not include those checks in the reported restore timer.

For a phase comparison, use `recovery_us` rather than comparing `compact_us`
alone. Crab intentionally verifies and owns every input before the merge; the
verified image is then encoded as a snapshot, synced, and read back to match its
exact length and BLAKE3 digest before installation. `compact_verify_us` builds
the explicit plan required by Crab's restore API and independently decodes that
snapshot. Celld's pinned `ReplicaCompactor` validates the range shape and
destination continuity but does not expose equivalent input-plan or compacted-
plan verification phases. Use `total_us` as the only full local-round headline.

The implementations do not have identical durability costs. Crab fsyncs the
LTX file and its parent directory before returning a capture batch. The pinned
Celld path fsyncs the file but uses a plain rename without a parent-directory
sync. Do not treat the capture-only gap as a portable performance win without
making that durability choice explicit.

The September 21 comparison and the sidecar before/after matrix below predate
the fix that syncs the newly created `ltx/0`, `ltx`, and session-directory names
on the first locally durable cut.
Its numbers are historical rather than a current-build timing claim. Later
cuts in the same session reuse that directory-chain proof.

The `replica-cost` JSON now separates the schema bootstrap's first immediate
capture (`bootstrap_capture_us`) and its complete parent-sync phase
(`bootstrap_parent_sync_us`) from measured commands. On the current build,
seven independent release processes with 4 KiB commands measured first-cut
capture at 6,665 / 9,591 µs p50 / p95 and parent sync at 3,003 / 6,024 µs.
This was macOS 25.5 on the same external APFS SSD, Rust 1.97.0, bundled
SQLite 3.49.1, and the in-memory object store. The phase includes the final
LTX rename's direct-parent sync and the three one-time ancestor syncs; it
does not isolate those four calls individually. Raw per-process JSON is at
`$HOME/Workspace/crabbuild-target/crab-1bab/ltx-firstcut-20260925/`.
Reproduce each process with the release binary and
`--payload-bytes 4096 --commands 12 --warmup 5`; repeat seven times.

The Celld runner accepts `--sync-parent` as a diagnostic contract-normalization
mode. After each upstream `Db::sync()`, it syncs Celld's L0 directory before
recording capture completion. On the first cut it also syncs the newly created
`0`, `ltx`, and session-directory names; after compaction it syncs the new L1
directory name. It syncs the destination directory after restore installation.
This is runner-only behavior, not pinned Celld behavior. The September 21
`--sync-parent` ratios above predate these additional directory-chain syncs.

The grouped Crab mode measures batch completion: all captures remain
unacknowledged until the final file-and-directory barrier succeeds. It is a
throughput comparison, not a measurement of independently durable
per-transaction acknowledgement latency. Compare Crab's default `capture()`
path when every capture must cross its own durability boundary.

## Scope

This is a local mechanics benchmark, not a claim about the complete durability
protocol. It does not measure immutable-root preparation, object-store latency,
network retries, Cell authority/owner-head CAS, acknowledgement ordering,
retention, scheduled multi-level compaction, or either implementation's paged
VFS. Those paths have different contracts and need a second harness with the
same object-store and authority model before they can be compared fairly.

## Cell publication cost per command

`replica-cost/` measures the other half of that scope: what one command costs
the Cell object store when it publishes an immutable root. Each measured command
commits one SQLite transaction, captures one LTX cut, and prepares one successor
root through `CellReplica` over an in-memory `object_store`, which reports the
objects and bytes a provider would receive.

```bash
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ltx-replica-cost" \
  cargo run --release \
  --manifest-path crates/crab-ltx/perf/replica-cost/Cargo.toml -- \
  --payload-bytes 4096 --commands 32 --warmup 4
```

### Sparse activation baseline (2026-09-25)

Pass `--sparse` to bootstrap a root, close the source writer, and activate a
real sparse writer through `open_root().paged().prepare_writable()` and
`open_writable()`. This mode uses deferred capture, prepares an immutable
successor, and prunes its local cut after preparation. It reports SQLite commit,
capture, checksum-sidecar sync count/time, root preparation, WAL bytes read, and
peak WAL-image allocation separately. The sidecar sync measurement wraps only
the local `FileSystem`; it does not include SQLite VFS syncs. The fresh mode
retains its original immediate-capture workload. Neither mode measures runtime
response proof latency or grants authority merely by preparing a root.

The runner also emits per-command `capture_*_us` fields for all phases in
`CaptureTiming`. They distinguish WAL transfer, page collection, cut encoding,
checkpoint maintenance, and LTX reinspection during batch collection.
`prune_us` measures sparse-mode cleanup after root preparation; it is null in
fresh mode, which retains cuts. `captured_bytes` is the total LTX length in that
command's batch, including any checkpoint cut. Preparation timing excludes
cleanup, and neither timing includes the runtime authority CAS.

### Streaming published-cut cleanup (2026-09-26)

Three release processes per workload and implementation used local RustFS,
command-seeded random payloads, sparse deferred capture, six commands, and one
warmup. The large workload used 4 MiB inserts and a 1 MiB incremental-capture
limit, exercising full-image cuts and checkpoint batches. Both versions used
the same macOS/APFS host and RustFS instance. Baseline production source was
`0c898097e95`; the candidate replaces cleanup's full-file buffer with a 64 KiB
buffered reader and verifies its digest during decoding.

| Captured bytes in batch | Before cleanup, median ms | Streaming cleanup, median ms |
| ---: | ---: | ---: |
| 16,941,374 | 63.204 | 52.233 |
| 25,412,106 | 94.847 | 88.384 |
| 33,882,834 | 128.005 | 127.587 |
| 42,353,558 | 160.156 | 143.721 |
| 50,824,284 | 194.740 | 153.671 |

Each row is the median of three matching command positions, not a tail
percentile. The small 4 KiB payload workload produced 5,162–7,155-byte batches;
its pooled cleanup median was 161 microseconds for both implementations over
15 measured commands each. Variation is visible in the raw samples. These
measurements suggest a large-cut benefit but do not establish an application
latency improvement, a 1 GiB RSS bound, or sustained throughput.

Raw reports, binary SHA-256 values, host metadata, and the candidate source diff
are retained under
`$HOME/Workspace/crabbuild-target/crab-8bc8/prune-streaming-20260926/`.
`summary.json` retains every per-command comparison. Reproduce the large run
against an existing local RustFS bucket:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
TMPDIR="$HOME/Workspace/crabbuild-target/crab-8bc8/tmp" \
  cargo run --release --locked \
  --manifest-path crates/crab-ltx/perf/replica-cost/Cargo.toml -- \
  --sparse --random-payload --payload-bytes 4194304 \
  --max-capture-bytes 1048576 --commands 6 --warmup 1 \
  --endpoint http://127.0.0.1:19010 --bucket crab-cell-issue-fleet \
  --access-key crab --secret-key crab
```

Cleanup still decodes every page and retains decoder indexes on the SQL worker.
The transfer-bound regression rejects a whole-file read for a large random cut;
the failure cases retain accounting and allow retry after repair. Decoder index
memory and same-worker response interference remain separate audit gates.

### Large sparse checkpoint capture (2026-09-25)

With `--sparse --payload-bytes 4194304 --max-capture-bytes 1048576
--commands 3 --warmup 1`, seven independent release processes measured two
commands each. Each row below is the p50 / nearest-rank p95 of the seven
per-process medians, in microseconds. The before and after binaries ran on the
same macOS/APFS host; run-to-run variation makes this local evidence rather
than a response-latency SLO.

| Phase | Before | After retaining every sealed cut |
| --- | ---: | ---: |
| SQLite commit | 4,327 / 4,550 | 4,482 / 5,258 |
| LTX capture | 48,935 / 50,122 | 38,801 / 47,617 |
| LTX reinspection during collection | 10,321 / 10,588 | 0 / 0 |
| LTX encode | 21,376 / 21,984 | 21,632 / 22,089 |
| In-memory root preparation | 2,860 / 2,926 | 2,881 / 2,967 |

Checkpointing can seal a second cut before the command receives its batch.
The prior implementation cached the newest cut's metadata and re-read the
earlier LTX file to obtain its size, digest, and checksums. The writer already
computed those values while sealing that same cut. The new cache holds each
sealed result until collection, removing that reinspection. `VerifiedPlan`
still reads and verifies every cut before exact restore. Raw per-process JSON
is under `$HOME/Workspace/crabbuild-target/crab-1bab/ltx-slice4-profile-20260925/`
(`sparse-*.json` and `metadata-cache-*.json`). The production response phase
profile and provider durability receipt remain open.

For a small-cut regression check, 21 independent processes per mode used
`--payload-bytes 4096` or `16384`, `--commands 12 --warmup 5`. Sparse deferred
capture measured 280 / 329 µs at 4 KiB and 301 / 351 µs at 16 KiB (p50 /
p95), compared with the earlier seven-process 285 / 335 and 314 / 374 µs.
Fresh immediate capture measured 4,278 / 5,578 µs and 4,572 / 5,509 µs,
compared with 4,255 / 5,362 and 4,465 / 5,565 µs. These are different
process counts and non-interleaved host runs, so small differences are not
attributable to this change; none shows a greater than 5% p95 regression.

Seven independent release-process rounds per row on macOS 25.5, APFS on a USB
SSD, Apple silicon, Rust 1.97.0, bundled SQLite 3.49.1, and `object_store`
0.14.2 in-memory. Each small-payload round measured seven commands after five
warmups; each 4 MiB round measured two after one warmup. Cells show p50 / p95
across the seven per-round medians, in microseconds. Peak RSS is the median
per-process maximum from `/usr/bin/time -l`. The large case sets
`--max-capture-bytes 1048576` and recorded four complete WAL reads per round.

| Workload | Payload | SQLite commit | LTX capture | Sidecar sync | Root prepare | Sidecar syncs / round | WAL read / round | Peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Fresh immediate | 4 KiB | 318 / 442 | 4,255 / 5,362 | 0 / 0 | 172 / 207 | 0 | 149 KiB | 11.8 MiB |
| Sparse deferred | 4 KiB | 332 / 346 | 2,465 / 3,203 | 2,188 / 2,741 | 176 / 256 | 7 | 177 KiB | 12.1 MiB |
| Fresh immediate | 16 KiB | 244 / 256 | 4,001 / 4,824 | 0 / 0 | 173 / 314 | 0 | 234 KiB | 12.0 MiB |
| Sparse deferred | 16 KiB | 279 / 351 | 3,154 / 3,347 | 2,790 / 3,006 | 214 / 420 | 7 | 262 KiB | 12.3 MiB |
| Fresh immediate | 4 MiB | 4,800 / 5,357 | 45,164 / 47,473 | 0 / 0 | 2,498 / 2,795 | 0 | 24.4 MiB | 37.1 MiB |
| Sparse deferred | 4 MiB | 4,485 / 4,812 | 48,017 / 48,920 | 5,055 / 6,165 | 2,580 / 2,620 | 4 | 24.3 MiB | 37.8 MiB |

The raw per-round JSON is outside tracked source at
`$HOME/Workspace/crabbuild-target/crab-1bab/ltx-baseline-20260925/`. To
reproduce a round after a release build, run the corresponding command seven
times, retaining each JSON output and `/usr/bin/time -l` maximum resident set
size:

```bash
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-<checkout>" \
  cargo build --release --locked \
  --manifest-path crates/crab-ltx/perf/replica-cost/Cargo.toml
/usr/bin/time -l "$HOME/Workspace/crabbuild-target/crab-<checkout>/release/crab-ltx-replica-cost" \
  --sparse --payload-bytes 4096 --commands 12 --warmup 5
```

Replace the payload with `16384` for the middle row. For 4 MiB, use
`--payload-bytes 4194304 --max-capture-bytes 1048576 --commands 3 --warmup 1`.
Omit `--sparse` for fresh rows. The published RustFS loopback measurements
below are provider preparation cost; runtime fleet or exact-root response
latency needs a separate qualification receipt.

Production telemetry now includes
`crab_cell_ltx_phase_seconds{phase="root_preparation"}` for each normal
`CellReplica::prepare` attempt, alongside capture phases and
`crab_cell_durability_wait_seconds{source="fleet|object"}`. Preparation includes
admission, immutable uploads, and verification; it can overlap follower proof.
Use the protected response profile to decide which phase controls acknowledgement
latency before selecting another optimization.

After removing the active sidecar's per-cut sync, a second seven-round matrix
with the same commands and host measured:

| Workload | Payload | Capture p50 / p95 before → after | Sidecar syncs / round before → after | WAL read / round before → after |
| --- | ---: | ---: | ---: | ---: |
| Fresh immediate | 4 KiB | 4,255 / 5,362 → 3,912 / 4,502 | 0 → 0 | 149 → 149 KiB |
| Sparse deferred | 4 KiB | 2,465 / 3,203 → 285 / 335 | 7 → 0 | 177 → 177 KiB |
| Fresh immediate | 16 KiB | 4,001 / 4,824 → 4,465 / 5,565 | 0 → 0 | 234 → 234 KiB |
| Sparse deferred | 16 KiB | 3,154 / 3,347 → 314 / 374 | 7 → 0 | 262 → 262 KiB |
| Fresh immediate | 4 MiB | 45,164 / 47,473 → 43,154 / 45,192 | 0 → 0 | 24.4 → 24.4 MiB |
| Sparse deferred | 4 MiB | 48,017 / 48,920 → 41,348 / 43,153 | 4 → 0 | 24.3 → 24.3 MiB |

The second raw matrix is at
`$HOME/Workspace/crabbuild-target/crab-1bab/ltx-after-sidecar-20260925/`.
The unchanged fresh 16 KiB path moved by more than the desired 5% tolerance,
so these two sequential matrices alone cannot establish a precise global
latency regression bound. The sidecar sync count and sparse capture reduction
are direct local evidence; a response-latency claim still requires the runtime
qualification environment.

Pass `--endpoint http://host:port --bucket <bucket> --access-key <key>
--secret-key <secret>` to run the identical workload against an S3-compatible
provider. The table below was measured on 2026-09-24 (Apple silicon, release
build, one bootstrap root then 28 measured commands) against the in-memory store
and against a local RustFS server:

| payload | objects/command | objects p95 | bytes/command | bytes p95 | in-memory us p50 / p95 | RustFS us p50 / p95 / p99 |
| --- | --- | --- | --- | --- | --- | --- |
| 4 KiB | 5 | 5 | 13,050 | 20,009 | 189 / 289 | 99,872 / 139,124 / 152,133 |
| 16 KiB | 5 | 5 | 19,057 | 29,331 | 222 / 393 | 87,197 / 94,588 / 98,919 |
| 256 KiB | 7 | 11 | 81,735 | 105,934 | 408 / 556 | 85,729 / 126,159 / 155,510 |
| 1 MiB | 8 | 11 | 159,666 | 178,523 | 604 / 848 | 97,929 / 120,578 / 132,084 |
| 4 MiB | 13 | 14 | 526,784 | 547,043 | 1,102 / 1,583 | 126,178 / 145,947 / 149,441 |

Every command pays a segment body, its index, the rewritten directory nodes, the
root document, and any segment page. Object and byte counts are byte-identical
across the two stores, so they are provider independent; only latency moves, and
the RustFS rows are loopback latency, not a cloud bucket. Counts grow with the
number of directory leaves a payload touches: 5 objects for a small write up to
13 at the 4 MiB maximum a built-in primitive may write in one command. Byte cost
is dominated by the rewritten leaves and root document, and the payload's
compressibility matters more than its size.

A hot Cell offering 100 commands per second would demand roughly 500-1,300
logical immutable-object uploads per second at these measured costs. These
are not HTTP request counts: at that revision native LTX bodies used multipart even when small,
and retries, metadata reads, and authority CAS add requests. The runtime admits
against one pending-publication byte high-water mark per Cell plus the
32-segment compaction debt that folds the
root graph. Publication stays one serialized root per command because the object
path is the long-term durability authority and the node-log fleet proof releases
the command earlier; on this loopback RustFS path one command costs roughly
0.09-0.15 seconds of provider work, so a Cell without a fleet proof is
provider-latency bound, not CPU bound. Coalescing several commands into one root
could save metadata objects while retaining each command's captured bodies.
Fleet proof can release a response before object publication; the response
winner and sustained publication drain rate still need measurement. Multi-Cell concurrency,
cloud-bucket p99, and retention cost remain unmeasured.

Audit qualification limits: `replica-cost` generates a periodic payload that
repeats every 251 bytes; it does not represent incompressible data. Its direct
`CellReplica::prepare` loop excludes runtime authority CAS, the follower race,
and scheduled compaction. With 28 measured commands, nearest-rank p99 is the
maximum sample. Retain these rows as a reproducible historical workload;
qualify entropy, sustained publication debt, and application latency separately.
The [LTX performance audit](../../crab-cell-runtime/docs/ltx-performance-audit.md)
records source-backed optimization candidates and their acceptance gates.

### Small-body transfer experiment

On 2026-09-25, seven independent release processes before (`a7091fd7138`)
and after (`85a3bf684d3`) the bounded single-PUT change used the same loopback
RustFS container. Each process ran `--sparse --payload-bytes 4096 --commands 32
--warmup 4`: 28 measured preparations of the historical periodic payload.
Values below are the median of the seven per-run statistics, in microseconds:

| Measurement | Before | After |
| --- | ---: | ---: |
| Root preparation p50 | 6,642 | 6,163 |
| Root preparation p95 | 10,951 | 12,142 |
| SQLite commit p50 | 463 | 550 |
| Capture p50 | 542 | 653 |
| Logical objects / command | 5 | 5 |
| Mean bytes / command | 13,046 | 13,046 |

Raw samples and source/binary SHA-256 metadata are retained outside the
checkout under `$HOME/.codex/cell-vfs-ltx-scale/ltx-upload-a7091fd/`, with
`before-0.json` through `before-6.json` and matching `after-*` files.
The store image is the pinned RustFS `1.0.0-beta.8-glibc` from the Compose
example; the harness uses `object_store` 0.14.2 and bundled SQLite 3.49.1.

This is an inconclusive latency comparison: phases ran sequentially on one
shared host, p95 worsened, and the unchanged commit/capture paths also slowed.
The baseline itself differs substantially from the earlier RustFS table.
Do not attribute that historical difference to this patch or claim a p99,
fleet throughput, or latency SLO. This harness does not install the persistent
directory cache, so it cannot measure removal of cache-index writes.

Focused transport tests prove the narrower change: native, compacted, and
bundled bodies up to 256 KiB use single PUT; larger bodies keep multipart;
lost responses reconcile; conflicting bytes are refused; restored databases
are identical. A real RustFS public HTTP/peer test also passed mutations and
restoration after takeover. Public-action p95/p99 and sustained publication
drain remain the performance acceptance gates.

### Backend calls and payload entropy

The runner now uses the existing `Store::with_storage_observer` boundary.
Each sample's `preparation_io` records backend operation/outcome, calls,
accumulated duration, bytes read, and bytes written for root preparation.
Bootstrap, activation, and commit/capture reads are excluded. Calls retried
by `Store` appear separately; retries inside the provider client do not.
These are logical backend calls, not a wire-level HTTP request counter.
Concurrent call durations overlap and must not be added to infer wall time.

The default report labels its historical data `periodic-251`.
`--random-payload` selects deterministic command-seeded xorshift64 bytes and
labels them `xorshift64-command-seeded`. Generation is outside the timed SQL
transaction. This is workload data, not a cryptographic random generator.

```sh
TMPDIR="$HOME/Workspace/crabbuild-target/crab-8bc8/tmp" \
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-8bc8" \
  cargo run --release --locked --manifest-path \
  crates/crab-ltx/perf/replica-cost/Cargo.toml -- \
  --sparse --random-payload --payload-bytes 300000 --commands 12 --warmup 4 \
  --endpoint http://127.0.0.1:19010 --bucket crab-cell-issue-fleet \
  --access-key crab --secret-key crab
```

Three real RustFS smoke runs at `6226f0445c1`, each with eight measured
preparations after four warmups, produced these aggregate counts:

| Payload | HEAD | PUT | Multipart start / part / complete | Mean logical bytes / root |
| --- | ---: | ---: | ---: | ---: |
| 4 KiB periodic | 16 | 40 | 0 / 0 / 0 | 7,468 |
| 4 KiB random | 16 | 40 | 0 / 0 / 0 | 13,627 |
| 300,000 bytes random | 16 | 51 | 8 / 8 / 8 | 356,575 |

All calls completed successfully. The small-root path still performs two
metadata presence checks and five immutable PUTs per measured command. The
larger body crosses the bounded single-PUT threshold. Raw `io-committed-*.json`
samples and source/binary metadata are retained beside the comparison above.
These short runs verify observation wiring and expose payload sensitivity;
they do not establish tail latency, runtime CAS cost, compaction cost, or
sustainable throughput. Keep the missing-root-metadata refusal invariant when
evaluating those two HEADs.

### Sparse activation over real RustFS

The scale example at `42b7eb8e0c9` measures cold and reused metadata at one,
four, and eight shared I/O slots. The library includes bounded leaf overlap
from `b2c3b51bede`. See the [runnable example](../examples/README.md#rustfs-scale-workload)
for setup, JSON fields, and cache semantics. This experiment compares admission
settings on the same implementation; it is not a previous-revision comparison.

Two release processes on 2026-09-25 (local time) used SQLite `randomblob`
payloads of 32 MiB and 256 MiB. Each process produced three samples per
slot/cache pair, reversing slot order in the middle round. The table gives
median checksum-preparation milliseconds; calls/bytes were identical in all
three cold samples for each size and slot count.

| Payload | Metadata cache | 1 slot | 4 slots | 8 slots | Origin calls / bytes |
| --- | --- | ---: | ---: | ---: | ---: |
| 32 MiB | Cold | 100.464 | 74.721 | 73.979 | 33 / 723,272 |
| 32 MiB | Reused | 52.158 | 78.214 | 83.789 | 0 / 0 |
| 256 MiB | Cold | 1,057.293 | 419.734 | 295.320 | 259 / 5,797,768 |
| 256 MiB | Reused | 119.020 | 57.479 | 71.758 | 0 / 0 |

The databases contained 8,207 and 65,626 pages of 4 KiB. Every activation
queried its restored row successfully and materialized four pages. Both
processes deleted the source and passed byte-identical full restore and
compaction restore. Cold checksum preparation benefits from overlap in these
samples; the zero-origin phase still has substantial and variable local work.
Wider concurrency does not improve every phase: cold writable-open medians at
256 MiB were 107, 82, and 126 ms for one, four, and eight slots. Three samples
per setting do not establish tails or a universally optimal concurrency.

The first query made two range calls totaling 524,994 bytes at 32 MiB and
265,332 bytes at 256 MiB. The current VFS requests up to 64 contiguous
same-object pages per miss. This exposes a demand-read versus read-ahead
tradeoff to qualify with scans and hydration before changing policy.

Environment: native ARM64 release binary on macOS 26.5.2, external APFS volume,
Rust 1.97.0, SQLite 3.49.1, workspace `object_store` 0.14.1. RustFS ran in the
existing Colima ARM64 VM (8 CPUs, approximately 16 GiB), using
`1.0.0-beta.8-glibc` at digest
`sha256:040304b66e029a5cde4bed140b41513e925909839a9b912a40a98340610d1f66`.
Provider connections and provider-side caches were reused. This is a local
library probe without per-node cgroup limits, runtime ownership/CAS, followers,
concurrent application traffic, or a persistent directory cache.

Raw JSON lines and phase summaries: `32mib.log` and `256mib.log` under
`$HOME/Workspace/crabbuild-target/crab-8bc8/activation-probe/`. Binary SHA-256:
`0be9ec6e3015dff76aea2a7c769c87a0087335d265cd86a32a66cd638954ac47`.
The same directory retains source/environment metadata and stderr. Keep this
microbenchmark separate from public-action and fleet qualification.

The runners also use the implementations' pinned bundled SQLite versions:
Crab currently links SQLite 3.49.1 while the pinned Celld revision links SQLite
3.45.0. `workload_write_us` and therefore `total_us` include that difference;
use the capture and recovery subtotals when attributing work specifically to
the LTX implementations.

Run each workload matrix on the same machine, filesystem, SQLite page size,
build profile, and power state. Use the median as a compact summary, but retain
the per-round JSON when investigating variance.
