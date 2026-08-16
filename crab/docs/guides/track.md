# crab track / crab untrack

Manage `.gitattributes` entries for crab file tracking.

## Synopsis

```
crab track <glob>
crab untrack <glob>
```

## Description

`crab track` registers a glob pattern in `.gitattributes` so that matching
files are processed through the crab filter driver. This is how you tell
crab which files should be stored in cloud object storage rather than directly
in git.

`crab untrack` removes a previously tracked pattern from `.gitattributes`.

Both commands are idempotent: tracking an already-tracked pattern or untracking
a pattern that isn't tracked are safe no-ops.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `glob` | Yes | Glob pattern to track or untrack (e.g. `*.bin`, `*.safetensors`) |

## What It Does

### `crab track`

Adds a line to `.gitattributes` in the current directory:

```
<glob> filter=crab diff=crab merge=crab -text
```

This tells git to:
- Route the file through the crab filter driver for clean/smudge operations.
- Use crab's chunk-level diff driver for diffs.
- Use crab's merge driver for merges.
- Treat the file as binary (`-text`), preventing line-ending conversions.

If `.gitattributes` does not exist, it is created.

### `crab untrack`

Removes the exact crab attributes line for the given glob from
`.gitattributes`. Other lines (including non-crab attributes) are preserved.

## Examples

### Track binary model files

```bash
crab track '*.bin'
crab track '*.safetensors'
crab track '*.onnx'
```

### Track all files in a directory

```bash
crab track 'data/*'
```

### View tracked patterns

Check `.gitattributes` to see all tracked patterns:

```bash
cat .gitattributes
```

```
*.bin filter=crab diff=crab merge=crab -text
*.safetensors filter=crab diff=crab merge=crab -text
```

### Stop tracking a pattern

```bash
crab untrack '*.bin'
```

### Commit the tracking configuration

`.gitattributes` is a regular git file — commit it so collaborators get the
same tracking rules:

```bash
git add .gitattributes
git commit -m "Track *.safetensors with crab"
```

## Common Patterns

| Pattern | Matches |
|---------|---------|
| `*.bin` | All `.bin` files in any directory |
| `*.safetensors` | All SafeTensors model files |
| `*.onnx` | ONNX model files |
| `*.h5` | HDF5 files |
| `*.tar.gz` | Compressed archives |
| `data/*` | Everything in the `data/` directory |
| `*` | All files (use with caution) |

## Important Notes

- Tracking a pattern does not retroactively convert existing files. Use
  `crab add` to stage existing files, or `crab migrate import` to rewrite
  history.
- The `.gitattributes` file should be committed to the repository so all
  collaborators share the same tracking configuration.
- Untracking a pattern does not restore pointer files to full content. Use
  `crab hydrate` to materialize files, then untrack the pattern.

## Related Commands

- [`crab add`](crab-add.md) — stage tracked files for crab.
- [`crab migrate import`](crab-migrate.md) — convert existing files in history to crab pointers.
- [`crab ls-files`](crab-ls-files.md) — list files tracked by crab.
- [`crab status`](crab-status.md) — see hydration state of tracked files.

## JSON Output

`crab track` (list mode, no arguments) supports `--json`.

```bash
crab track --json
```

```json
{
  "schema": "track",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "patterns": [
      { "glob": "*.bin", "source": ".gitattributes" },
      { "glob": "*.safetensors", "source": ".gitattributes" }
    ]
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
