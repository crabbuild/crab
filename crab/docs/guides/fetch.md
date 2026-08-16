# crab fetch

Pre-fetch objects from the remote into the local cache.

## Synopsis

```
crab fetch [OPTIONS]
```

## Description

`crab fetch` downloads xorbs and shards from the remote store into the local
cache without hydrating any files. This warms the cache so that subsequent
`crab hydrate` or `git checkout` operations are fast, even on slow or
unreliable networks.

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

1. Reads the remote URL from `.crab/remote`.
2. Builds an S3 client for the configured bucket.
3. Lists objects under the repository prefix (shards first, then xorbs).
4. For each object:
   - Checks if it already exists in the local cache.
   - If not cached, downloads it and writes it to the cache directory.
5. Reports the total number of objects and bytes fetched.

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

```
fetch (dry run): would fetch objects from crab://my-bucket/my-repo
  prefix: my-repo
  include: []
  exclude: []
```

### Fetch everything except training data

```bash
crab fetch --exclude 'data/train/*'
```

## Output

```
fetch complete: 42 objects, 1288490188 bytes downloaded
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
- `--jsonl` streams per-xorb progress followed by a terminal `result` event.

### crab fetch --json

```json
{
  "schema": "fetch",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:30.200Z",
  "data": {
    "objects_fetched": 42,
    "bytes_fetched": 1288490188,
    "duration_ms": 8500
  }
}
```

### crab fetch --jsonl

```
{"schema":"fetch.event","version":"1.0","timestamp":"2026-04-24T18:32:22.100Z","type":"progress","data":{"operation":"fetching","current":10,"total":42,"bytes":314572800,"total_bytes":1288490188,"rate_bytes_per_sec":52000000.0}}
{"schema":"fetch.event","version":"1.0","timestamp":"2026-04-24T18:32:25.300Z","type":"xorb_done","data":{"hash":"a1b2c3d4e5f6","bytes":31457280,"compressed_bytes":28311552,"status":"ok"}}
{"schema":"fetch.event","version":"1.0","timestamp":"2026-04-24T18:32:30.200Z","type":"result","data":{"objects_fetched":42,"bytes_fetched":1288490188,"duration_ms":8500}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.
