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

`StagingArea::open` acquires an exclusive process lock, validates the canonical
schema, runs crash recovery, and opens the current segment.
`StagingAreaReadOnly` acquires a shared lock for concurrent push readers.
In-process index and writer locks are scoped so no synchronous mutex is held
across an async suspension.

`stage_chunks_batch` is the hot path: it deduplicates, appends, inserts
locators atomically, and performs the configured flush check. `flush_pending`
fsyncs the segment and records the durable boundary. Reads verify both the
record checksum and the requested BLAKE3 hash. Recipes, push plans, streaming
helpers, compaction, verification, and retirement build on these primitives.

### Payload ownership and compaction

Ownership retirement removes each unowned file batch in one SQLite transaction,
including chunk rows, segment live counts and unreferenced payload inventory.
Prepared bodies are unlinked only after that transaction commits. A database
failure rolls back the batch; an unlink failure leaves an orphan for recovery,
not a missing live payload. This shares one cleanup path across publication,
rollback and push, avoiding repeated per-file commits and inventory scans.

A direct-stream preparation retains each reused prepared Xorb in the same
transaction that selects its locator. The original recipe can then retire
without deleting bytes needed by the unsealed recording. Sealing transfers
retention to recipe leases; abort releases preparation retention and reclaims
only bodies with no other owner. Selection failures roll back these pins.

Segment payload inventory is also independent of file occurrence rows. An
in-flight recipe recording or sealed recipe retains reused segment bytes when
the original file retires. Reads and deduplication use that inventory; recipes
preserve every ordered occurrence separately. Batch rollback releases recording
retention, and cleanup removes inventory only after the last owner retires.

Compaction copies each retained payload once, then atomically moves inventory
and all committed/pending occurrence locators. Output reservation remains in
that transaction until the new body is durable. Readers capture file handles
with their locators, so unlink and reader-pool eviction cannot invalidate an
already captured segment read. Recovery reclaims unindexed compaction bodies
only after checking both segment registry and canonical payload ownership.

### Layout upgrades

Staging layout v3 upgrades the released v1 layout transactionally on writable
or shared open. It repairs inventory using the locators v1 readers used and
creates explicit external-publication ownership in the same transaction.

Old writers reject v3. Payload encoding stays unchanged. Missing dependencies or
failed schema installation leave v1 admission intact; deleted bytes cannot be
recovered by migration.

The unreleased v2 prototype is rejected and needs a
fresh staging root and restaging, rather than an implicit compatibility reader.

### Git handoff and push retirement

Native add owns its batches until its Git-index transaction succeeds or rolls
back. Git-clean cannot acknowledge Git's subsequent index replacement. It
instead delegates a complete verified lease after payload flush, before returning
a pointer. Native `--defer` delegates the same way. Delegation freezes the
completed batch and requires all of its paths to be handed off, with no active
preparation. It does not create a published path head.

Explicit staging maintenance holds Git's index lock, checks bounded pointer
blobs and publishes only batches whose every path matches the index. A missing
or different index entry retains its lease, including native `--defer` work and
older unpushed history. Publication consumes its delegation markers atomically.

Push discovers pointers in selected Git history, loads only
their published or delegated recipes, then captures exact eligible leases.

Retirement releases only captured owners after remote publication; newer handoffs
and active native transactions survive. Publishing a newer head never cancels
another caller's open lease. Failed Git conversion can leave a retained handoff;
reconciliation and abandoned-handoff reclamation still need qualification.

`StagingPushSnapshot::begin` fences retirement before recipe selection;
`capture` pins exact selected leases. Call `commit` only after confirmed remote
publication. It records a committed journal before retiring captured owners.
Drop clears the reader marker and discards an uncommitted snapshot when no
sibling reader prevents cleanup. Deferred journals remain for exclusive
maintenance; native and uncaptured leases remain owned throughout.

Mounted commits publish single-file batches with
`StagingAreaReadOnly::mark_batch_history_published`. Their distinct history
owners preserve earlier unpushed versions instead of replacing a worktree path
head. Publication is transactional, rejects active native/delegated handoffs,
and preserves the existing prohibition on rolling back published batches.
Push snapshots retire these owners after confirmed remote publication.

`StagedFileReader` captures a retained recipe and fences concurrent push
retirement for one read operation. It locates recipe pages by byte range and
uses the bounded coalesced reader, opening each selected prepared source once
per operation. Returned ranges verify chunk identities and coverage; complete
files additionally verify their BLAKE3 hash. Reads require local authority for
the selected terms. Its temporary pin journal is removed even while sibling
readers remain active; payload reclamation belongs to push or exclusive
maintenance. Shared-open filesystem/index admission runs off the Tokio worker.

`StagedFileReader::into_pin` transfers the captured recipe into a
`StagedFilePin` for an open unpublished file. It retains a metadata connection
and an individually locked ready marker, releasing ordinary shared staging
admission. Writers and confirmed push retirement can proceed while the file
remains open. Read snapshots retain recipe metadata without capturing path
leases or granting publication authority. Later reads reacquire shared staging
admission and verify bytes through the same recipe reader.

Maintenance preserves a ready pin while its OS lock is held, including across
PID namespaces. Dropping the pin releases its journal and marker; physical
payload reclamation stays with maintenance. Interrupted cleanup is recoverable
after the marker lock is released. Ownership IDs retain their opaque 64-hex
shape and use UUID entropy rather than process-local PID/clock counters.

### Bounded residual reads

The coalesced residual reader uses `CoalescedChunkReadPlan::read_next_bounded` after all
selected requests are appended. One prepared source is opened and fully verified
once, then retained as a format-bounded serialized body while decoded batches obey
the caller's count and byte limits. A single oversized chunk may exceed the byte
target to make progress; its format limit still applies. Returned chunks own their
allocations.

Source descriptors are pinned before worker admission, and cancelling
a pending read preserves its result for the next call.

The public `read_next`
method retains the complete-source behavior shipped in v1.2.4; it shares the same
source and chunk verification. Canonical callers should migrate to the bounded
method; removing the released method requires a breaking library release.

Native intent recovery may preserve caller-attested committed recipes under
canonical history owners before rolling back an old Git index state.
`preserve_publication_history` accepts only verified recipe roots belonging to
that unresolved intent. It leaves the original intent intact on cancellation or
error, so interrupted preservation can be retried before rollback.

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

`flush_pending` is the durability barrier before publication. `close` performs
that flush explicitly; dropping staging only releases handles. Recovery is not
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

Production cleaners normally use `stream` or `recipe` helpers and submit
multiple batches with consecutive chunk offsets. Flush all staged bytes before
publishing a push bundle; otherwise a remote upload can reference data that is
not yet durable locally.

`stream::stage_reader_streaming` accepts an `AsyncRead + Unpin` byte source
without seeking, reopening or reading source metadata. Native file staging uses
the same ingestion engine behind descriptor/path validation. Both paths compute
ordered recipes and use the existing segment/prepared-Xorb ownership machinery.

Successful ingestion flushes pending segment authority before recording the
version lease. The returned batch stays open until its caller publishes or
rolls back; the supplied logical path is not proof of a Git index update.

Cancel through the token while awaiting the operation so failed attempts clean
up. Production Git-clean adapts packet bytes to this engine without reopening or
seeking the source.

Exact prepared inputs reuse their recipe after byte verification;
changed inputs replay a pinned, bounded prefix and continue through the same engine.
Short reads coalesce into one bounded block before CDC.

The released custom `ChunkStager` injection API retains its byte-to-chunk adapter until a breaking
library release. Real 10-GiB crossover, termination and failure qualification remain
required before declaring the unification complete.

## Boundaries

- [`crab-xet`](../crab-xet/README.md) computes chunk hashes and builds Xorb
  payloads; staging owns local durability and lookup.
- [`crab-diff`](../crab-diff/README.md) compares staged file sequences; it
  does not own the staging database.
- [`crab-storage`](../crab-storage/README.md) publishes remote objects after
  staging has flushed and the push plan is ready.
