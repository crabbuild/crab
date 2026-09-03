# crab hydrate

Materialize pointer files into full content.

## Synopsis

```
crab hydrate [OPTIONS] [GLOBS]...
```

## Description

`crab hydrate` downloads chunk data from the remote store and reconstructs
the original file content, replacing pointer stubs with the actual files. This
is the primary way to "check out" large files after a lazy clone or after
switching branches.

Hydration is selective: you can hydrate specific files by glob pattern, or use
`--all` to hydrate everything. Files already replaced with full content are not
selected again.

Explicit hydration, post-clone eager/selective hydration, clone's `always`
prefetch profile, post-pull hydration and init's configured auto-patterns use
the same cloud-reader composition. Clone profiles resolve configuration and
files from the cloned repository, not the parent directory. Restore defaults
come from that repository's resolved configuration; explicit hydrate flags
override them. A configured remote error is returned rather than switching to
local staging. Local unpublished staging remains available when no remote is
configured. Automatic clone profile failures remain warnings; retry with
`crab hydrate --profile always` after addressing the reported cause.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `GLOBS` | No | Positional glob patterns to hydrate (e.g. `*.safetensors`) |

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--include` | | Additional include patterns (composable with positional globs) |
| `--exclude` | | Exclude patterns (subtract from includes) |
| `--all` | `false` | Hydrate all tracked pointer files |

## How It Works

1. Reads `.gitattributes` to identify crab-tracked patterns.
2. Walks the working tree to find pointer files matching the requested patterns.
3. For each pointer file:
   a. Parses the pointer to extract the file hash and shard hint.
   b. Resolves the file index from the shard metadata.
   c. Downloads the required xorb chunks from the remote store (or local cache).
   d. Reconstructs the original file content from chunks.
   e. Verifies the full BLAKE3 hash and atomically replaces the pointer.
   f. Refreshes Git's stat entry and records the exact verified stat for the
      first subsequent `crab add`.
4. Prints a summary of hydrated files, bytes downloaded, and elapsed time.

If another process hydrates a selected pointer during the run, Crab skips it
only after its size and full BLAKE3 hash match the pointer.

## Examples

### Hydrate specific file types

```bash
crab hydrate '*.safetensors'
```

### Hydrate multiple patterns

```bash
crab hydrate '*.bin' '*.safetensors'
```

### Hydrate with include/exclude

```bash
crab hydrate --include '*.bin' --exclude 'archive/*'
```

### Hydrate everything

```bash
crab hydrate --all
```

### Hydrate with only exclude (implies include all)

```bash
crab hydrate --exclude 'data/train/*'
```

This hydrates all tracked files except those under `data/train/`.

## Output

```
Hydrating 5 files...
  models/weights.bin       1.2 GB  ✓
  models/embeddings.bin    800 MB  ✓
  data/eval.safetensors    300 MB  ✓
  data/test.safetensors    150 MB  ✓
  config/vocab.bin         2.1 MB  ✓

Hydrated 5 files (2.5 GB) in 12.3s
  Skipped: 3 files (already hydrated)
```

## Pattern Resolution

Patterns are resolved in this order of precedence:

1. Positional glob arguments (`crab hydrate '*.bin'`)
2. `--include` / `--exclude` flags
3. `--all` flag (overrides all patterns)
4. If none specified, falls back to `hydrate.include` / `hydrate.exclude` from
   `.crab/local.toml`
5. If still nothing, prints help and exits

## Performance Tips

- Pre-warm the cache with `crab fetch` before hydrating to avoid
  download latency during hydration.
- Hydrate only what you need — lazy checkout is the whole point.
- In linked Git worktrees, Crab automatically uses a verified filesystem CoW
  clone when a sibling already has the same pointer hydrated. Unsupported or
  cross-filesystem cases fall back to normal chunk/Xorb hydration.
- The hydrator uses concurrent downloads. Configure
  `download_concurrency` in `.crab/local.toml` to tune parallelism.
- For very large files, ensure sufficient disk space — the full file is
  written atomically, so you need space for both the pointer and the
  reconstructed file briefly.

## Manifest Hydration

When you know exactly which files a job needs — CI builds, sparse checkouts,
targeted developer workflows — manifest hydration lets you hydrate a precise
set of paths without walking the entire tree.

A **manifest** is a newline-delimited text file listing paths and globs
relative to the repo root. Blank lines and lines starting with `#` are
ignored.

### Manifest file format

```
# CI manifest — hydrate only what the build needs
src/main.rs
src/**/*.rs
*.toml
Cargo.lock
tests/fixtures/small/**
```

Each line is either a literal path (`src/main.rs`) or a glob pattern
(`src/**/*.rs`). The resolver expands globs against the working tree and
batches the resulting files into shard-grouped fetches, so hydrating 1 000
files from a 10 M-file repo is fast — typically under 10 seconds on a warm
cache.

### `--manifest <path>`

Point at a manifest file on disk:

```bash
crab hydrate --manifest .crab/manifests/ci.txt
```

The path is relative to the current directory (not the repo root).

### `--manifest -` (stdin)

Pass `-` to read the manifest from stdin. This composes with any tool that
produces a file list:

```bash
# Hydrate only Rust source files tracked by git
git ls-files '*.rs' | crab hydrate --manifest -

# Hydrate files changed in the last commit
git diff --name-only HEAD~1 | crab hydrate --manifest -
```

### `--manifest-ref <ref>`

Read a manifest that is committed to the repo without checking it out first.
The ref uses standard git revision syntax:

```bash
# Read from HEAD
crab hydrate --manifest-ref HEAD:.crab/manifests/ci.txt

# Read from a specific branch
crab hydrate --manifest-ref origin/main:.crab/manifests/ci.txt
```

This is useful in CI where the working tree may be a sparse checkout that
doesn't include the manifest file itself.

### JSONL progress output

Combine `--manifest` with `--jsonl` to get machine-readable per-file progress
and a final summary — handy for CI dashboards and build tooling:

```bash
crab hydrate --manifest ci.txt --jsonl
```

Each hydrated file emits a progress row, followed by a summary record:

```
{"schema":"hydrate.event","version":"1.0","type":"file_done","data":{"path":"src/main.rs","bytes":4096,"duration_ms":12,"status":"ok"}}
{"schema":"hydrate.event","version":"1.0","type":"file_done","data":{"path":"Cargo.lock","bytes":81920,"duration_ms":45,"status":"ok"}}
{"schema":"hydrate.event","version":"1.0","type":"result","data":{"files_hydrated":42,"bytes_hydrated":10485760,"files_skipped":3,"duration_ms":8200}}
```

See [Structured Output](structured-output.md) for the full envelope schema.

### Manifest hydration examples

```bash
# Hydrate from a manifest file
crab hydrate --manifest .crab/manifests/ci.txt

# Pipe from git ls-files
git ls-files '*.rs' | crab hydrate --manifest -

# Read manifest from a committed file
crab hydrate --manifest-ref HEAD:.crab/manifests/ci.txt

# With JSONL progress output
crab hydrate --manifest ci.txt --jsonl
```

## Prerequisites

- The repository must be initialized with `crab init`.
- AWS credentials must be configured for the remote bucket.
- Files must be tracked in `.gitattributes` with `filter=crab`.

## Related Commands

- [`crab dehydrate`](crab-dehydrate.md) — replace hydrated files with pointers.
- [`crab clone`](crab-clone.md) — clone with optional selective hydration.
- [`crab fetch`](crab-fetch.md) — pre-fetch objects into the local cache.
- [`crab status`](crab-status.md) — see which files are hydrated vs. pointers.

## JSON Output

Supports `--json` and `--jsonl`.

- `--json` runs to completion and emits a single result envelope.
- `--jsonl` streams per-file progress followed by a terminal `result` event.

### crab hydrate --all --json

```json
{
  "schema": "hydrate",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:29.800Z",
  "data": {
    "hydrated": 5,
    "bytes_written": 2469606195,
    "skipped": 3,
    "bytes_skipped": 1048576,
    "failed": 0,
    "cow_cloned": 2,
    "bytes_cow_cloned": 2147483648,
    "duration_ms": 12300
  }
}
```

### crab hydrate --all --jsonl

```
{"schema":"hydrate.event","version":"1.0","timestamp":"2026-04-24T18:32:17.200Z","type":"progress","data":{"operation":"hydrating","current":2,"total":5,"bytes":838860800,"total_bytes":2469606195,"rate_bytes_per_sec":68000000.0}}
{"schema":"hydrate.event","version":"1.0","timestamp":"2026-04-24T18:32:18.100Z","type":"file_done","data":{"path":"models/weights.bin","bytes":1288490188,"duration_ms":1800,"status":"ok"}}
{"schema":"hydrate.event","version":"1.0","timestamp":"2026-04-24T18:32:29.800Z","type":"result","data":{"hydrated":5,"bytes_written":2469606195,"skipped":3,"bytes_skipped":1048576,"failed":0,"cow_cloned":2,"bytes_cow_cloned":2147483648,"duration_ms":12300}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.
