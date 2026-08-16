# crab status

Report hydration state of the working tree.

## Synopsis

```
crab status [OPTIONS]
```

## Description

`crab status` walks the working tree and reports the hydration state of every
crab-tracked file. For each file, it shows whether the file is hydrated (full
content on disk), a pointer (lightweight stub), or modified relative to the
committed version.

This is the quickest way to see which files are available locally and which
would need hydration before use.

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--porcelain` | `false` | Machine-readable output (one line per file) |

## Output Format

### Human-Readable (default)

```
crab status
  Tracked files:  42
  Hydrated:       12 (3.2 GB)
  Pointers:       30 (4.5 KB)
```

### Porcelain Mode

Each line has the format:

```
<state> <path>
```

Where `<state>` is one of:

| State | Meaning |
|-------|---------|
| `p` | Pointer file (not hydrated) |
| `h` | Hydrated (full content, matches committed pointer size) |
| `m` | Modified (hydrated but size differs from committed pointer) |

Example:

```
p models/weights.bin
h models/embeddings.bin
m data/train.safetensors
p data/eval.safetensors
```

## How It Works

1. Reads `.gitattributes` to identify crab-tracked patterns.
2. Walks the working tree, skipping hidden directories (`.git`, `.crab`).
3. For each file matching a tracked pattern:
   - If the file is small enough to be a pointer and parses as one → `p`.
   - If the file is full content:
     - Reads the committed pointer from git's index to compare sizes.
     - If sizes match → `h` (hydrated).
     - If sizes differ → `m` (modified).
4. Prints a summary (human mode) or per-file state (porcelain mode).

## Examples

### Quick status check

```bash
crab status
```

### Machine-readable output for scripting

```bash
crab status --porcelain
```

### Count hydrated files

```bash
crab status --porcelain | grep '^h ' | wc -l
```

### Find modified files

```bash
crab status --porcelain | grep '^m '
```

## Related Commands

- [`crab hydrate`](crab-hydrate.md) — materialize pointer files.
- [`crab dehydrate`](crab-dehydrate.md) — replace files with pointers.
- [`crab ls-files`](crab-ls-files.md) — list tracked files with hash details.
- [`crab du`](crab-du.md) — disk usage breakdown.

## JSON Output

Supports `--json`. Conflicts with `--porcelain`.

```bash
crab status --json
```

```json
{
  "schema": "status",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "total_tracked": 42,
    "hydrated": 12,
    "pointer": 30,
    "modified": 2,
    "files": [
      { "path": "models/weights.bin", "state": "hydrated", "bytes": 1288490188 },
      { "path": "models/embeddings.bin", "state": "pointer", "bytes": 128 },
      { "path": "data/train.safetensors", "state": "modified", "bytes": 314572800 }
    ]
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
