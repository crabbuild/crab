# crab diff-driver

Git external diff driver for chunk-level diffs.

## Synopsis

```
crab diff-driver <path> <old-file> <old-hex> <old-mode> <new-file> <new-hex> <new-mode>
```

## Description

`crab diff-driver` conforms to git's external diff driver protocol. It is not
intended for direct user invocation — instead, it is registered via
`.gitattributes` and `.git/config` so that `git diff` automatically uses
chunk-level comparison for crab-tracked files.

When git encounters a diff for a file with `diff=crab` in `.gitattributes`,
it invokes this driver with the old and new file versions. The driver parses
both as crab pointers, resolves their reconstruction terms (chunk lists), and
compares them to show which chunks changed.

## Arguments

| Argument | Description |
|----------|-------------|
| `path` | File path being diffed |
| `old-file` | Path to the old file version (temp file created by git) |
| `old-hex` | Old file hex hash |
| `old-mode` | Old file mode |
| `new-file` | Path to the new file version (temp file created by git) |
| `new-hex` | New file hex hash |
| `new-mode` | New file mode |

## Setup

`crab init` and `crab install` register the `diff.crab.command` git config.
`crab track` writes the `.gitattributes` entry that selects that driver:

```
*.bin filter=crab diff=crab merge=crab -text
```

You also need the driver configured in `.git/config`:

```ini
[diff "crab"]
    command = crab diff-driver
```

This is set up automatically by `crab init` or `crab install`.

## How It Works

1. Git invokes the driver with paths to temporary files containing the old and
   new versions.
2. The driver reads both files and attempts to parse them as crab pointers.
3. If Git passed hydrated working-tree content on one side, the driver reports
   a size-only modification and asks you to run `git add` for chunk diff.
4. If both sides are pointers, it resolves reconstruction terms from shard metadata.
5. Compares chunk lists to determine added, removed, and modified segments.
6. Formats the output to stdout.
7. If neither file is a pointer, falls back to showing a file size difference.

## Output

For crab-tracked files:

```
models/weights.bin:
  423 segments changed (+312 added, -111 removed)
  Delta: +1.2 GB
  Dedup ratio: 73.8%
```

For non-tracked files:

```
models/weights.bin: 1.2 GB → 1.5 GB (not crab-tracked)
```

## Usage with git diff

Once configured, `git diff` automatically uses the chunk-level driver:

```bash
git diff HEAD~1          # uses crab diff-driver for tracked files
git diff main..feature   # chunk-level comparison across branches
```

## Related Commands

- [`crab diff`](crab-diff.md) — standalone chunk-level diff between refs.
- [`crab track`](crab-track.md) — register patterns with `diff=crab`.
- [`crab install`](crab-install.md) — register the diff driver in git config.
