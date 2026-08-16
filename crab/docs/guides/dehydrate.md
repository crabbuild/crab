# crab dehydrate

Replace hydrated files with pointer blobs, freeing disk space.

## Synopsis

```
crab dehydrate [OPTIONS] [GLOBS]...
```

## Description

`crab dehydrate` is the inverse of `crab hydrate`. It replaces fully
materialized files with their lightweight pointer stubs, freeing disk space
while keeping the file tracked in git. The original content remains safely
stored in the remote object store and can be re-hydrated at any time.

Dehydration is safe: it only replaces clean files whose committed Git blob is
an existing Crab pointer with the same content hash. Files with uncommitted
modifications or raw Git blobs are left unchanged.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `GLOBS` | No | Positional glob patterns to dehydrate (e.g. `*.safetensors`) |

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--all` | `false` | Dehydrate all tracked hydrated files |

## How It Works

1. Reads `.gitattributes` to identify crab-tracked patterns.
2. Walks the working tree to find hydrated files (non-pointer files matching
   tracked patterns).
3. Queries `git status` to identify dirty (modified/untracked) files.
4. For each clean, hydrated file:
   a. Reads the file content.
   b. Computes the Blake3 hash and verifies it against the committed Crab pointer.
   c. Atomically writes that pointer in place of the full file while preserving
      the file's permissions.
5. Skips any file that has uncommitted modifications.
6. Prints a summary of dehydrated files and bytes freed.

## Examples

### Dehydrate specific file types

```bash
crab dehydrate '*.safetensors'
```

### Dehydrate everything

```bash
crab dehydrate --all
```

### Dehydrate files in a directory

```bash
crab dehydrate 'models/*'
```

## Output

```
Dehydrating 5 files...
  models/weights.bin       1.2 GB → 128 B  ✓
  models/embeddings.bin    800 MB → 128 B  ✓
  data/eval.safetensors    300 MB → 128 B  ✓
  data/dirty.bin           skipped (modified)

Dehydrated 3 files, freed 2.3 GB in 0.4s
  Skipped: 1 file (dirty)
```

## Safety

- Files with uncommitted changes are never dehydrated. This prevents data loss
  from replacing modified content with a stale pointer.
- Files without a committed Crab pointer are never dehydrated because Crab
  cannot prove that their content is reconstructable from remote storage.
- Pointer files (already dehydrated) are skipped automatically.
- The write is atomic: a temporary file is written first, then renamed into
  place. A crash mid-dehydration leaves the original file intact, and the
  replacement keeps the original file permissions.

## When to Use

- After finishing work on a set of large files to reclaim disk space.
- Before switching branches when you don't need the current branch's large
  files.
- On CI runners to free space after processing.
- When your disk is running low and you want to keep only the files you're
  actively working on.

## Prerequisites

- The repository must be initialized with `crab init`.
- Files must be tracked in `.gitattributes` with `filter=crab`.
- Files must be committed (dirty files are skipped).

## Related Commands

- [`crab hydrate`](crab-hydrate.md) — materialize pointer files into full content.
- [`crab status`](crab-status.md) — see which files are hydrated vs. pointers.
- [`crab du`](crab-du.md) — see disk usage breakdown.

## JSON Output

Supports `--json` and `--jsonl`.

- `--json` runs to completion and emits a single result envelope.
- `--jsonl` streams per-file progress followed by a terminal `result` event.

### crab dehydrate --all --json

```json
{
  "schema": "dehydrate",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.900Z",
  "data": {
    "files_dehydrated": 3,
    "bytes_freed": 2336462028,
    "files_skipped": 1,
    "duration_ms": 400
  }
}
```

### crab dehydrate --all --jsonl

```
{"schema":"dehydrate.event","version":"1.0","timestamp":"2026-04-24T18:32:17.500Z","type":"file_done","data":{"path":"models/weights.bin","bytes":1288490188,"duration_ms":120,"status":"ok"}}
{"schema":"dehydrate.event","version":"1.0","timestamp":"2026-04-24T18:32:17.700Z","type":"file_done","data":{"path":"data/dirty.bin","bytes":0,"duration_ms":0,"status":"skipped"}}
{"schema":"dehydrate.event","version":"1.0","timestamp":"2026-04-24T18:32:17.900Z","type":"result","data":{"files_dehydrated":3,"bytes_freed":2336462028,"files_skipped":1,"duration_ms":400}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.
