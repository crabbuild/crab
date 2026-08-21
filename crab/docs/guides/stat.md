# crab stat

Print staging area statistics or performance counters.

## Synopsis

```
crab stat
crab stat perf
crab stat push-plan
```

## Description

`crab stat` provides staging area summaries, add-time push-plan inventory, and
performance counter reporting (`perf`).

## Subcommands

### crab stat (default)

Print staging area statistics. This is similar to `crab staging stats` but
operates on the default staging root (`.crab/staging`).

```bash
crab stat
```

```
Staging area: .crab/staging
  Sealed segments:       3
  Current segment bytes: 1234567
  Total staged bytes:    45678901
  Live bytes:            40000000
  Dead bytes:            5678901
  Dead ratio:            12.43%
  Chunk count:           847
  File count:            12
```

### crab stat perf

Print persisted performance counters from `.crab/perf-state.json`.

```bash
crab stat perf
```

Performance counters track cumulative metrics across all crab operations:

| Counter | Description |
|---------|-------------|
| `push_duration_ms` | Total time spent pushing |
| `bytes_uploaded` | Total xorb payload bytes uploaded; excludes Git packs, indexes, manifests, refs, locks, and metadb objects |
| `fetch_duration_ms` | Total time spent fetching |
| `bytes_downloaded` | Total bytes downloaded from remote |
| `gc_duration_ms` | Total time spent in garbage collection |
| `gc_objects_deleted` | Total objects deleted by GC |
| `chunk_index_lookups` | Number of chunk index lookups |
| `chunk_index_hits` | Number of chunk index cache hits |
| `shard_bloom_queries` | Number of shard bloom filter queries |
| `shard_bloom_false_positives` | Number of bloom filter false positives |
| `staging_bytes_written` | Total bytes written to staging |
| `staging_bytes_read` | Total bytes read from staging |
| `xorbs_skipped` | Number of xorbs skipped (already uploaded) |
| `clean_fastpath_taken` | Number of clean filter fast-path hits |
| `xorb_fetch_requests_coalesced` | Number of coalesced xorb fetch requests |
| `xorb_fetch_bytes_saved` | Bytes saved by request coalescing |
| `multipart_resumed_uploads` | Number of resumed multipart uploads |
| `head_list_requests` | LIST requests issued by batched xorb resume checks |
| `head_point_requests` | Individual HEAD requests issued by xorb resume checks |
| `metadb_buffered_batch_write_count` | Buffered metadb batches submitted during pushes |
| `metadb_wal_flush_count` | Explicit metadb WAL flushes |
| `metadb_memtable_flush_count` | Explicit metadb memory-table-to-L0 flushes |

Pushes update the file cumulatively under an advisory file lock. If the
perf-state file doesn't exist or is corrupt, zeroed counters are shown.

These counters cover selected Crab operations. They do not count every object
store `GET`, `HEAD`, `PUT`, `LIST`, retry, pack byte, or metadata byte, so do
not use them as a billing ledger. Correlate them with provider or RustFS server
metrics for request and transfer cost.

### crab stat push-plan

Print add-time push-plan inventory for `.crab/staging`. This reads the indexed
staging store; JSON sidecar files are not staging authority and are not used as
fallback plan data.

```bash
crab stat push-plan
crab stat push-plan --verify
```

`--verify` additionally hashes and parses referenced prepared-xorb payload files
to detect payload hash, corruption, and metadata mismatches.

The indexed-row counters report prepared-xorb candidate rows in SQLite. Orphaned
rows are not referenced by an authoritative plan body; invalid rows have
malformed key blobs or metadata that disagrees with the authoritative plan body.

## Examples

### View staging stats

```bash
crab stat
```

### View performance counters

```bash
crab stat perf
```

### Inspect add-time push plans

```bash
crab stat push-plan --verify
```

### Use perf counters for monitoring

```bash
crab stat perf | grep bytes_uploaded
```

## Related Commands

- [`crab staging stats`](crab-staging.md) — detailed staging area statistics.
- [`crab du`](crab-du.md) — disk usage breakdown.
- [`crab env`](crab-env.md) — diagnostic environment information.

## JSON Output

`crab stat`, `crab stat perf`, and `crab stat push-plan` support `--json`.

### crab stat --json

```json
{
  "schema": "stat",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "sealed_segments": 3,
    "current_segment_bytes": 1234567,
    "total_staged_bytes": 45678901,
    "live_bytes": 40000000,
    "dead_bytes": 5678901,
    "dead_ratio": 0.1243,
    "chunk_count": 847,
    "file_count": 12
  }
}
```

### crab stat perf --json

```json
{
  "schema": "stat.perf",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "push_duration_ms": 45230,
    "bytes_uploaded": 1288490188,
    "fetch_duration_ms": 12400,
    "bytes_downloaded": 524288000,
    "gc_duration_ms": 3200,
    "gc_objects_deleted": 42,
    "chunk_index_lookups": 8400,
    "chunk_index_hits": 7200
  }
}
```

### crab stat push-plan --json

```json
{
  "schema": "stat.push-plan",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "format_version": 3,
    "verified_prepared_xorbs": false,
    "plan_files": 2,
    "invalid_plan_files": 0,
    "planned_file_bytes": 104857600,
    "planned_chunks": 128,
    "existing_chunks": 12,
    "prepared_xorbs": 4,
    "prepared_chunks": 116,
    "prepared_bytes": 94371840,
    "indexed_prepared_xorbs": 4,
    "orphaned_indexed_prepared_xorbs": 0,
    "invalid_indexed_prepared_xorbs": 0,
    "referenced_prepared_xorb_files": 4,
    "referenced_prepared_xorb_file_bytes": 94371840,
    "missing_prepared_xorb_files": 0,
    "mismatched_prepared_xorb_files": 0,
    "stale_prepared_xorb_files": 0,
    "stale_prepared_xorb_file_bytes": 0,
    "verified_prepared_xorb_files": 0,
    "verified_prepared_xorb_file_bytes": 0,
    "payload_hash_mismatched_prepared_xorb_files": 0,
    "corrupt_prepared_xorb_files": 0,
    "metadata_mismatched_prepared_xorb_files": 0
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
