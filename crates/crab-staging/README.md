# crab-staging

`crab-staging` is Crab's durable local write buffer for chunks produced while
cleaning files and preparing a push. It retains prepared xorbs or append-only segment
payloads and records their locations in SQLite so deduplication, recovery, and
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
native descriptor reads / Git content packets
        │ bounded byte blocks
        ▼
IngestSession   ── hash, CDC or prepared verification, authority claims
index.db        ── recording terms, preparation ownership, recipe/path leases
push-plans/     ── durable prepared xorb bodies for missing chunks
segments/       ── existing segment consumers and reusable segment authority
        │ source validation, payload flush, recipe seal, index checkpoint
        ▼
caller publication / push upload
```

`StagingArea::open` acquires an exclusive process lock, validates the canonical
schema, runs crash recovery, and opens the current segment.
`StagingAreaReadOnly` acquires a shared lock for concurrent push readers.
In-process index and writer locks are scoped so no synchronous mutex is held
across an async suspension. Writer and shared-reader connections use a 64 MiB
page-cache budget to avoid random hash-index I/O with SQLite's smaller default.
This is a per-connection budget, not memory proportional to file size.

`ingest::IngestSession` is the shared mutation path for native add and Git
clean. `stream` supplies descriptor-safe file reads and validation. Git owns
packet framing, LFS/pointer dispatch, history preservation and pointer emission.
The session batches chunk work, retains existing authority, prepares only
missing payloads, and records every ordered occurrence. Both input adapters pin
a locally retained recipe before reading. Exact input verifies every chunk and
the whole-file hash without CDC, payload writes or rebuilding recipe occurrences.
Native still validates descriptor and pathname identity before sealing.
Changed input retains verified prefix terms and
replays only the last verified chunk to preserve EOF-tail boundary behavior.

`finish_input` completes input and workers. After source validation, `seal`
flushes payloads and installs a complete unpublished recipe lease. Callers own
publication. Clean retains published ownership before replying because Git
provides no later index-commit acknowledgment. Explicit abort and writable
reopen recovery retire unfinished operations.

`stage_chunks_batch` remains available to import, LFS, adopt and VFS segment
consumers. Reads verify record checksums and BLAKE3. Compaction moves canonical
payload inventory, including chunks retained solely by recipes or recordings.

Ownership retirement removes each unowned file batch in one SQLite transaction,
including chunk rows, segment live counts and unreferenced payload inventory.
Prepared bodies are unlinked only after the transaction commits and its WAL
checkpoint completes. The index lock remains held through unlink. A database
failure rolls back the batch; an unlink failure leaves an orphan for recovery,
not a missing live payload. This shares one cleanup path across publication,
rollback and push, avoiding repeated per-file commits and inventory scans.

## Recovery contract

Opening writable staging runs recovery before creating or reusing its writer.
Recovery treats missing files differently from failed filesystem operations:

| Observed state | Recovery action |
| --- | --- |
| Sealed segment missing or shorter than its indexed size | Reject as staging corruption. |
| Current segment missing, with promoted chunks | Reject as staging corruption. |
| Current segment missing, with only pending rows | Discard those rows and reset its durable boundary. |
| Current segment has a torn tail | Keep complete recoverable records, truncate the tail, and update SQLite's boundary. |
| Current segment metadata lookup fails for another reason | Return the I/O cause before changing that segment's rows or boundary. |
| Orphan `current.seg.tmp` entry | Remove the entry; an already absent entry is harmless. |

`flush_pending` syncs segment bytes; `flush_index` completes a FULL WAL
checkpoint for durable ownership. `seal` and `close` perform the required
barriers explicitly; dropping staging only releases handles. Recovery is not
a substitute for flushing a successful operation.

## Multipart cleanup

The multipart journal records provider sessions separately from chunk staging.
`find_abandoned` selects only expired leases whose last update predates the
grace cutoff. Invalid scan times return an error rather than a candidate list.

Cleanup must acquire the observed row revision with `claim_abandoned` before
contacting the provider. A scan result alone does not grant ownership; a
concurrent resume can make that result stale.

## Usage

The smallest complete staging cycle is:

```rust
use crab_staging::StagingArea;
use crab_xet::hash::compute_data_hash;
use std::path::PathBuf;

async fn example() -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
}
```

Production cleaners use `ingest`; file-based callers use the `stream` adapter. Flush all staged bytes before
publishing a push bundle; otherwise a remote upload can reference data that is
not yet durable locally.

## Boundaries

- [`crab-xet`](../crab-xet/README.md) computes chunk hashes and builds Xorb
  payloads; staging owns local durability and lookup.
- [`crab-diff`](../crab-diff/README.md) compares staged file sequences; it
  does not own the staging database.
- [`crab-storage`](../crab-storage/README.md) publishes remote objects after
  staging has flushed and the push plan is ready.

## Local layout version

The canonical local layout is version 2. Version 1 shipped in release v1.2.4.
Run `crab staging upgrade`, then `crab staging verify`, to preserve staged-only
content and ownership while adding the two ownership indexes and advancing
the writer-admission marker. Upgrade requires exclusive admission and rejects
unrecognized schema shapes. Ordinary opens reject old layouts; no fallback
reader or automatic deletion is provided. Old binaries reject upgraded data.
Pointer and remote xorb formats are unchanged.
