# `crab-ltx` examples

These programs demonstrate the library boundary with real SQLite databases.
The focused replica examples use an in-memory object store and need no cloud
credentials. The million-record workloads use a real RustFS endpoint through
the same `crab_storage::Store` used by production callers.

| Example | Demonstrates |
| --- | --- |
| `local_roundtrip` | Local WAL capture, verified plan construction, exact restore, and SQL verification |
| `replica_roundtrip` | Immutable upload, conditional head publication, source loss, remote restore, and SQL verification |
| `paged_read` | Read-only SQLite queries over authenticated object-store page ranges without a local database file |
| `sparse_writer` | Externally allocated epoch inheritance, writable sparse activation, publication, and exact restore |
| `compact_history` | Full-chain compaction plus reopening and restoring an immutable historical manifest |
| `repository_replication_lifecycle` | Complete per-repository lifecycle: write, capture, publish, paged read, epoch handoff, sparse write, compact, historical reopen, and exact restore |
| `rustfs_replication_scale_load` | The 1M/10M/100M profiles with real RustFS publication, pruning, source loss, remote verification, and throughput reporting |
| `rustfs_paged_read_scale_performance` | The 1M/10M/100M profiles followed by cold RustFS head/page-map opening, paged point/range queries, and a full aggregate scan |

From the repository root:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo run -p crab-ltx --example local_roundtrip --locked

CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo run -p crab-ltx --features replica --example replica_roundtrip --locked

CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo run -p crab-ltx --features replica --example paged_read --locked

CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo run -p crab-ltx --features replica --example sparse_writer --locked

CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo run -p crab-ltx --features replica --example compact_history --locked

CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo run -p crab-ltx --features replica --example repository_replication_lifecycle --locked
```

## Million-record workloads

Provision a disposable, pre-created RustFS bucket and export its endpoint and
credentials outside tracked files:

```sh
export CRAB_LTX_TEST_BUCKET=crab-ltx-examples
export CRAB_LTX_TEST_ENDPOINT=http://127.0.0.1:9000
export AWS_ACCESS_KEY_ID="<RustFS access key>"
export AWS_SECRET_ACCESS_KEY="<RustFS secret key>"
export CRAB_LTX_WORKLOAD_ROOT="$HOME/Workspace/crab-ltx-workloads"
```

Then run the workloads with optimizations enabled:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo run --release -p crab-ltx --features replica \
  --example rustfs_replication_scale_load --locked -- 1m

CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-main" \
  cargo run --release -p crab-ltx --features replica \
  --example rustfs_paged_read_scale_performance --locked -- 10m
```

Both workloads accept exactly one scale profile: `1m` (the default), `10m`, or
`100m`. Every profile uses about 100 transactions by increasing the batch size
from 10,000 to 100,000 and then 1,000,000 records. This bounds head growth while
each captured cut is published and its local file is pruned only after the CAS
receipt. The read workload deletes the source database before reopening the
remote head and running SQLite queries.

| Profile | Database limit | Plan limit | Verification |
| --- | ---: | ---: | --- |
| `1m` | 256 MiB | 2 GiB | Exact restore and aggregate query |
| `10m` | 2 GiB | 4 GiB | Exact restore and aggregate query |
| `100m` | 16 GiB | 32 GiB | Paged aggregate after source deletion |

The `10m` and `100m` profiles require `CRAB_LTX_WORKLOAD_ROOT`; put it on a
dedicated volume with enough space for source and recovery files. The `100m`
load deliberately uses paged verification instead of full restore because the
current recovery path retains database-sized buffers. It is an opt-in soak
workload, not evidence that 100M full recovery has bounded memory.

An isolated loopback RustFS qualification completed both `10m` workloads with
an 838,262,784-byte database, 204,654 pages and 200 published segments. The load
path sustained 42,339 records/second and completed exact restore plus aggregate
verification in 36.4 seconds. A separate cold paged run opened the head in 18
milliseconds, constructed its page map in 264 milliseconds, completed a point
lookup in 9.2 milliseconds, counted an indexed repository range in 209
milliseconds and scanned the 10M-row aggregate in 78.0 seconds. These are
single-host qualification observations, not portable performance promises.
The `100m` profile has not been executed or qualified.

These are reproducible executable workloads, not statistically rigorous
benchmarks. They use `crab-storage`'s real S3-compatible client, including
RustFS network I/O and conditional head publication. Each invocation prints a
unique repository prefix so an old head cannot collide with a new measurement.
The examples never list or delete remote objects; use only a disposable bucket
and let the bucket owner remove its contents afterward. Run several iterations
under representative CPU, disk, memory, and network limits before using the
results for capacity planning.

## Public API

The default feature set is the synchronous local SQLite/LTX engine. Enable
`replica` for object-store and async APIs.

| Type or function | Main API | Purpose |
| --- | --- | --- |
| `ManagedDb` | `open`, `open_with_host`, `transaction`, `capture`, `checkpoint`, `snapshot`, `position`, `path`, `close` | Own one exclusive SQLite writer and turn committed WAL state into retained LTX cuts |
| `VerifiedLocalPlan` | plan construction through `Host::verify`, `position` | Verify an exact local LTX lineage before restore or compaction |
| Recovery | `restore_exact`, `compact_exact`; `Host::restore`, `Host::compact` | Restore an exact database image or create a full compacted LTX snapshot |
| Data contracts | `Position`, `SegmentInfo`, `LocalSegment`, `CaptureBatch`, `Limits`, `CheckpointMode` | Describe lineage, immutable files, capture results, admission limits, and checkpoint policy |
| Errors and SQLite | `Result`, `CrabError`, re-exported `rusqlite` | Preserve typed storage, SQLite, checksum, fencing, and limit failures |

With `--features replica`:

| Type | Main API | Purpose |
| --- | --- | --- |
| `Replica` | `new`, `with_host`, `head`, `replicate` | Bind one repository prefix plus one epoch; publish its head with compare-and-swap |
| `Replica` recovery | `open_exact`, `restore`, `resume` | Reopen an immutable manifest, restore a file, or start a full local writer at that exact cut |
| `Replica` handoff | `inherit` | Create a new epoch pinned to an explicitly supplied predecessor without copying LTX objects |
| `Replica` reads | `paged` | Build an authenticated page map without downloading full LTX bodies |
| `Replica` maintenance | `compact`, `compact_range`, `bundle`, `replicate_bundle` | Compact chains or change their immutable transport representation |
| `ReplicaHead` | `manifest_digest`, `position`, `segment_count`, `segments` | Return the pinned manifest identity, lineage position, and exact object expectations |
| `PagedDatabase` | `position`, `page_size`, `page_count`, `read_page`, `read_run`, `open_sqlite`, `open_writable` | Read verified ranges, run read-only SQLite, or create a writable sparse activation |
| `PagedConnection` | `connection`, `take_read_error` | Access the read-only `rusqlite::Connection` and recover the underlying provider/checksum error |
| Sparse `ManagedDb` | `hydration`, `hydrate_step`, `take_io_error`, `prune_published` | Observe/materialize inherited pages, recover VFS errors, and delete acknowledged local cuts |
| `Hydration` | `resolved`, `total`, `faults`, `complete` | Report sparse activation progress and on-demand page faults |
| `CompactionSchedule` | `new`, `next_due`, `run_due` | Let an owner drive bounded level compaction from its own monotonic scheduler |
| `bundle` | `Bundle::encode`, `decode`, `rows`, `bytes`, `segment`; `BundleEntry`, `BundleRow` | Aggregate verified LTX files while keeping publication atomic per repository head |

`Host` supplies injectable filesystem, clock, SQLite VFS, blocking executor, and
shared concurrency limits. Most applications should use its defaults and inject
only facilities they actually own.

## Service boundary

`crab-ltx` deliberately does not provide HTTP endpoints, Git protocol handling,
repository discovery, owner election, leases, stale-owner fencing, epoch
allocation, authentication, authorization, request routing, retries, retention,
garbage collection, or provider credential configuration. The service must do
those jobs and construct the `crab_storage::Store` and per-repository
`StoreLayout`.

In particular, `ManagedDb::transaction` success is only a local SQLite commit.
The server may acknowledge a durable mutation only after `Replica::replicate`
returns a new `ReplicaHead`. On an ambiguous publication failure it must retain
the `CaptureBatch`, re-read `Replica::head`, and reconcile exact positions and
segments. A `ReplicaHead` is an epoch-local compare-and-swap receipt, not an
ownership lease.
