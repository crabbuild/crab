# crab-staging

`crab-staging` is Crab's durable local write buffer for chunks produced while
cleaning files and preparing a push. It stores payloads in append-only segment
files and records their locations in SQLite so deduplication, recovery, and
push planning can share one local source of truth.

## Why it exists

A clean operation can produce many chunks before the remote push is ready.
Writing one file per chunk creates filesystem overhead and makes a crash easy
to mishandle. Staging instead gives the pipeline a durable boundary: either a
chunk locator and its bytes become visible together, or recovery removes both.

The staging area also separates local mutation from remote publication. A push
can read and verify staged data, build recipes and plans, upload immutable
objects, and retire rows only after publication succeeds.

## Architecture

```text
clean/filter process
        │ pre-register file + append chunk batch
        ▼
segments/*.seg  ── payload bytes, CRC/BLAKE3 checked
index.db        ── file/chunk locators, pending and durable boundaries
lockfile        ── process-wide advisory flock
        │ flush, verify, plan, retire
        ▼
push upload / recipe publication
```

`StagingArea::open` acquires an exclusive process lock, runs migrations and
crash recovery, and opens the current segment. `StagingAreaReadOnly` acquires
a shared lock for concurrent push readers. In-process index and writer locks
are scoped so no synchronous mutex is held across an async suspension.

`stage_chunks_batch` is the hot path: it deduplicates, appends, inserts
locators atomically, and performs the configured flush check. `flush_pending`
fsyncs the segment and records the durable boundary. Reads verify both the
record checksum and the requested BLAKE3 hash. Recipes, push plans, streaming
helpers, compaction, verification, and retirement build on these primitives.

## Usage

The smallest complete staging cycle is:

```rust
use crab_staging::StagingArea;
use crab_xet::hash::compute_data_hash;
use std::path::PathBuf;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let staging = StagingArea::open(PathBuf::from(".crab/staging")).await?;
let data = b"hello Crab";
let chunk_hash = compute_data_hash(data);
let file_hash = compute_data_hash(data);

staging.pre_register_file(&file_hash, data.len() as u64)?;
staging
    .stage_chunks_batch(&[(&chunk_hash, &data[..])], &file_hash, 0)
    .await?;
staging.flush_pending().await?;
assert_eq!(
    staging.get_chunk(&chunk_hash).await?.unwrap(),
    bytes::Bytes::copy_from_slice(data)
);
staging.close().await?;
# Ok(())
# }
```

Production cleaners normally use `stream` or `recipe` helpers and submit
multiple batches with consecutive chunk offsets. Flush all staged bytes before
publishing a push bundle; otherwise a remote upload can reference data that is
not yet durable locally.

## Boundaries

- [`crab-xet`](../crab-xet/README.md) computes chunk hashes and builds Xorb
  payloads; staging owns local durability and lookup.
- [`crab-diff`](../crab-diff/README.md) compares staged file sequences; it
  does not own the staging database.
- [`crab-storage`](../crab-storage/README.md) publishes remote objects after
  staging has flushed and the push plan is ready.
