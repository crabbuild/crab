# crab repack

Consolidate remote Git pack files in the remote store.

## Synopsis

```
crab repack [OPTIONS]
```

## Description

`crab repack` applies Git's geometric pack policy to the remote store. It rolls
up smaller packs into progressively larger packs while leaving already-large
packs intact. This keeps the active inventory bounded without repeatedly
rewriting the entire repository as it grows.

Over time, as pushes create new pack files, the number of packs can grow. Repack
consolidates them while preserving the committed Git object set and atomically
replacing the segmented pack inventory selected by the unified manifest.

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--dry-run` | `false` | Report pack statistics without modifying the remote |

## How It Works

1. Pins the unified manifest and its exact segmented pack inventory.
2. Sorts packs by verified object count and selects the smallest suffix whose
   replacement restores a factor-two geometric progression.
3. Downloads only that suffix and its canonical indexes to a bounded temporary
   Git workspace after a free-space preflight. Stable large packs are neither
   read nor rewritten.
4. Verifies each selected pack/index pair, enumerates their exact OID union,
   and asks `git pack-objects` to produce one replacement pack. Crab verifies
   that the replacement contains exactly that union before publication.
5. Uploads immutable pack/index/reverse-index files only for newly generated
   packs, repairs missing metadata sidecars, and compacts the pack-index
   segment.
6. Performs one CAS of a new manifest generation with unchanged refs, HEAD,
   shard index, commit graph, and ref registry. Existing reachability proofs
   are rebound to the new generation because repack changes physical layout,
   not refs or reachable objects.
7. Publishes exact SlateDB object locators and a generation receipt. Locator
   or receipt failure is repairable and does not invalidate an already
   committed manifest.

Apply mode holds the repository-wide maintenance lease. The temporary
workspace lives under Crab's cache root and is removed automatically. Dry-run
reads only the canonical inventory and reports current bytes; replacement
size is unknown until Git performs the repack.

### Atomicity

The unified manifest is updated using one CAS (compare-and-swap). If any other
writer advances it after repack pins the input inventory, repack returns a
conflict instead of retrying against different repository state. Uploaded
immutable files remain unreferenced and are collected later under the normal GC
grace period. A rerun repairs missing post-CAS evidence without advancing an
already consolidated generation.

### Metadata Preservation

The replacement inventory records the complete pinned manifest ref-tip set for
new roll-up packs. Its object counts come from verified Git indexes rather than
estimates. Existing packs retain their original content-addressed identity and
metadata.

## Examples

### Dry run to see pack statistics

```bash
crab repack --dry-run
```

```
repack dry run: 15 packs, 2400000000 bytes, 0.1s
```

### Run repack

```bash
crab repack
```

```
repack complete: 15 → 6 packs, 2400000000 → 2300000000 bytes, 4.2s
```

## When to Run

- After many small pushes have accumulated numerous pack files.
- When `crab du --remote` shows high object counts.
- Through the continuously supervised `crab metadb owner`, which performs the
  same bounded geometric operation outside push acknowledgement.
- The `repack_auto_threshold` config option emits an advisory warning after a
  fetch when the pack count exceeds a threshold. It does not run repack in the
  developer's push/fetch path because consolidation is intentionally
  background maintenance.

## Configuration

The following settings in `.crab/config.toml` affect repack behavior:

| Key | Default | Description |
|-----|---------|-------------|
| `repack_auto_threshold` | `50` | Pack count that triggers an advisory warning |
| `download_concurrency` | `8` (capped at 16) | Maximum concurrent `.pack`/`.idx` downloads |

## Prerequisites

- The repository must be initialized with `crab init`.
- AWS credentials must be configured with read/write permissions on the bucket.

## Related Commands

- [`crab gc`](crab-gc.md) — garbage collect unreachable objects.
- [`crab fsck`](crab-fsck.md) — check repository integrity.
- [`crab du`](crab-du.md) — see storage usage breakdown.

## JSON Output

Supports `--json` and `--jsonl`.

- `--json` runs to completion and emits a single result envelope.
- `--jsonl` streams progress followed by a terminal `result` event.

### crab repack --json

```json
{
  "schema": "repack",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:21.400Z",
  "data": {
    "packs_before": 15,
    "packs_after": 6,
    "bytes_before": 2400000000,
    "bytes_after": 2300000000,
    "bytes_read": 180000000,
    "bytes_written": 165000000,
    "elapsed_ms": 4200
  }
}
```

### crab repack --jsonl

```
{"schema":"repack.event","version":"1.0","timestamp":"2026-04-24T18:32:18.000Z","type":"progress","data":{"operation":"repacking","current":8,"total":15,"bytes":1200000000,"total_bytes":2400000000,"rate_bytes_per_sec":300000000.0}}
{"schema":"repack.event","version":"1.0","timestamp":"2026-04-24T18:32:21.400Z","type":"result","data":{"packs_before":15,"packs_after":6,"bytes_before":2400000000,"bytes_after":2300000000,"bytes_read":180000000,"bytes_written":165000000,"elapsed_ms":4200}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.
