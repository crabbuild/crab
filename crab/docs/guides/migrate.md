# crab migrate

Inspect large-file history and convert DVC workflow state into Crab metadata.
The history-rewrite commands use Git's built-in fast-export/fast-import engine.
They require a clean working tree, stage verified Crab content locally, and
rewrite the selected refs atomically from Git's point of view. Back up the
repository before running them; after a rewrite, collaborators must re-clone
and the rewritten refs require a force push.

## Synopsis

```
crab migrate info [OPTIONS]
crab migrate import [OPTIONS]
crab migrate export [OPTIONS]
```

## Description

`crab migrate` provides an analysis tool (`info`), dry-run previews, and
verified history conversion. `migrate import` replaces selected regular Git
blobs with Crab pointers and stages their Xet chunks in `.crab/staging`.
`migrate export` reconstructs selected Crab pointers through the configured
Crab remote, verifies their file hashes, and writes regular Git blobs back to
history. Neither command requires `git-filter-repo`.

## Subcommands

### crab migrate info

Analyze the repository to identify large files that would benefit from crab
tracking.

| Option | Default | Description |
|--------|---------|-------------|
| `--above` | `1048576` (1 MB) | Only consider files above this size in bytes |
| `--top` | `10` | Show the top N file extensions |

### crab migrate import

Convert large files in history to crab pointers.

| Option | Default | Description |
|--------|---------|-------------|
| `--include` | (required) | Glob patterns for files to convert |
| `--exclude` | | Glob patterns to exclude from migration |
| `--above` | `1048576` (1 MB) | Only migrate files above this size |
| `--dry-run` | `false` | Report what would be migrated without changing refs or staging objects |
| `--everything` | `false` | Include all refs instead of the current branch |

### crab migrate export

Convert crab pointers back to full files in history.

| Option | Default | Description |
|--------|---------|-------------|
| `--include` | (required) | Glob patterns for files to convert back |
| `--dry-run` | `false` | Report what would be exported without changing refs |

## Examples

### Analyze which files would benefit from migration

```bash
crab migrate info
```

```
Extension         Total Size    Count
-------------------------------------
*.bin              12.4 GB        42
*.safetensors       8.2 GB        15
*.h5                3.1 GB         8
*.onnx              1.5 GB         3
*.tar.gz            800 MB         5
```

### Analyze with a higher size threshold

```bash
crab migrate info --above 10485760 --top 5
```

Only shows files above 10 MB, top 5 extensions.

### Dry run import

```bash
crab migrate import --include '*.bin' --dry-run
```

```
migrate import (dry run):
  include: ["*.bin"]
  exclude: []
  above: 1048576 bytes
  everything: false
  (no changes will be made)
```

### Import large files into crab tracking

```bash
crab migrate import --include '*.bin' --include '*.safetensors'
```

### Import across all branches

```bash
crab migrate import --include '*.bin' --everything
```

### Import with size threshold

```bash
crab migrate import --include '*' --above 5242880
```

Converts all files above 5 MB to crab pointers.

### Export pointers back to full files

```bash
crab migrate export --include '*.bin'
```

### Dry run export

```bash
crab migrate export --include '*.bin' --dry-run
```

## Important Warnings

- History rewriting is a destructive operation. Always back up your repository
  before running `migrate import` or `migrate export`.
- After rewriting, all collaborators must re-clone the repository.
- Force-pushing rewritten history will break existing clones.
- `--everything` rewrites all branches — use with extreme caution.

## Prerequisites

- A clean Git working tree is required for both rewrite commands.
- `migrate import` needs a writable `.crab/staging` directory; it can be run
  before `crab init` and uploads occur later when the rewritten refs are
  pushed.
- `migrate export` needs a configured Crab remote and read access to the
  selected pointer recipes and shard/xorb objects.

## Workflow

### Migrating an existing repository to crab

1. Analyze which files to migrate:
   ```bash
   crab migrate info
   ```

2. Back up the repository:
   ```bash
   cp -r my-repo my-repo-backup
   ```

3. Run the migration:
   ```bash
   crab migrate import --include '*.bin' --include '*.safetensors' --everything
   ```

4. Verify the result:
   ```bash
   crab status
   crab fsck
   ```

5. Force-push the rewritten history:
   ```bash
   git push --force-with-lease origin --all
   ```

6. Notify collaborators to re-clone.

## Related Commands

- [`crab track`](crab-track.md) — track new files (without rewriting history).
- [`crab add`](crab-add.md) — stage files for crab.
- [`crab lfs migrate`](crab-lfs.md) — LFS-compatible migration.
