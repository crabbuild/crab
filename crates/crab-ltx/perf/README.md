# `crab-ltx` versus `celld-ltx`

This directory contains a small, reproducible local-filesystem comparison of
the two in-process LTX implementations. It is intentionally outside the Crab
Cargo workspace: `crab-ltx` uses `rusqlite` 0.34 while the pinned Celld source
uses `rusqlite` 0.31, and Cargo cannot link two `libsqlite3-sys` versions in
one process. Each runner is therefore its own package and emits JSON.

The Celld runner is pinned to the revision documented in
[`UPSTREAM.md`](../UPSTREAM.md):

`10cb1303dac710dcb3b557e318e08c855261f68b`

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
  file sync; `capture_parent_sync_us` is the directory-entry sync that makes
  the atomic rename durable. The other fields split position resolution, WAL
  reads, page collection, encoding, and local writes.
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

The Celld runner accepts `--sync-parent` as a diagnostic contract-normalization
mode. After each upstream `Db::sync()`, it syncs Celld's L0 directory before
recording capture completion. It also syncs the destination directory after
compaction and restore installation. This is not pinned Celld behavior and
must be reported separately; it answers what the local comparison looks like
when both runners pay parent-directory barriers for installed artifacts.

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

The runners also use the implementations' pinned bundled SQLite versions:
Crab currently links SQLite 3.49.1 while the pinned Celld revision links SQLite
3.45.0. `workload_write_us` and therefore `total_us` include that difference;
use the capture and recovery subtotals when attributing work specifically to
the LTX implementations.

Run each workload matrix on the same machine, filesystem, SQLite page size,
build profile, and power state. Use the median as a compact summary, but retain
the per-round JSON when investigating variance.
