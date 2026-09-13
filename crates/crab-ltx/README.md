# crab-ltx

Embedded, synchronous SQLite WAL capture and exact LTX recovery. Crab-owned
source integration of Celld's replication mechanics; no Celld Git dependency,
Litestream daemon, network client, cloud credentials, or Tokio dependency.

Status: local replication library implemented. **Not wired into
`crab-http-server` yet.** The HTTP server still uses its existing application
storage. See the [next architecture](../crab-http-server/next-architecture/README.md)
for the publication, ownership and hard-cutover work that remains.

## Contract

| API | Local result |
| --- | --- |
| `ManagedDb::open(path, limits)` | Exclusive fresh capture session; owns control, read-lock and application-writer SQLite connections |
| `transaction(closure)` | One locally committed SQL transaction; no remote-durability claim |
| `capture()` | Ordered `CaptureBatch` containing every newly generated cut and its endpoint, including checkpoint cuts |
| `snapshot(path)` | Captures pending work, then creates a standalone `1..=txid` snapshot |
| `VerifiedLocalPlan::new(files, target, limits)` | Owns verified bytes of an explicitly selected snapshot-plus-deltas chain |
| `restore_exact(plan, path)` | Installs a new SQLite file at exactly the verified endpoint; never overwrites |
| `compact_exact(plan, path)` | Compacts that complete chain into a verified standalone snapshot; never deletes inputs |
| `close()` | Releases local connections/read lock; does not upload, publish or release a remote lease |

`SegmentInfo` includes TXID range, page size/count, pre/post rolling checksum,
encoded size and BLAKE3 digest. Persist those expectations in the server's
authenticated manifest. `LocalSegment::new` is an **unverified selection**;
`VerifiedLocalPlan::new` validates it before recovery. Changing a path after plan
construction cannot change its owned bytes. LTX CRC64 checks file structure and
database state; it is not cryptographic authentication.

The first file must be a full snapshot. Every subsequent range starts at the
previous maximum TXID plus one. Validation rejects gaps, overlaps, missing files,
wrong digests, wrong metadata/target, invalid page order/index offsets, missing
snapshot or growth pages, and checksum-disabled files. Every applied cut's
rolling database checksum is verified, not just the final trailer.

Writers emit checksum-bearing LTX v3 **sized-block** files (LTX v0.5.2 layout).
Readers accept both sized-block and older LZ4-frame files when checksummed.
Litestream v0.5.11 cannot read the sized-block layout; compatibility must not be
inferred from the unchanged file-version number. Independent local vectors test
both encodings; a full external Litestream/Celld interoperability matrix remains
a release gate. Source revision and notices: [UPSTREAM.md](UPSTREAM.md).

## Use

```rust,no_run
use crab_ltx::{Limits, ManagedDb, VerifiedLocalPlan, restore_exact};
use std::path::Path;

# fn example() -> crab_ltx::Result<()> {
// Parent directories already exist, are private, and are exclusively owned.
let limits = Limits::default();
let mut db = ManagedDb::open(Path::new("cell/repository.sqlite"), limits)?;
db.transaction(|tx| {
    tx.execute("CREATE TABLE issues (number INTEGER PRIMARY KEY, title TEXT)", [])?;
    tx.execute("INSERT INTO issues VALUES (1, 'First issue')", [])?;
    Ok(())
})?;
let captured = db.capture()?;

// Server integration goes here: upload immutable files, publish a manifest and
// prove the owner/head CAS before responding. Capture alone is not publication.
let plan = VerifiedLocalPlan::new(&captured.segments, captured.position, limits)?;
restore_exact(&plan, Path::new("recovery/repository.sqlite"))?;
db.close()?;
# Ok(())
# }
```

Runnable demonstration, from the repository root with this worktree's external
Cargo target directory configured:

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" \
  cargo run -p crab-ltx --example local_roundtrip --locked
```

It writes an issue to real SQLite, copies LTX artifacts to another local
directory, deletes the original database directory, restores and queries the
issue. This demonstrates local mechanics, **not RustFS publication**.

## Session, filesystem and execution rules

- Use a dedicated database thread or bounded blocking executor. No network
  awaits occur inside this crate. `&mut ManagedDb` serializes SQL and capture;
  the server must additionally prevent reads/writes while publication is pending.
- Treat SQL callbacks as trusted application code. Only mutate the main
  database. Do not ATTACH databases, change pager pragmas/hooks, manually
  commit/rollback, run direct checkpoints, or alter `_litestream_seq` and
  `_litestream_lock`. There is no arbitrary-SQL service or borrowed writer pool.
- SQLite's WAL hook records the application's committed frame boundary. Capture
  must reach that boundary before any checkpoint/control write can reset it.
  A damaged later commit cannot be acknowledged as an earlier valid cut.
- The private metadata directory `.<filename>-crab-ltx` is atomically claimed.
  Existing sessions are refused, including after clean close. On activation or
  capture failure, restore the authoritative plan to a **fresh local directory**
  and start a new caller-owned epoch. Local file listing never selects truth.
- No other process may mutate the database, sidecars or session directory.
  Paths are canonicalized before claiming a session; hard-linked database aliases
  remain forbidden, as they do not share SQLite's filename-derived sidecars.
  Directory ownership is local exclusion, not distributed fencing. Paths are
  UTF-8. Destination parent directories must exist; restore rejects SQLite
  sidecars and will not replace an existing destination.
- Retained artifacts are not removed by drop/close. Keep them intact until the
  coordinator resolves publication. The caller owns later session-directory
  cleanup; the library has no local prune acknowledgement or remote retention API.
- Artifact writes fsync files and their containing directory. A failed operation
  may have installed a file before directory fsync failed; treat it as ambiguous,
  not published. No power-loss guarantee beyond the filesystem's fsync contract.
  Qualified locally on macOS; Linux CI and other platform/filesystem qualification
  are separate evidence. Windows directory fsync is not currently supported.

## Resource bounds and current limits

Defaults: 256 MiB database, 512 MiB per local input/output file (including WAL),
1 GiB aggregate plan/retained captured bytes and 1,024 segments. Oversized headers
are rejected before page allocation. The managed writer has `max_page_count`;
capture checks database/WAL sizes and stops on limits. Capture errors fence the
handle. A session at its retention limit must be published/rotated by the caller.

These are admission bounds, **not an RSS or disk quota**. Snapshot capture and
restore materialize database-sized buffers. Plans retain compressed input bytes;
verification/restore/compaction may hold multiple database images and page
buffers. Each cut clones/scans the packed checksum index (eight bytes per page,
about 2 MiB per GiB at 4 KiB pages). Compaction currently verifies full input and
output images rather than providing bounded streaming memory. One failed capture
can leave additional bounded artifacts on disk before aggregate accounting
rejects its result. The server must reserve headroom and throttle aggregate cells.

Not implemented here: incremental local artifact pruning, partial-range delta
compaction, configurable public checkpoint policy, async scheduling, remote
restore planning, manifests/control CAS, ownership, metrics export, retention,
encryption/key management, application schema or HTTP integration.

## Verification

```sh
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" cargo test -p crab-ltx --locked
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" cargo clippy -p crab-ltx --all-targets --locked -- -D warnings
CARGO_TARGET_DIR="$HOME/Workspace/crabbuild-target/crab-b347" cargo fmt -p crab-ltx -- --check
```

Tests exercise real SQLite commit/rollback, passive/truncate checkpoints,
auto-vacuum shrink/regrowth, cold restore, process kill followed by source loss,
snapshot/compaction byte identity, an independent bitwise CRC oracle, both page
encodings, corrupt WAL capture fencing, malformed files/chains and limits.
The workspace's existing `cargo test --workspace` CI includes this member.

Initial local evidence (2026-09-13, macOS): 8 unit tests, 14 integration tests
(including the subprocess fixture entry point), and 1 compiling doc-test passed.
Strict Clippy, minimal-feature/all-target compilation, formatting and the local
round-trip example passed. Both copied license texts match upstream SHA-256.

Remaining qualification: broad affected-consumer/platform CI, upstream golden
fixture corpus/external interoperability, fuzzing, filesystem I/O/power-loss
faults, measured memory/latency and the complete RustFS/HTTP owner-publication
protocol. No production-ready or browser-parity claim is made by these tests.
