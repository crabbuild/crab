# crab fetch

Pre-fetch objects from the remote into the local cache.

## Synopsis

```
crab fetch [OPTIONS]
```

## Description

`crab fetch` resolves Crab pointer blobs from Git, reconstructs each selected
file into a discard sink, and retains the verified xet ranges and metadata in
the canonical local caches. It never materializes the selected files in the
working tree.

Think of it as "download now, use later" — you can fetch objects while on a fast
connection, then hydrate files later when offline or on a slower link.

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--include` | | Glob patterns to limit which files' objects are fetched |
| `--exclude` | | Glob patterns to exclude from fetching |
| `--all` | `false` | Fetch objects for all refs, not just HEAD |
| `--dry-run` | `false` | Report what would be fetched without downloading |

## How It Works

1. Reads the remote URL from `crab.toml`.
2. Resolves pointer blobs from the index/HEAD, or every local ref with `--all`.
3. Applies `--include` and `--exclude` to repository paths before reading blob
   bodies. Git blob sizes are batch-checked so large non-pointer blobs are not
   loaded into memory.
4. Selects the configured primary or replica through the normal read policy.
5. Reconstructs each unique file through the canonical file-index, shard, and
   xet-core range-cache path. Output is discarded after its hash and size are
   verified, so memory use is independent of logical file size.
6. Synchronizes the local chunk index unless `--no-sync-chunk-index` is set.

Objects are stored in the local cache at `~/.cache/crab/` (or the path
specified by `$CRAB_CACHE_DIR`).

## Examples

### Fetch all objects for HEAD

```bash
crab fetch
```

### Fetch objects for specific file types

```bash
crab fetch --include '*.safetensors'
```

### Fetch objects for all branches

```bash
crab fetch --all
```

### Dry run to see what would be fetched

```bash
crab fetch --dry-run
```

Dry-run resolves the exact local Git selection and reports its file count and
logical bytes. It does not resolve credentials, contact a replica, or mutate a
cache.

### Fetch everything except training data

```bash
crab fetch --exclude 'data/train/*'
```

## Output

```
fetch complete: 42 file(s), 1288490188 logical bytes verified
```

## Use Cases

- Pre-warm the cache before a flight or going offline.
- Speed up `crab hydrate` by downloading objects in advance.
- In CI, fetch objects before running tests that need hydrated files.
- On shared machines, populate the cache once for all users.

## Cache Location

The local cache defaults to `~/.cache/crab/`. Override with:

```bash
export CRAB_CACHE_DIR=/fast-ssd/crab-cache
crab fetch
```

## Prerequisites

- The repository must be initialized with `crab init`.
- AWS credentials must be configured for the remote bucket.

## Related Commands

- [`crab hydrate`](crab-hydrate.md) — materialize pointer files using cached objects.
- [`crab prune`](crab-prune.md) — remove unreferenced objects from the local cache.
- [`crab cache stats`](crab-cache.md) — inspect cache statistics.

## JSON Output

Supports `--json` and `--jsonl`.

- `--json` runs to completion and emits a single result envelope.
- `--jsonl` emits phase progress followed by a terminal `result` event.

### crab fetch --json

```json
{
  "schema": "fetch",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:30.200Z",
  "data": {
    "objects_fetched": 42,
    "bytes_downloaded": 1288490188,
    "objects_skipped": 0,
    "duration_ms": 8500
  }
}
```

### crab fetch --jsonl

```
{"schema":"perf.phase","version":"1.0","timestamp":"2026-04-24T18:32:30.100Z","type":"event","data":{"command":"fetch","phase":"hydration_prefetch","duration_ms":8400,"bytes":1288490188,"items":42}}
{"schema":"fetch.event","version":"1.0","timestamp":"2026-04-24T18:32:30.200Z","type":"result","data":{"objects_fetched":42,"bytes_downloaded":1288490188,"objects_skipped":0,"duration_ms":8500}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.
