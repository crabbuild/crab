# crab migrate

Inspect large-file history and convert DVC workflow state into Crab metadata.
The history-rewrite commands are currently dry-run only: non-dry-run requests
fail explicitly without changing the repository. Use `crab adopt` for the
supported working-tree cutover path.

## Synopsis

```
crab migrate info [OPTIONS]
crab migrate import [OPTIONS]
crab migrate export [OPTIONS]
```

## Description

`crab migrate` provides an analysis tool (info) and dry-run previews for
history conversion. Applying the history rewrite is not yet supported and
returns an explicit error without changing the repository.

Use `crab adopt` for the supported working-tree conversion path. Keep a
repository backup before any future history-rewrite implementation is used.

## Subcommands

### crab migrate info

Analyze the repository to identify large files that would benefit from crab
tracking.

| Option | Default | Description |
|--------|---------|-------------|
| `--above` | `1048576` (1 MB) | Only consider files above this size in bytes |
| `--top` | `10` | Show the top N file extensions |

### crab migrate import (dry-run only)

Convert large files in history to crab pointers.

| Option | Default | Description |
|--------|---------|-------------|
| `--include` | (required) | Glob patterns for files to convert |
| `--exclude` | | Glob patterns to exclude from migration |
| `--above` | `1048576` (1 MB) | Only migrate files above this size |
| `--dry-run` | `required` | Report what would be migrated; applying the rewrite is unsupported |
| `--everything` | `false` | Include all branches in the dry-run report |

### crab migrate export (dry-run only)

Convert crab pointers back to full files in history.

| Option | Default | Description |
|--------|---------|-------------|
| `--include` | (required) | Glob patterns for files to convert back |
| `--dry-run` | `required` | Report what would be exported; applying the rewrite is unsupported |

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

- `git-filter-repo` must be installed:
  ```bash
  pip install git-filter-repo
  ```
- The repository must be initialized with `crab init` (for import).
- AWS credentials must be configured (for import, to upload converted objects).

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
   git push --force origin --all
   ```

6. Notify collaborators to re-clone.

## Related Commands

- [`crab track`](crab-track.md) — track new files (without rewriting history).
- [`crab add`](crab-add.md) — stage files for crab.
- [`crab lfs migrate`](crab-lfs.md) — LFS-compatible migration.
