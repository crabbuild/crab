# `crab-ltx` examples

These programs demonstrate the canonical Rust Cell persistence boundary. Local
capture remains usable without the `replica` feature; object-store examples use
`CellReplica` and never publish a standalone epoch head.

| Example | Demonstrates |
| --- | --- |
| `local_roundtrip` | Local WAL capture, verified plan construction, exact restore, and SQL verification |
| `rustfs_cell_replica_scale_load` | Cell-scoped immutable publication, source deletion, exact restore, and full-range compaction with checksum verification |

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
