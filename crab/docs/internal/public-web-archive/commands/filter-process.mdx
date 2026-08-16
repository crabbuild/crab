# crab filter-process

Git clean/smudge filter driver.

## Synopsis

```
crab filter-process
```

## Description

`crab filter-process` implements git's long-running filter process protocol.
It is invoked automatically by git during checkout, commit, and other operations
that touch crab-tracked files. It is not intended for direct user invocation.

When git encounters a file with `filter=crab` in `.gitattributes`, it routes
the file through this filter:

- **Clean** (commit direction): Replaces file content with a pointer blob
  containing the content hash, chunk count, and size.
- **Smudge** (checkout direction): Replaces a pointer blob with the original
  file content by downloading and reconstructing chunks from the remote store
  (or returns the pointer as-is in lazy mode).

## How It Works

The filter process runs as a long-lived subprocess of git, communicating via
stdin/stdout using git's packet-line protocol. This avoids the overhead of
spawning a new process for each file.

### Clean (content → pointer)

1. Receives file content from git.
2. Computes a Blake3 hash of the content.
3. Performs content-defined chunking (CDC).
4. Stages chunks in the local staging area.
5. Returns a pointer blob to git.

### Smudge (pointer → content)

1. Receives a pointer blob from git.
2. If lazy mode is enabled, returns the pointer as-is (fast).
3. If not lazy, resolves the file hash to chunk data.
4. Downloads missing chunks from the remote store.
5. Reconstructs the original content.
6. Returns the full content to git.

## Configuration

The filter is registered in git config by `crab init` or `crab install`:

```ini
[filter "crab"]
    process = crab filter-process
    clean = crab filter-process
    smudge = crab filter-process
    required = true
```

With `--skip-smudge` (for lazy mode):

```ini
[filter "crab"]
    smudge = cat
```

## When Is It Invoked?

- `git add` — clean filter converts content to pointer.
- `git checkout` — smudge filter converts pointer to content (or passes through
  in lazy mode).
- `git clone` — smudge filter runs on initial checkout.
- `git stash` / `git merge` — both clean and smudge may be invoked.

## Troubleshooting

**Filter process crashes**
Check `crab logs last` for error details. Common causes:
- Missing AWS credentials.
- Corrupt staging area (run `crab staging clean --force`).
- Incompatible crab version.

**Slow checkouts**
If smudge is downloading content for every file, you may want lazy mode:
```bash
crab config set checkout.lazy true
crab install --skip-smudge
```

## Related Commands

- [`crab install`](crab-install.md) — register the filter driver.
- [`crab config`](crab-config.md) — configure lazy mode.
- [`crab add`](crab-add.md) — parallel alternative to git add for large files.
