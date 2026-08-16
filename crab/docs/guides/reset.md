# crab reset

Unstage files from git's index and clean crab staging data.

## Synopsis

```
crab reset [OPTIONS] <PATTERNS>...
```

## Description

`crab reset` mirrors `git reset HEAD -- <paths>` and also removes the
corresponding chunk data from the local staging area. This is the inverse of
`crab add`: it unstages files from git's index and cleans up the chunks that
were staged for those files.

Use `--sync` mode after manual `git reset`, `git rm`, or other operations that
remove files from git's index to clean up orphaned staging data.

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `patterns` | Yes (unless `--sync`) | Glob patterns to unstage (e.g. `*.safetensors`, `models/`) |

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--dry-run` | `false` | Show what would be unstaged without modifying anything |
| `--sync` | `false` | Scan for files removed from git's index and clean orphaned staging data |

## How It Works

### Normal Mode

1. Collects files matching the provided glob patterns that are currently staged
   in git's index.
2. Runs `git reset HEAD -- <paths>` to unstage them.
3. Removes the corresponding chunk data from `.crab/staging/`.

### Sync Mode (`--sync`)

1. Scans the staging area for all staged file entries.
2. Checks each entry against git's current index.
3. Removes staging data for any files that are no longer in the index.

This is useful after manual git operations that bypass crab (e.g. `git reset`,
`git rm`, `git checkout -- <file>`).

## Examples

### Unstage specific files

```bash
crab reset '*.safetensors'
```

### Unstage files from a directory

```bash
crab reset 'models/*'
```

### Preview what would be unstaged

```bash
crab reset --dry-run '*.bin'
```

### Clean orphaned staging data

After running `git reset HEAD -- models/weights.bin` manually:

```bash
crab reset --sync
```

This scans the staging area and removes data for files no longer in the index.

## Prerequisites

- The repository must be initialized with `crab init`.
- Files must have been previously staged with `crab add`.

## Related Commands

- [`crab add`](crab-add.md) — stage files for crab.
- [`crab staging clean`](crab-staging.md) — purge stale staging data.
- [`crab status`](crab-status.md) — see hydration state of tracked files.
