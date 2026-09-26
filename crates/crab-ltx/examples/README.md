# `crab-ltx` examples

These programs demonstrate the canonical Rust Cell persistence boundary. Local
capture remains usable without the `replica` feature; object-store examples use
`CellReplica` and never publish a standalone epoch head.

| Example | Demonstrates |
| --- | --- |
| `local_roundtrip` | Local WAL capture, verified plan construction, exact restore, and SQL verification |
| `rustfs_cell_replica_scale_load` | Immutable publication, sparse activation phase measurements, source deletion, exact restore, and full-range compaction with checksum verification |
| `power_cut_probe` | Exact capture and clean-continuation checkpoints for an external power-cut controller |

Run the local example from the repository root:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo run -p crab-ltx --example local_roundtrip --locked
```

## RustFS scale workload

Provision a disposable, pre-created RustFS bucket and export its endpoint and
credentials outside tracked files:

```sh
export CRAB_LTX_TEST_BUCKET=crab-ltx-examples
export CRAB_LTX_TEST_ENDPOINT=http://127.0.0.1:9000
export AWS_ACCESS_KEY_ID="<RustFS access key>"
export AWS_SECRET_ACCESS_KEY="<RustFS secret key>"
export CRAB_LTX_WORKLOAD_ROOT="$HOME/Workspace/crab-ltx-workloads"
```

The default workload grows a 5 GiB incompressible SQLite database. Keep source,
restore, and compaction scratch files on the external workspace volume:

```sh
CRAB_CELL_LTX_TARGET_BYTES=$((5 * 1024 * 1024 * 1024)) \
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo run --release -p crab-ltx --features replica \
  --example rustfs_cell_replica_scale_load --locked
```

The example uses a unique Cell storage prefix, prepares each capture through
`CellReplica`, prunes only the exact batch after the prepared root is verified,
deletes the source database, restores the published root, compacts its complete
range, and compares source and restored BLAKE3/length. It never lists or deletes
remote objects. Run it only against a disposable bucket and let the bucket
owner clean up remote data.

After deleting the source, the example emits 18 JSON lines with
`measurement: "sparse_activation"`: three rounds at one, four, and eight shared
I/O slots, each with a fresh metadata cache and a second activation reusing it.
Slot order reverses in the middle round. Each line separates exact-root open,
checksum preparation, writable open (including blocking-worker dispatch), and
the first row-length query. Each phase reports elapsed microseconds, storage
read calls, and returned bytes. Hydrated-page and page-fault counts show how
much of the database the initial SQL access actually touched.

Fresh `Store` identities exclude previously cached immutable metadata; provider
connections and RustFS caches remain warm. The reused pass retains bounded
metadata caches and uses a fresh sparse destination, so a database larger than
the metadata cache may still require origin reads. There is no persistent
directory cache in this probe. Read calls include the Store read API's GET,
range, and HEAD observations, not provider-internal retries. Three samples per
slot/cache pair are diagnostics, not p95/p99 or a supported latency limit.

For a small real-provider smoke, use `CRAB_CELL_LTX_TARGET_BYTES=33554432`
with the same command. The final load, exact restore, compaction, and compacted
restore timers cover separate phases; digest comparison is outside restore
timing. Retain stdout with the source revision, image/provider version,
architecture, filesystem, and resource limits when comparing runs.

## Public API exercised

With `--features replica`, the canonical surface is:

| Type | Main API | Purpose |
| --- | --- | --- |
| `CellReplica` | `new`, `prepare`, `prepare_bundle`, `prepare_compaction`, `open_root` | Prepare, reopen, and compact immutable Cell roots |
| `PreparedRoot` | `root`, `predecessor`, `verified` | Carry an exact proposal into Cell authority CAS |
| `CellPagedDatabase` | `read_page`, `read_run`, `prepare_writable` | Authenticate sparse reads and seed a writable activation |
| `CellWritableDatabase` | `open_writable` | Open a fresh sparse SQLite writer at one exact root |
| `Db` | `hydration`, `hydrate_step`, `take_io_error`, `prune_captured` | Drive bounded hydration and release exact acknowledged captures |
| `Hydration` | `resolved`, `total`, `faults`, `complete` | Report sparse activation progress |
| `bundle` | `Bundle::encode`, `decode`; `BundleEntry::for_cell` | Carry verified Cell-scoped LTX ranges into recovery overlays |

Mutable owner/epoch/root publication remains a `crab-cell-runtime` authority
operation. `crab-ltx` prepares immutable bytes and never acknowledges an HTTP
request or changes mutable Cell control.

## Dedicated-host power-cut probe

`power_cut_probe` has a writer stage and a verifier stage for each local
durability contract. Run it on a dedicated fault host with the probe directory
on a disposable test filesystem. Keep the controller and its captured stdout
on a separate, unaffected device. Before starting each writer, create its fresh
test directory and sync that directory's parent; otherwise losing the test
directory's own unsynced name can masquerade as an LTX failure. The writer
emits `READY_CAPTURE` immediately
after a successful standalone `capture()` and parks with SQLite still open;
`READY_RESUME` follows a successful `persist_continuation()` and `close()`.
The controller must cut host power or inject the planned block-device fault
when it observes the relevant marker. A process signal alone is only a smoke
test, not power-loss evidence. For a block-device fault, verify only after a
fresh mount without the writer's warm page cache; otherwise cached bytes can
hide lost device writes.

Build once on that host with its own mounted target directory:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-<fault-host>" \
  cargo build --release -p crab-ltx --features replica \
  --example power_cut_probe --locked
```

For the capture case, use a fresh directory on the disposable device. Record
the `SEGMENT`, `TXID`, `CHECKSUM`, and `DIGEST` lines off-device before cutting
at `READY_CAPTURE`. After reboot, use those exact values:

```sh
power_cut_probe capture-write <fresh-test-directory>
power_cut_probe capture-verify <test-directory> <recorded-segment-path> \
  <recorded-txid> <recorded-checksum> <recorded-digest>
```

The verifier checks the recorded LTX digest, constructs an exact verified
plan, restores it into a fresh file, and reads the expected SQL value. For
clean continuation, use a different fresh directory, record its `TXID`,
`CHECKSUM`, and `DIGEST`, and cut at `READY_RESUME`:

```sh
power_cut_probe resume-write <fresh-test-directory>
power_cut_probe resume-verify <test-directory> <recorded-txid> \
  <recorded-checksum> <recorded-digest>
```

The resume verifier first checks the complete database BLAKE3 digest, then
moves the continuation to a fresh path, verifies its exact recorded position
and SQL value, and captures the next transaction. Each verifier is one-shot:
start from a new directory for every cut. Save host/kernel, filesystem and
mount options, device cache mode, fault mechanism, cut marker and timing,
writer stdout, verifier stdout/stderr/exit status, and the expected and
observed endpoint with the off-device evidence. Neither this probe nor a
local process-kill smoke replaces the dedicated-host run in Plan 035.
