# crab prune

Remove unreferenced objects from the local cache.

## Synopsis

```
crab prune [OPTIONS]
```

## Description

`crab prune` scans the local cache directory and removes objects that are no
longer referenced by any pointer file in the current working tree. This frees
disk space without affecting the remote store.

Unlike `crab gc` (which operates on the remote), `prune` is purely local and
safe to run at any time. The remote store always retains the authoritative copy
of all objects.

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--dry-run` | `false` | Report what would be pruned without deleting |
| `--verbose` | `false` | Print each object as it is pruned |
| `--verify-remote` | `false` | Verify objects exist on the remote before pruning locally |

## How It Works

1. Walks the working tree and collects all content hashes from pointer files.
2. Walks the local cache directory (`~/.cache/crab/` or `$CRAB_CACHE_DIR`).
3. For each cached object, checks if its hash is in the referenced set.
4. Removes objects that are not referenced by any current pointer file.

### Verify Remote Mode

With `--verify-remote`, before deleting a local cache entry, crab confirms
that the object exists on the remote store. This provides an extra safety net:
if an object is only in the local cache (not yet pushed), it won't be pruned.

## Examples

### Dry run to see what would be pruned

```bash
crab prune --dry-run
```

```
prune (dry run): would remove 15 objects (234567890 bytes)
```

### Prune with verbose output

```bash
crab prune --verbose
```

```
pruning: /home/user/.cache/crab/repo/xorbs/abc123 (12345678 bytes)
pruning: /home/user/.cache/crab/repo/xorbs/def456 (23456789 bytes)
...
prune complete: removed 15 objects (234567890 bytes freed)
```

### Prune with remote verification

```bash
crab prune --verify-remote
```

### Standard prune

```bash
crab prune
```

```
prune complete: removed 15 objects (234567890 bytes freed)
```

## Safety

- Prune only removes local cache entries — the remote store is never modified.
- Objects can always be re-fetched from the remote with `crab fetch`.
- Use `--verify-remote` for extra safety when you're unsure if all objects have
  been pushed.
- `--dry-run` lets you preview what would be removed before committing.

## When to Run

- After switching branches to clean up objects from the previous branch.
- After `crab dehydrate` to remove cached chunks for dehydrated files.
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
- [`crab cache clean`](crab-cache.md) — clear the entire local cache.
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
    "bytes_freed": 234567890,
    "duration_ms": 1200
  }
}
```

### crab prune --jsonl

```
{"schema":"prune.event","version":"1.0","timestamp":"2026-04-24T18:32:18.600Z","type":"file_done","data":{"path":"xorbs/abc123","bytes":12345678,"duration_ms":50,"status":"ok"}}
{"schema":"prune.event","version":"1.0","timestamp":"2026-04-24T18:32:19.800Z","type":"result","data":{"objects_pruned":15,"bytes_freed":234567890,"duration_ms":1200}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.
