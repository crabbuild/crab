# crab lock / crab unlock / crab locks

Advisory file locking for collaborative workflows.

## Synopsis

```
crab lock [OPTIONS] <PATHS>...
crab unlock [OPTIONS] <PATHS>...
crab locks [OPTIONS]
```

## Description

Crab provides advisory file locking to prevent wasted work when multiple
collaborators edit the same non-mergeable binary file. Lock records are stored
as JSON objects in the remote bucket, using atomic compare-and-swap (CAS) for
acquisition.

Locks are advisory — they don't prevent file modification, but they signal to
other collaborators that someone is actively working on a file.

## Commands

### crab lock

Acquire an advisory lock on one or more files.

| Argument/Option | Required | Description |
|-----------------|----------|-------------|
| `paths` | Yes | File paths to lock |
| `--json` | No | Machine-readable JSON output |

### crab unlock

Release an advisory lock on one or more files.

| Argument/Option | Required | Description |
|-----------------|----------|-------------|
| `paths` | Yes | File paths to unlock |
| `--force` | No | Force-break another user's lock |
| `--json` | No | Machine-readable JSON output |

### crab locks

List all active advisory file locks.

| Option | Short | Description |
|--------|-------|-------------|
| `--path` | `-p` | Filter by file path |
| `--owner` | `-o` | Filter by owner ("self" for your own locks) |
| `--limit` | `-l` | Maximum number of locks to display |
| `--json` | `-j` | Machine-readable JSON output |

## How It Works

Lock records are stored in the remote bucket at:

```
{repo-path}/locks/files/{blake3(path)}
```

- Lock acquisition uses `PutMode::Create` (S3 conditional write) for atomic,
  race-free locking.
- The lock owner is determined from `git config user.email`, falling back to
  `git config user.name`.
- Each lock record contains: file path, owner identity, lock ID, and timestamp.

Note: `crab lfs lock` uses a separate namespace (`lfs/locks/`) for Git LFS
protocol compatibility. The native `crab lock` command uses `locks/files/`.

## Examples

### Lock a file

```bash
crab lock models/weights.bin
```

```
Locked models/weights.bin
```

### Lock multiple files

```bash
crab lock models/weights.bin data/train.bin
```

### Unlock a file

```bash
crab unlock models/weights.bin
```

```
Unlocked models/weights.bin
```

### Force-unlock another user's lock

```bash
crab unlock --force models/weights.bin
```

### List all locks

```bash
crab locks
```

```
O models/weights.bin    alice@example.com    ID:abc123
  data/train.bin        bob@example.com      ID:def456
```

The `O` marker indicates locks owned by you.

### List your own locks

```bash
crab locks --owner self
```

### List locks for a specific file

```bash
crab locks --path models/weights.bin
```

### JSON output

```bash
crab locks --json
```

```json
[{"path":"models/weights.bin","owner":"alice@example.com","id":"abc123","locked_at":"2024-01-15T10:30:00Z"}]
```

## Lock Conflicts

If you try to lock a file that's already locked by someone else:

```bash
crab lock models/weights.bin
```

```
error: models/weights.bin is locked by bob@example.com
```

## Path Normalization

File paths are normalized before locking:
- Leading `./` is stripped.
- Backslashes are converted to forward slashes.
- Paths are made relative to the repository root.

## Prerequisites

- The repository must be initialized with `crab init`.
- AWS credentials must be configured for the remote bucket.
- `git config user.email` (or `user.name`) must be set for lock ownership.

## Related Commands

- [`crab lfs lock`](crab-lfs.md) — LFS-compatible file locking.
- [`crab fsck`](crab-fsck.md) — detect expired push locks.

## JSON Output

All three commands (`lock`, `unlock`, `locks`) support `--json`. The `--json`
flag now wraps payloads in the standard envelope (version `1.1`).

### crab locks --json

```json
{
  "schema": "locks",
  "version": "1.1",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": [
    {
      "path": "models/weights.bin",
      "owner": "alice@example.com",
      "id": "abc123",
      "locked_at": "2024-01-15T10:30:00Z"
    }
  ]
}
```

### crab lock --json

```json
{
  "schema": "lock",
  "version": "1.1",
  "timestamp": "2026-04-24T18:32:17.123Z",
  "data": {
    "path": "models/weights.bin",
    "owner": "alice@example.com",
    "id": "def456",
    "locked_at": "2026-04-24T18:32:17Z"
  }
}
```

See [Structured Output](structured-output.md) for envelope details and error handling.
