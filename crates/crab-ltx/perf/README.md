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

A hot Cell at 100 commands per second would issue roughly 500-1,300 immutable
PUTs per second, which is why the runtime admits against one pending-publication
byte high-water mark per Cell plus the 32-segment compaction debt that folds the
root graph. Publication stays one serialized root per command because the object
path is the long-term durability authority and the node-log fleet proof releases
the command earlier; on this loopback RustFS path one command costs roughly
0.09-0.15 seconds of provider work, so a Cell without a fleet proof is
provider-latency bound, not CPU bound. Coalescing several commands into one root
would save metadata objects, not bodies, and the fleet-proof race already
absorbs most of that latency for enrolled nodes. Multi-Cell concurrency,
cloud-bucket p99, and retention cost remain unmeasured.

The runners also use the implementations' pinned bundled SQLite versions:
Crab currently links SQLite 3.49.1 while the pinned Celld revision links SQLite
3.45.0. `workload_write_us` and therefore `total_us` include that difference;
use the capture and recovery subtotals when attributing work specifically to
the LTX implementations.

Run each workload matrix on the same machine, filesystem, SQLite page size,
build profile, and power state. Use the median as a compact summary, but retain
the per-round JSON when investigating variance.
