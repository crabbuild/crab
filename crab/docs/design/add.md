# Crab Add Operation

`crab add` stages Crab-tracked working-tree files without sending the large
file bytes through Git's clean-filter protocol. It streams each file into the
local staging area, builds add-time push plans from the staged chunk rows, then
writes small Crab pointer blobs into Git's object database and index.

## Current Architecture

```mermaid
flowchart LR
    user[User: crab add patterns]
    cli[cmd/add.rs run_add]
    attrs[TrackedClassifier and PatternFilter]
    walker[collect_candidates]
    stream[cmd/stream_stage.rs stage_file_streaming]
    staging[(StagingArea segments + SQLite)]
    plan[crab-staging canonical authority]
    remote[(optional crab:// remote chunk index)]
    gitblob[gix write_blob]
    gitindex[git update-index -z --index-info]
    index[(Git index)]

    user --> cli
    cli --> attrs --> walker
    walker --> stream
    stream --> staging
    staging --> plan
    remote -. candidate lookup .-> plan
    plan --> staging
    staging --> gitblob
    gitblob --> gitindex --> index
```

The important ownership boundaries are:

| Surface | Responsibility |
| --- | --- |
| `crab/src/cmd/add.rs` | CLI orchestration, candidate discovery, progress, rollback, pointer publication |
| `crates/crab-staging/src/stream.rs` | Bounded file streaming, Blake3 hashing, CDC chunking, preparation-wide claims, provisional staging adoption |
| `crates/crab-staging/src/lib.rs` | Segment writes, SQLite rows, flush/promotion, staged-file adoption and retirement |
| `crates/crab-staging/src/add_push_plan.rs` | Add-time push-plan construction from staged chunk rows |
| `crates/crab-staging/src/push_plan.rs` | Add-time plan model, prepared-xorb payload helpers, and diagnostics |
| `crab/src/git/push.rs` | Push-time adoption of add-time plans |

## Operation Flow

```mermaid
sequenceDiagram
    participant CLI as crab add
    participant FS as Working tree
    participant ST as StagingArea
    participant PP as Push-plan builder
    participant REM as Remote chunk index
    participant GIT as Git ODB + index

    CLI->>FS: discover Crab-tracked candidates
    loop bounded by --jobs
        CLI->>FS: stream file in 1 MiB buffers
        CLI->>ST: write CDC chunks under provisional file key
        CLI->>ST: adopt provisional key under final Blake3 file hash
    end
    CLI->>PP: prepare file push plans
    PP-->>REM: optional chunk lookup when origin is crab://
    PP->>ST: validate chunk sequence and pack missing chunks
    CLI->>ST: close and flush pending segment rows
    CLI->>GIT: write pointer blobs
    CLI->>GIT: publish all index entries in one nul-delimited batch
```

## Candidate Discovery

`run_add` resolves the current worktree through
`crab/src/git/worktree.rs`, opens a `TrackedClassifier`, builds the user's
path filter, then walks the worktree.

The classifier has two modes:

- With `gix-pathmatch`, it delegates path matching to
  `crab/src/core/attrs.rs`, which reads root and nested `.gitattributes`
  using `gix_attributes`.
- Without `gix-pathmatch`, it uses the legacy root `.gitattributes`
  suffix matcher in `crab/src/cmd/add.rs`.

The empty-classifier branch exists so a repository with no Crab filter patterns
can auto-track large files before scanning again. In `gix-pathmatch` builds,
`TrackedClassifier::is_empty` separately scans root and nested `.gitattributes`
for any `filter=crab` assignment because the path matcher itself answers
per-path questions, not "are there any configured Crab patterns?"

The walker skips Git/Crab internals, non-files, paths outside the user filter,
paths not marked `filter=crab`, and files that already contain a valid Crab
pointer.

## Streaming and Staging Algorithm

`crab/src/cmd/stream_stage.rs` is the bounded-memory file path shared by
`crab add` and `crab adopt`.

```mermaid
flowchart TD
    start[open file]
    key[derive provisional staging key from relative path]
    retire[retire stale provisional rows]
    stat1[capture before stat]
    read[read 1 MiB buffer]
    hash[update Blake3 file hasher]
    cdc[feed GearChunker]
    batch[stage chunks in batches of up to 1024 or 64 MiB]
    eof{EOF?}
    stat2[capture after stat]
    changed{stat changed?}
    adopt[adopt_staged_file provisional -> final hash]
    record[record file path]
    done[return file hash, size, chunk sequence, index stat]
    cleanup[retire provisional rows and error]

    start --> key --> retire --> stat1 --> read
    read --> hash --> cdc --> batch --> eof
    eof -- no --> read
    eof -- yes --> stat2 --> changed
    changed -- yes --> cleanup
    changed -- no --> adopt --> record --> done
```

The chunker and file hasher are fed from the same read loop. CDC chunks are
flushed to staging in bounded batches, and progress counters are updated from
the same stream. The staging schema keys rows by file hash, so the stream first
writes under a provisional key and then calls `StagingArea::adopt_staged_file`
once the final Blake3 file hash is known.
Because the provisional key is retired before streaming begins, the stream uses
the retired-file staging path to avoid a redundant pending-position probe for
each flush batch while keeping the general `stage_chunks_batch` stale-position
guard for shared callers.

Rollback rules:

- If streaming or staging fails, provisional rows are retired.
- If the file's verified stat changes during staging, provisional rows are
  retired and the command returns `FileChangedDuringStaging`.
- If adoption fails, provisional rows are retired.
- If path recording fails after adoption, final rows are retired.
- If any file in the batch fails before Git index publication, all unpublished
  staged entries from the batch are retired.

The provisional key is internal staging state. It is not written to pointer
blobs or committed to Git.

## Canonical Add-Time Authority

One add preparation spans every direct-stream file in the command. For each
bounded chunk batch, staging selects exactly one authority in this order:

1. a proof-bearing committed remote placement;
2. an existing sealed local prepared placement;
3. a verified staging-segment payload; or
4. a preparation-wide claim for bytes that must be compressed once.

SQLite owns the claim and `prepared_payload_chunks.chunk_hash` is unique, so
parallel files and later adds cannot assign the same chunk to different local
xorbs. Only the claim winner feeds its builder. Other recipe occurrences wait
for and then lease the winner's placement. The coordination is disk-backed and
batch-bounded; it does not require a repository-sized in-memory hash map.

```mermaid
flowchart LR
    chunks[verified recipe occurrences]
    remote{committed remote proof?}
    prepared{sealed prepared placement?}
    segment{verified segment payload?}
    claim[atomic preparation claim]
    pack[claim winner packs once]
    normalize[(normalized v1 authority rows)]

    chunks --> remote
    remote -- yes --> normalize
    remote -- no --> prepared
    prepared -- yes --> normalize
    prepared -- no --> segment
    segment -- yes --> normalize
    segment -- no --> claim --> pack --> normalize
```

Remote lookup is opportunistic. Failure to open or query the remote never
publishes a guess: add continues using local authority. The bucket-global
SlateDB contains only committed, origin-bound receipts and is updated after a
successful push CAS; it never stores add claims, local paths, or pending
uploads.

Prepared bodies use one local content-addressed path:

```text
.crab/staging/push-plans/payloads/<first-two>/<xorb-hash>.xorb
```

Sealing validates the xorb identity and full payload digest, fsyncs a unique
same-directory temporary file, and installs the final name create-once.
Equivalent concurrent writers coalesce; a different body at the same name is
corruption and is never overwritten. Writable-open recovery aborts unfinished
preparations and removes unindexed final bodies and abandoned stream temps.

Before staging, same-size candidates with matching bounded
head/middle/tail fingerprints are grouped as possible duplicates. A later path
is fully Blake3-hashed, and only if its final hash and size match the
representative does `crab add` reuse the representative's staged chunk layout.
Duplicate candidates are queued behind only their representative, so they can
start as soon as the representative finishes instead of waiting for unrelated
files. Fingerprints are scheduling hints, not identity proof.

Direct-prepared chunks intentionally do not also require a segment copy. Push
may repack a missing or corrupt prepared body only when every affected recipe
occurrence has independent verified segment authority. Otherwise it fails
closed and asks the user to run `crab add` again. Recipe leases and push
snapshots retain shared bodies until the final owner is released; restaging
reclaims only globally unleased payloads.

There is no persisted per-file JSON plan or per-file payload copy. Runtime
`FilePushPlan` values are derived from normalized recipe, remote, prepared, and
segment rows for the push attempt.

## Progress and JSONL

The text progress bar has four command phases:

| Phase | What moves the counter |
| --- | --- |
| `Streaming` | per-file read and CDC counters |
| `Planning` | files whose add-time push plan is prepared, plus chunks, cached xorbs, prepared xorbs, prepared bytes, and remote lookup state |
| `Flushing` | staging segment fsync and pending-row promotion |
| `Indexing` | pointer blobs staged into Git's index |

JSONL mode emits:

- `file_done` after each staged file.
- `progress` with `operation: "staging"` during file streaming.
- `progress` with `operation: "push-plan"` while preparing add-time plans.
- final `result` with `AddSummary`.

The JSONL `push-plan` event reports completed files as `current`, total files as
`total`, prepared xorb bytes as `bytes`, and prepared xorb count as
`xorbs_produced`. Because progress events are rate-limited, the terminal
`AddSummary` also carries the wall-clock phase durations plus cumulative worker
time for CDC, bucket-global lookup, compression, and prepared-payload writes.
Worker durations can exceed wall time because files execute concurrently.

Independent `crab add` processes queue for up to 30 minutes on the staging
flock. A later process does not fail merely because an earlier large add is
still preparing or publishing, and rechecks Git's index after ownership so it
does not repeat work the preceding process just published.
For `--skip-git-add`, ordinary `git add` hashes the Git-provided stream into an
anonymous staging-root spool. An exact prepared hash promotes the retained
recipe without CDC or chunk-index writes; changed bytes replay from that spool
through the canonical clean pipeline.

## Git Pointer Publication

`write_pointers_to_git_index` publishes only after staging and plan preparation
succeed. When add generated tracking rules, the publisher applies those exact
rules to the currently indexed `.gitattributes` blob and commits the metadata
and pointer entries through the same index lock. Unrelated unstaged attribute
edits remain in the worktree rather than being swept into the commit.

```mermaid
flowchart TD
    entry[StagedEntry]
    pointer[build Crab pointer with optional shard hint]
    blob[gix Repository::write_blob]
    collect[collect mode, oid, path]
    batch[git update-index -z --index-info]
    stat[populate Git stat cache]

    entry --> pointer --> blob --> collect --> batch --> stat
```

Pointer blob creation is native: Crab opens the repository with `gix`, writes
the pointer payload through `Repository::write_blob`, and uses the repository's
configured object hash. The index update remains one nul-delimited
`update-index --index-info` subprocess for the whole batch.

The stat-cache population step matters because entries inserted through
`update-index --index-info` do not automatically receive the normal worktree
stat fields. Crab updates those fields after publication so a later
`git status` can avoid rehashing large files through the clean filter.

## Algorithms

### Single-pass stream stage

1. Compute a provisional staging key from the repo-relative path.
2. Retire stale rows under that provisional key.
3. Register the provisional file row.
4. Stream the file in `READ_BUF_SIZE` buffers.
5. For each buffer, update the Blake3 hasher and feed `GearChunker`.
6. Stage emitted chunks in batches capped by `STAGE_BATCH_CHUNKS` or
   `STAGE_BATCH_TARGET_BYTES`, whichever comes first.
7. On EOF, finalize the chunker and file hash.
8. Compare before/after verified stat.
9. Adopt provisional staged rows under the final file hash.
10. Record the path and return the file hash, size, chunk count, chunk sequence,
    and optional stat snapshot.

### Push-plan preparation

1. Build the wanted chunk set from streamed `chunk_pairs`.
2. Load prepared xorb cache entries and local xorb cache candidates for those
   chunks.
3. Optionally open a Crab remote metadata guard.
4. For multi-file adds, query the remote chunk index once for the whole batch.
5. For each file, validate staged chunk rows against the streamed sequence.
6. Mark size-matching remote refs as existing chunks.
7. Prefer prepared/local cached xorbs that cover file chunks still needed.
8. Read only remaining new chunks from staging in bounded batches.
9. Pack those chunks with `XorbBuilder`.
10. Write the file push plan and prepared xorb records.

### Pointer index publication

1. Build a pointer from file hash, size, and shard-hint cache.
2. Write the pointer payload as a Git blob.
3. Build a nul-delimited index-info entry with mode, blob id, and path bytes.
4. Publish all index-info entries in one Git subprocess.
5. Refresh stat-cache fields for the published paths.

## Invariants

- Git index publication happens only after the chunks referenced by every
  pointer are staged and flushable.
- Rollback retires unpublished staging rows on file errors, cancellation before
  publication, and push-plan preparation errors.
- Push plans are validated against the exact staged chunk sequence before a
  plan is promoted.
- Add-time remote lookup is an optimization, not a correctness dependency.
- The clean-filter path remains independently correct for normal `git add`.

## Authoritative Xorb Staging

Prepared xorbs can become the authoritative staging store, but only after they
replace the complete segment contract rather than only the push-plan packing
path.

| Current segment contract | Xorb-backed replacement needed |
| --- | --- |
| `chunks` and `pending_chunks` rows locate bytes by `segment_id`, `segment_offset`, and size | Chunk rows locate bytes by durable xorb id plus placement metadata |
| `get_chunk`, `get_chunks_batch`, and `get_located_chunks_batch` read framed bytes from segment files | The same readers can fetch, verify, and decompress chunk payloads from xorb files |
| `chunks_for_file_with_sizes` and `chunks_for_file_with_locators` prove ordered file coverage before push | Ordered file coverage is verified from xorb-backed rows with no segment fallback |
| Recovery truncates the current segment to a durable SQLite byte boundary and deletes torn pending rows | Recovery removes torn temp xorbs, validates committed xorb manifests, and drops rows for missing or corrupt payloads |
| `retire_file`, `sweep_orphans`, and push-inflight markers keep segment bytes alive until all readers finish | The same lifecycle protects xorb files while local pushes may still read them |
| Push rejects stale or corrupt prepared plans by falling back to segment reads | If xorbs are authoritative, stale/corrupt xorb rows are staging corruption unless another authoritative row covers the chunk |

That migration is worth doing when benchmarks show push-enabled adds are still
dominated by segment write plus add-time xorb packing. It should land as a new
staging layout with a reader abstraction and crash-recovery tests before any
segment write is removed from `crab add`.

## Improvement Ideas

1. **Stronger concurrent mutation exclusion.** Clean indexed files are hashed
   before Crab reuses their pointer, and streamed files compare descriptor and
   path stat before publication. A malicious writer that changes bytes after a
   verified read while preserving every observed stat field still requires
   cooperative file locking or filesystem snapshots to exclude completely.
2. **Plan progress inside large single files.** The planning phase reports per
   completed file. If one file packs many chunks, an additional per-batch
   callback from `prepare_one_file_plan` would make `push-plan` progress more
   granular.
3. **Remote lookup policy surface.** The current policy is opportunistic and
   quiet except for progress/logging. If users need strictly local `crab add`,
   add an explicit product-level policy rather than another hidden fallback.
4. **Prepared-plan compaction.** Prepared xorb cache reuse can grow local
   staging metadata. A future maintenance pass could retire old prepared plans
   once a push has verified their chunks in the remote index.
5. **Authoritative xorb staging.** Add-time prepared xorbs are currently a
   verified push-plan payload cache, not the staging source of truth. Replacing
   segment bytes needs a transactional xorb manifest that serves
   `chunks_for_file_with_sizes`, chunk reads that resolve
   `(xorb_hash, chunk_index)` placements, crash recovery for partially written
   xorbs and manifests, and retire/clean accounting for xorb files. Until those
   surfaces exist, segment rows remain the authoritative staging store.
6. **Eliminate double local writes.** For push-enabled adds, the current
   architecture can write segment bytes and stream-built prepared xorb bytes for
   the same chunks. The next structural speedup is a xorb-backed staging mode
   that can serve every staging reader directly from prepared-xorb manifests,
   removing segment writes for add-owned data instead of only removing planning
   rereads and post-stream packing.

## Source Map

| Topic | Source |
| --- | --- |
| CLI orchestration and progress | `crab/src/cmd/add.rs` |
| Stream staging | `crab/src/cmd/stream_stage.rs` |
| Add-time push plans | `crates/crab-staging/src/add_push_plan.rs` |
| Staging storage | `crates/crab-staging/src/lib.rs` |
| Push-plan storage format | `crates/crab-staging/src/push_plan.rs` |
| Pointer format | `crab/src/git/pointer.rs` |
| Push-time plan adoption | `crab/src/git/push.rs` |
