# crab du

Show disk usage breakdown for crab-managed storage.

## Synopsis

```
crab du [OPTIONS]
```

## Description

`crab du` reports a detailed breakdown of disk usage for crab-managed data,
including the working tree, staging area, local cache, and optionally the remote
store. It helps you understand where storage is being consumed and identify
opportunities to reclaim space.

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--remote` | `false` | Include remote storage size (requires network access) |
| `--json` | `false` | Machine-readable JSON output |

## Output Sections

### Working Tree

- Total tracked files (matching `.gitattributes` patterns with `filter=crab`)
- Number of hydrated files (full content on disk)
- Number of pointer files (lightweight stubs)
- Disk space used by hydrated files
- Disk space used by pointer files

### Staging Area

- Size of `.crab/staging/` directory

### Local Cache

- Size of the local chunk cache (`~/.cache/crab` or `$CRAB_CACHE_DIR`)

### Remote (with `--remote`)

- Total size of objects in the remote bucket under the repository prefix

## Examples

### Basic disk usage report

```bash
crab du
```

Example output:

```
Working tree:
  Tracked files:     42
  Hydrated:          12 files (3.2 GB)
  Pointers:          30 files (4.5 KB)

Staging:             45.2 MB

Local cache:         1.8 GB

Total local:         5.0 GB
```

### Include remote storage

```bash
crab du --remote
```

Adds a remote section:

```
Remote:              8.4 GB (crab://my-bucket/my-repo)
```

### JSON output for scripting

```bash
crab du --json
```

```json
{
  "working_tree": {
    "tracked_files": 42,
    "hydrated_files": 12,
    "hydrated_bytes": 3435973836,
    "pointer_files": 30,
    "pointer_bytes": 4608
  },
  "staging_bytes": 47395635,
  "cache_bytes": 1932735283,
  "remote_bytes": null
}
```

### JSON with remote

```bash
crab du --json --remote
```

## Use Cases

- Before running `crab dehydrate` to see how much space you can reclaim.
- After `crab prune` to verify cache space was freed.
- Monitoring storage growth over time in CI pipelines (use `--json`).
- Comparing local vs. remote storage to identify sync issues.

## Related Commands

- [`crab dehydrate`](crab-dehydrate.md) — free disk space by replacing files with pointers.
- [`crab prune`](crab-prune.md) — remove unreferenced objects from the local cache.
- [`crab staging stats`](crab-staging.md) — detailed staging area statistics.
- [`crab cache stats`](crab-cache.md) — local cache statistics.

## JSON Output

Supports `--json`. The `--json` flag now wraps the payload in the standard
envelope (version `1.1` — the inner payload is unchanged from the original
`--json` output).

```bash
crab du --json
```

```json
{
  "schema": "du",
  "version": "1.1",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "working_tree": {
      "tracked_files": 42,
      "hydrated_files": 12,
      "hydrated_bytes": 3435973836,
      "pointer_files": 30,
      "pointer_bytes": 4608
    },
    "staging_bytes": 47395635,
    "cache_bytes": 1932735283,
    "remote_bytes": null
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
