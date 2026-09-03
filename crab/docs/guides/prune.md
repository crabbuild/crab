# crab prune

Evict eligible local cache payloads toward the configured byte budget.

## Synopsis

```
crab prune [OPTIONS]
```

## Description

`crab prune` trims cached chunks, xorbs, shards, and decoded ranges. It does not
walk worktree pointers, determine remote reachability, or contact the remote.
Eviction can remove warm data used by the current checkout; a later read may
need to download it again. This command does not replace `crab gc`.

`crab optimize cache prune` runs the same implementation. Configure the cache
budget through `[cache].max_bytes`; the default is 10 GiB. Object and range
maintenance still have separate passes, and busy entries are retained. This
is not yet a qualified hard cap on all bytes below the cache root; full
accounting and unified lifecycle acceptance remain in Plan 017.

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--dry-run` | `false` | Report what would be pruned without deleting |
| `--verbose` | `false` | Print each object as it is pruned |
| `--json` | `false` | Emit one structured JSON result |
| `--jsonl` | `false` | Emit per-object events and a terminal result |

## How It Works

1. Rejects an unsafe broad root before either payload pass.
2. Scans recognized private payload layouts, retaining unknown entries.
3. Orders eligible entries by recorded modification time, oldest first.
4. Takes the parent/payload locks and removes entries toward each pass's budget.
   Active readers and publishers are skipped. Dry-run uses the same eligibility
   checks and locks without deleting.

This is not a complete access-time LRU implementation. SQLite/index ownership,
reservations across every maintenance path, and complete accounting remain
hardening work. The command has no `--verify-remote` option.

## Examples

### Dry run to see what would be pruned

```bash
crab prune --dry-run
```

### Prune with verbose output

```bash
crab prune --verbose
```

### Standard prune

```bash
crab prune
```

## Safety

- Prune only removes local cache entries — the remote store is never modified.
- Filesystem roots, the home directory, the current directory, and its ancestors
  are rejected, including relative spellings. Missing roots are not created.
- Unknown files, live subtrees, unpublished temporaries, and database files
  are not payload-deletion authority. Private-I/O checks reject unsafe paths.
- Published data can be fetched again only while its remote and authorization
  remain available. Do not treat a disposable cache as the sole durable copy.
- `--dry-run` lets you preview what would be removed before committing.

## When to Run

- After reducing the configured cache budget.
- When disk space is low and you want to reclaim cache space.
- Periodically as maintenance.

## Cache Location

The local cache defaults to `~/.cache/crab/`. Override with:

```bash
export CRAB_CACHE_DIR=/path/to/cache
```

## Related Commands

- [`crab gc`](crab-gc.md) — garbage collect unreachable objects from the remote.
- [`crab fetch`](crab-fetch.md) — pre-fetch objects into the local cache.
- [`crab cache clean`](cache.md) — remove eligible local payloads while preserving retained state.
- [`crab du`](crab-du.md) — see disk usage breakdown.

## JSON Output

Supports `--json` and `--jsonl`.

- `--json` runs to completion and emits a single result envelope.
- `--jsonl` streams per-object progress followed by a terminal `result` event.

### crab prune --json

```json
{
  "schema": "prune",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:19.800Z",
  "data": {
    "objects_pruned": 15,
    "chunks_pruned": 10,
    "shards_pruned": 3,
    "xorbs_pruned": 2,
    "bytes_freed": 234567890,
    "dry_run": false
  }
}
```

### crab prune --jsonl

```
{"schema":"prune.event","version":"1.0","timestamp":"2026-04-24T18:32:19.800Z","type":"result","data":{"objects_pruned":15,"chunks_pruned":10,"shards_pruned":3,"xorbs_pruned":2,"bytes_freed":234567890,"dry_run":false}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.
