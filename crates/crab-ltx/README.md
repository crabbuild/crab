# `crab-ltx`

`crab-ltx` is an embeddable Rust library for capturing SQLite WAL commits as
checksum-bearing LTX files and recovering an exact, verified database state.
With the optional `replica` feature, it can prepare immutable Crab Cell roots
in object storage and open them for exact restore or sparse SQL.

It is **not Litestream packaged as a Rust crate**. Litestream is a standalone
sidecar that monitors a database and operates its replication lifecycle.
`crab-ltx` runs inside the application and leaves scheduling, storage
configuration, authority, retention, and request acknowledgement to its host.

![Litestream sidecar and crab-ltx embedded architecture](diagram/litestream-vs-crab-ltx.svg)

## Choose the right tool

Use Litestream when you want an operational SQLite backup tool with a CLI,
configuration file, background synchronization, provider integrations,
compaction, snapshots, and retention.

Use `crab-ltx` when a Rust service must:

- execute SQLite writes and capture their exact WAL commit boundary in one
  owned session;
- validate every LTX segment before selecting it for recovery;
- publish immutable objects behind an application-specific authority CAS;
- restore a caller-selected state without listing a bucket for “latest”; or
- lazily open an authenticated Cell root and hydrate it through SQLite.

Do not use `crab-ltx` as a drop-in Litestream client or point the two systems at
the same replica prefix. They share LTX concepts, not a publication protocol.

## Litestream comparison

This comparison was checked against the official
[Litestream v0.5.17 release](https://github.com/benbjohnson/litestream/releases/tag/v0.5.17),
its [`DB`](https://github.com/benbjohnson/litestream/blob/v0.5.17/db.go),
[`Replica`](https://github.com/benbjohnson/litestream/blob/v0.5.17/replica.go),
and [`Store`](https://github.com/benbjohnson/litestream/blob/v0.5.17/store.go)
implementations, and the official
[How it works](https://litestream.io/how-it-works/) guide.

| Concern | Litestream v0.5.17 | `crab-ltx` |
| --- | --- | --- |
| Deployment | Standalone process next to the application | Library linked into a Rust process |
| Write ownership | Observes an application-owned SQLite database through SQLite and WAL files | All supported SQL writes pass through an exclusive `ManagedDb` session |
| Progress | Background monitor loops sync WAL, upload LTX, compact levels, create snapshots, and enforce retention | The host explicitly calls `capture`, `checkpoint`, `prepare`, compaction, and pruning |
| Remote state | `ReplicaClient` lists LTX levels and selects ranges for restore | `CellReplica` opens an exact `RootRef`; it never discovers truth by listing objects |
| Durability boundary | A successful replica sync advances Litestream's replica position | Uploaded immutable objects are only a proposal; the host must publish the root with Cell authority before acknowledging |
| Storage providers | Litestream owns its CLI/config provider integrations | The host supplies an existing `crab-storage` `Store` and `CellStorageLayout` |
| Restore selection | Latest, TXID, or timestamp is resolved from replica LTX files | The caller supplies a verified local plan or an authority-pinned Cell root |
| Retention | Built-in snapshot and LTX retention monitors | Remote pinning, retention, and garbage collection are host policy |
| Format | Uses `superfly/ltx` v0.5.2 | Writes checksum-bearing LTX v3 files using the v0.5.2 sized-block layout and reads that layout plus older checksummed LZ4-frame files |

The matching LTX dependency means current Litestream can parse the sized-block
encoding used here. It does **not** prove end-to-end interoperability: Crab adds
its own BLAKE3-bound segment metadata, authenticated Cell directory, immutable
root schema, and authority protocol. External Litestream/Celld golden-vector
qualification remains a release gate. See [UPSTREAM.md](UPSTREAM.md) for source
lineage and the exact compatibility boundary.

## Lifecycle

The embedding runtime owns the steps around the library calls. In particular,
`prepare` does not publish a mutable head and `capture` does not mean remote
durability.

![Capture, publish, and recover sequence](diagram/capture-publish-recover.svg)

The safe write path is:

1. Run a transaction through `ManagedDb`.
2. Call `capture()` to produce one or more ordered local LTX segments.
3. Upload and verify them with `CellReplica::prepare()`.
4. Publish the returned root through the embedding runtime's owner/head CAS.
5. Only after that CAS is durable, acknowledge the mutation and prune the exact
   captured batch.

Recovery reverses the boundary: load the authority-pinned `RootRef`, verify its
complete immutable object graph, then restore it or activate sparse SQL.

## Features

| Feature | Default | Adds |
| --- | --- | --- |
| none | yes | Local capture, checkpointing, snapshots, exact verification, restore, and compaction |
| `replica` | no | `crab-storage` transport, Cell roots, bundles, remote compaction, exact-root restore, sparse SQL, and hydration |

The crate is currently an unpublished workspace library.

## Local capture and exact restore

The following example is compiled as a Rust doc test. Both destination
directories exist, and the restored database path does not.

```rust,no_run
use crab_ltx::{Limits, ManagedDb, VerifiedLocalPlan, restore_exact};

fn main() -> crab_ltx::Result<()> {
    let source = tempfile::tempdir()?;
    let restored = tempfile::tempdir()?;
    let limits = Limits::default();
    let database_path = source.path().join("repository.sqlite");

    let mut database = ManagedDb::open(&database_path, limits)?;
    database.transaction(|transaction| {
        transaction.execute(
            "CREATE TABLE issues (number INTEGER PRIMARY KEY, title TEXT NOT NULL)",
            [],
        )?;
        transaction.execute(
            "INSERT INTO issues VALUES (1, 'Recover this issue from LTX')",
            [],
        )?;
        Ok(())
    })?;

    let captured = database.capture()?;
    let plan = VerifiedLocalPlan::new(
        &captured.segments,
        captured.position,
        limits,
    )?;
    let restored_path = restored.path().join("repository.sqlite");
    let restored_position = restore_exact(&plan, &restored_path)?;

    assert_eq!(restored_position, captured.position);
    database.close()?;
    Ok(())
}
```

`LocalSegment::new` only describes a selected file and its expected metadata.
It is not trusted until `VerifiedLocalPlan::new` has read the bytes, checked the
BLAKE3 digest and LTX structure, verified the complete checksum-linked chain,
and reconstructed the requested endpoint.

Run the complete local demonstration from the repository root:

```sh
export CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-your-worktree"
cargo run -p crab-ltx --example local_roundtrip --locked
```

It writes to real SQLite, copies the resulting LTX artifacts across the
transport boundary, deletes the source directory, restores the database, and
queries the recovered row.

## Preparing a Cell root

Enable `replica` and construct `CellReplica` with the application's existing
`crab_storage::Store` and `CellStorageLayout`. Provider credentials, retries,
leases, and authority stay outside this crate.

```rust,ignore
use crab_ltx::{CellReplica, ManagedDb, PreparedRoot, RootRef};

async fn prepare_root(
    database: &mut ManagedDb,
    replica: &CellReplica,
    previous: Option<&RootRef>,
    commit_sequence: u64,
    schema: u32,
) -> crab_ltx::Result<PreparedRoot> {
    let captured = database.capture()?;
    let prepared = replica
        .prepare(previous, &captured, commit_sequence, schema)
        .await?;

    // `prepared.root()` is a proposal. The embedding runtime must publish it
    // with its owner/head CAS before acknowledging the mutation or pruning
    // `captured`.
    Ok(prepared)
}
```

`CellReplica::prepare` writes only immutable, content-addressed objects. The
embedding runtime publishes `PreparedRoot::root()` through `crab-cell-runtime`
authority. After durable publication it may call
`ManagedDb::prune_captured(&captured)` for that exact acknowledged batch.

The live RustFS example exercises Cell publication, sparse activation,
compaction, source deletion, and exact recovery. See the
[examples guide](examples/README.md) before running it against a disposable
bucket.

## Core API

### Local capture and recovery

| API | Contract |
| --- | --- |
| `ManagedDb::open` | Claims a fresh exclusive session and owns the writer, control, and read-lock SQLite connections |
| `ManagedDb::transaction` | Commits one local SQL transaction; does not claim remote durability |
| `ManagedDb::capture` | Returns every new ordered cut plus its exact TXID/checksum endpoint |
| `ManagedDb::checkpoint` | Captures a barrier, runs the selected SQLite checkpoint, and returns every generated cut |
| `ManagedDb::snapshot` | Returns an independent full snapshot plus any pending captured cuts |
| `VerifiedLocalPlan::new` | Owns and verifies the complete selected snapshot-plus-delta chain |
| `restore_exact` | Installs a fresh database at exactly the verified endpoint; never overwrites |
| `compact_exact` | Produces a verified full snapshot without deleting its inputs |
| `ManagedDb::resume` | Restores a verified plan into a fresh session and continues its TXID/checksum lineage |

### Cell replication (`replica`)

| API | Contract |
| --- | --- |
| `CellReplica::prepare` | Verifies captured cuts and prepares an immutable successor root |
| `CellReplica::prepare_bundle` | Selects and verifies this Cell's rows from a shared bundle |
| `CellReplica::prepare_compaction` | Rewrites an exact range into a representation-only prepared root |
| `CellReplica::open_root` | Reopens one exact root and verifies its scope, chain, metadata, and directory |
| `VerifiedRoot::restore` | Streams an exact verified database into a fresh destination |
| `VerifiedRoot::paged` | Opens authenticated page and page-run reads |
| `CellPagedDatabase::prepare_writable` | Seeds a fresh sparse writable activation at the root's exact position |
| `ManagedDb::hydrate_step` | Resolves a bounded number of missing sparse pages on the owner-controlled database worker |
| `CellReplica::reachable_objects` | Returns the verified immutable dependency set for pinning and collection |

## Safety model

`crab-ltx` fails closed around state selection and reconstruction:

- The first local plan segment must be a full snapshot. Later segments must be
  contiguous, checksum-linked, ordered, and consistent in page size.
- Every segment's declared size, BLAKE3 digest, LTX checksum, page ordering,
  page coverage, and pre/post database checksum is verified.
- Restore and compaction create a new destination and never replace an existing
  database or consult sidecars, local listings, or object listings for truth.
- A capture/checkpoint error fences the managed handle when the commit boundary
  can no longer be proven.
- Cell objects are scoped to one Cell incarnation. A `PreparedRoot` is not a
  lease, authority update, or durable response gate.
- Cancellation does not roll back work already dispatched to blocking or
  object-store workers. The host must await or reconcile the exact root before
  retrying.

LTX CRC64 protects file structure and rolling database state. It is not a
cryptographic authenticator. Crab manifests and Cell objects add BLAKE3 digests;
the embedding service remains responsible for authenticating the manifest or
authority record that selects them.

## Host responsibilities

The application must provide the policy a sidecar would normally own:

- serialize SQL and publication for each database;
- keep all mutations inside `ManagedDb` and avoid direct checkpoints, `ATTACH`,
  pager-changing pragmas, and edits to `_litestream_seq` or `_litestream_lock`;
- publish roots through owner/incarnation/sequence authority before responding;
- schedule capture, checkpoint, hydration, compaction, and retries;
- configure provider access through `crab-storage`;
- budget local disk, scratch space, blocking work, remote I/O, and active SQLite
  connections; and
- pin live roots and own remote retention and garbage collection.

Local calls are synchronous and should run on a dedicated database thread or a
bounded blocking executor. `&mut ManagedDb` serializes access within one handle;
it does not create a distributed lock.

The database path, SQLite sidecars, and private
`.<filename>-crab-ltx` directory must have one owner. Parent directories must
already exist. Reopening a prior session directory is intentionally refused;
recover the authoritative plan or Cell root into a fresh directory instead.

## Resource limits

`Limits::default()` admits a 256 MiB database, 64 MiB per capture, 512 MiB per
input/output file, 1 GiB across a plan or retained captures, and 1,024 segments.
These are per-operation correctness bounds, not an RSS quota.

Each open `ManagedDb` retains three SQLite connections with a 64 KiB page-cache
target per connection. `Host` can share disk, I/O, blocking-job, recovery,
dirty-job, scratch, and telemetry admission across many databases. Sparse page
read-ahead is capped at 64 pages or 1 MiB per request, and the shared decoded
page cache is capped at 8 MiB.

Large-database and multi-tenant capacity still require workload-specific
measurement. The existing tests prove bounded correctness behavior; they do not
establish a 10,000-database or 1,000-TPS production capacity claim.

## Verification

From the repository root, choose a target directory unique to this checkout:

```sh
export CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-your-worktree"

cargo test -p crab-ltx --locked
cargo test -p crab-ltx --features replica --locked
cargo test -p crab-ltx --doc --features replica --locked
cargo clippy -p crab-ltx --all-targets --locked -- -D warnings
cargo clippy -p crab-ltx --features replica --all-targets --locked -- -D warnings
cargo fmt -p crab-ltx -- --check
```

The suite covers real SQLite commits and rollbacks, checkpoints, database
growth and truncation, cold restore, process death followed by source loss,
snapshot/compaction byte identity, both supported page encodings, malformed
chains, checksum failures, exact Cell roots, bundles, sparse activation,
hydration, remote compaction, and provider/cache lifecycle boundaries.

Still required before a broad production-readiness claim: external
Litestream/Celld fixture interoperability, fuzzing, exhaustive filesystem and
power-loss faults, broader platform/provider CI, and measured latency, memory,
scratch, and concurrency qualification.

## Provenance and compatibility

This is Crab-owned, modified source derived from the Apache-2.0 Celld LTX
implementation—not an unmodified vendor directory or a floating dependency.
The readable source inventory, upstream revisions, deliberate adaptations,
license obligations, and future-import checklist live in
[UPSTREAM.md](UPSTREAM.md).

Compatibility summary:

- readers accept checksum-bearing sized-block LTX and the older checksummed
  LZ4-frame representation;
- writers emit only the v0.5.2 sized-block representation;
- checksum-disabled LTX is rejected;
- retired standalone Crab epoch-head and page-map layouts are not read; and
- Cell root JSON, authenticated directories, bundle envelopes, and authority
  records are Crab-specific contracts.
