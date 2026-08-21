# crab fsck

Check repository integrity.

## Synopsis

```
crab fsck [OPTIONS]
```

## Description

`crab fsck` cross-checks Crab manifests, pack/index presence, the shard/xorb
data chain, and coordination state against the object store.

With `--repair`, it can automatically fix certain categories of issues.

## Options

| Option | Default | Description |
|--------|---------|-------------|
| `--repair` | `false` | Attempt safe repairs for detected issues |

## Issue Categories

### Critical Issues

| Issue | Description | Repairable |
|-------|-------------|------------|
| Missing file index | A pointer references a file index that's not in any shard | No |
| Missing xorb | A shard references a xorb that doesn't exist in the store | No |
| Pack list divergence | A manifest-selected pack or canonical index is missing from storage | No |

### Warnings

| Issue | Description | Repairable |
|-------|-------------|------------|
| Orphan shard | A shard exists in the store but isn't referenced by any file index | No |
| Shard list divergence | The shard list doesn't match actual shard files | No |
| Orphan file index | A file index exists but isn't referenced by any pointer | No (informational) |

Unreferenced immutable pack files are expected after interrupted pushes,
conflicted manifest updates, and repack. They are GC candidates, not fsck
damage, and remain protected by the normal GC grace period.

### Repairable Issues

| Issue | Description | Repair Action |
|-------|-------------|---------------|
| Expired push lock | A push lock older than its backend-clock lease | Delete through holder-safe repair |
| Historical visibility backfill | A historical generation lacks its derivable Git visibility proof | Rebuild the proof |

## Examples

### Run integrity check

```bash
crab fsck
```

Example output:

```
crab fsck: 1 error(s), 0 info, 0 repaired, 0 repair failure(s)

1 issue found:
  ⚠ Expired push lock: refs/push-locks/abc123 (age: 3d 2h)
    Repairable: yes (run with --repair)
```

### Run with automatic repair

```bash
crab fsck --repair
```

```
crab fsck: 1 error(s), 0 info, 1 repaired, 0 repair failure(s)
```

## Repair Safety

- Only issues marked as "repairable" are fixed with `--repair`.
- Repairs are conservative and limited to issue types the report marks as
  repairable.
- Missing xorbs and other content damage require recovery from history, a
  replica, cache, or backup.
- Missing shard and xorb warning events can be fed to
  [`crab recover`](repository-recovery.md) with `--fsck-jsonl` to build a
  verified repair plan from cache, source, or replica candidates.

## When to Run

- After a push failure or crash to check for leftover locks.
- Periodically as a health check (e.g. in CI).
- Before running `crab gc` to ensure the reference graph is consistent.
- When `crab doctor` reports issues with the remote store.

## Prerequisites

- The repository must be initialized with `crab init`.
- AWS credentials must be configured for the remote bucket.
- `--repair` requires write permissions on the bucket.

## Verification Boundaries

- The Git-object check currently confirms that ref-journal records are
  readable; it does not reconstruct the current repository and run full Git
  reachability for missing commits, trees, or blobs. Use `crab recover history
  verify <generation>` for strict Git fsck of a selected historical root, and
  qualify current-head clone/fetch in production monitoring.
- Generic `object_store` does not expose provider multipart enumeration. The
  current multipart check therefore cannot find or abort remote incomplete
  uploads; configure the provider's incomplete-multipart lifecycle rule.
- Git locator and visibility acceleration damage is diagnostic. Rebuild it
  with `crab metadb rebuild`, then rerun `crab fsck`.
- Full backup recovery requires provider versioning/replication or another
  failure domain. Manifest history in the same bucket is not an independent
  backup.

## Related Commands

- [`crab gc`](crab-gc.md) — garbage collect unreachable objects.
- [`crab doctor`](crab-doctor.md) — local health check.
- [`crab repack`](crab-repack.md) — consolidate remote Git pack files.

## JSON Output

Supports `--json` and `--jsonl`.

- `--json` runs to completion and emits a single result envelope.
- `--jsonl` streams warnings per integrity issue followed by a terminal
  `result` event.

### crab fsck --json

```json
{
  "schema": "fsck",
  "version": "1.0",
  "timestamp": "2026-04-24T18:32:21.600Z",
  "data": {
    "checks_passed": 5,
    "checks_warned": 1,
    "checks_failed": 0,
    "issues": [
      { "severity": "warn", "category": "push_locks", "message": "1 expired lock (3 days old)", "repairable": true }
    ]
  }
}
```

### crab fsck --jsonl

```
{"schema":"fsck.event","version":"1.0","timestamp":"2026-04-24T18:32:18.200Z","type":"warning","data":{"code":"EXPIRED_LOCK","message":"expired push lock: refs/push-locks/abc123 (age: 3d 2h)","path":"refs/push-locks/abc123"}}
{"schema":"fsck.event","version":"1.0","timestamp":"2026-04-24T18:32:21.600Z","type":"result","data":{"checks_passed":5,"checks_warned":1,"checks_failed":0,"issues":[{"severity":"warn","category":"push_locks","message":"1 expired lock (3 days old)","repairable":true}]}}
```

See [Structured Output](structured-output.md) for envelope details, event types,
and error handling.
