# `crab-ltx` versus `celld-ltx`

This directory contains a small, reproducible local-filesystem comparison of
the two in-process LTX implementations. It is intentionally outside the Crab
Cargo workspace: `crab-ltx` uses `rusqlite` 0.34 while the pinned Celld source
uses `rusqlite` 0.31, and Cargo cannot link two `libsqlite3-sys` versions in
one process. Each runner is therefore its own package and emits JSON.

The Celld runner is pinned to the revision documented in
[`UPSTREAM.md`](../../UPSTREAM.md):

`10cb1303dac710dcb3b557e318e08c855261f68b`

## Run it

The script uses release builds, one warmup round, and five measured rounds by
default. It stores build output under the mounted workspace volume when
`CARGO_TARGET_DIR` is not supplied.

```bash
crates/crab-ltx/perf/compare/run.sh
```

Override the workload without editing the harness:

```bash
LTX_TRANSACTIONS=512 \
LTX_PAYLOAD_BYTES=16384 \
LTX_ROUNDS=7 \
LTX_WARMUP=2 \
crates/crab-ltx/perf/compare/run.sh
```

The binaries also run directly when a single side is useful:

```bash
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-ltx-perf" \
  cargo run --release \
  --manifest-path crates/crab-ltx/perf/compare/crab/Cargo.toml -- \
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
- `verify_us`: Crab's explicit owned-input plan verification. Celld reports
  zero because its compactor does not expose an equivalent call.
- `compact_us`: local LTX compaction, including source listing, reads, merge,
  and output fsync for Celld; Crab's `compact_exact` merge and output install.
- `compact_verify_us`: Crab's verification of the compacted output. Celld's
  destination-level continuity check is included in `compact_us`.
- `restore_us`: end-to-end restore wall time. Celld additionally reports its
  plan, download, and apply sub-timings.
- `input_ltx_bytes` and `compacted_ltx_bytes`: storage amplification evidence.

The harness checks the restored row count and SQLite integrity in every round;
it does not include those checks in the reported restore timer.

For a phase comparison, add Crab's `verify_us` to its `compact_us` (and, when
you want the fully checked path, `compact_verify_us`) before comparing it with
Celld's `compact_us`. Crab intentionally verifies and owns every input before
the merge; Celld's pinned `ReplicaCompactor` validates the range shape and
destination continuity but does not expose the same input-plan verification
phase. The `end_to_end_us` field already includes all reported phases for each
implementation, so it is the safer headline number.

## Scope

This is a local mechanics benchmark, not a claim about the complete durability
protocol. It does not measure object-store latency, network retries, Cell
authority/owner-head CAS, acknowledgement ordering, retention, scheduled
multi-level compaction, or either implementation's paged VFS. Those paths have
different contracts and need a second harness with the same object-store and
authority model before they can be compared fairly.

Run each workload matrix on the same machine, filesystem, SQLite page size,
build profile, and power state. Use the median as a compact summary, but retain
the per-round JSON when investigating variance.
