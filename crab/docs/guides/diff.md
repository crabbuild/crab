# crab diff

Chunk-level diff between two git refs.

## Synopsis

```
crab diff [OPTIONS] <ref1> [ref2] [-- <paths>...]
```

## Description

`crab diff` compares crab-tracked files between two git refs using only
metadata (file-index and shards). It shows which chunks changed, bytes affected,
and dedup ratio — all with zero data transfer. Unlike `git diff`, which would
need to download and reconstruct full files, `crab diff` operates entirely on
lightweight metadata.

This is especially powerful for multi-GB files where a traditional diff would be
impractical.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `ref1` | Yes | First git ref (branch, tag, SHA, `HEAD~N`) |
| `ref2` | No | Second git ref (defaults to `HEAD` when omitted) |
| `paths` | No | Restrict diff to specific paths (after `--`) |

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--json` | `false` | Output as JSON |
| `--stat` | `false` | Summary-only output (like `git diff --stat`) |
| `--name-only` | `false` | List only changed file names |
| `--verbose` | `false` | Show per-segment detail (xorb hashes, chunk ranges, sizes) |
| `--byte-ranges` | `false` | Show changed byte offset ranges within each file |
| `--no-color` | `false` | Disable colored output |
| `--no-annotations` | `false` | Disable format-aware annotations |

## How It Works

1. Resolves both git refs to tree objects.
2. Walks both trees to find crab-tracked files that differ.
3. For each changed file, parses the pointer to extract the file hash and shard
   hint.
4. Resolves reconstruction terms (chunk lists) from shard metadata — no actual
   file data is downloaded.
5. Compares chunk lists to determine which segments were added, removed, or
   modified.
6. Formats and displays the diff report.

## Examples

### Compare HEAD with the previous commit

```bash
crab diff HEAD~1
```

### Compare two branches

```bash
crab diff main feature/new-model
```

### Compare two tags

```bash
crab diff v1.0 v2.0
```

### Summary output

```bash
crab diff --stat HEAD~3
```

```
 models/weights.bin    | 423 chunks changed (+312, -111), +1.2 GB
 data/train.bin        | 12 chunks changed (+12, -0), +45 MB
 2 files changed, 435 segments, +1.245 GB delta
```

### Name-only output

```bash
crab diff --name-only v1.0 v2.0
```

```
models/weights.bin
data/train.bin
```

### Verbose output with chunk details

```bash
crab diff --verbose HEAD~1
```

Shows xorb hashes, chunk ranges, and sizes for each changed segment.

### Byte ranges

```bash
crab diff --byte-ranges HEAD~1
```

Shows the byte offset ranges within each file that changed.

### JSON output for tooling

```bash
crab diff --json HEAD~1 HEAD
```

### Restrict to specific paths

```bash
crab diff HEAD~1 -- models/
```

## Output Modes

| Mode | Flag | Description |
|------|------|-------------|
| Human-readable | (default) | Colored, formatted diff with chunk counts and sizes |
| Stat | `--stat` | One-line-per-file summary like `git diff --stat` |
| Name-only | `--name-only` | Just file paths, one per line |
| Verbose | `--verbose` | Full segment detail with xorb hashes |
| JSON | `--json` | Machine-readable JSON for CI/tooling |

## Prerequisites

- The repository must be initialized with `crab init`.
- AWS credentials must be configured (metadata is fetched from the remote).
- Both refs must exist in the local git repository.

## Related Commands

- [`crab diff-driver`](crab-diff-driver.md) — git external diff driver for chunk-level diffs.
- [`crab status`](crab-status.md) — see current hydration state.
- [`crab ls-files`](crab-ls-files.md) — list tracked files.

## JSON Output

Supports `--json`. The `--json` flag now wraps the payload in the standard
envelope (version `1.1` — the inner payload is unchanged).

```bash
crab diff --json HEAD~1 HEAD
```

```json
{
  "schema": "diff",
  "version": "1.1",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "files": [
      {
        "path": "models/weights.bin",
        "chunks_changed": 423,
        "chunks_added": 312,
        "chunks_removed": 111,
        "bytes_delta": 1288490188
      }
    ],
    "summary": {
      "files_changed": 1,
      "total_chunks_changed": 423,
      "total_bytes_delta": 1288490188
    }
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
