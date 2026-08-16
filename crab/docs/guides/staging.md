# crab staging

Manage the staging area.

## Synopsis

```
crab staging stats
crab staging clean [OPTIONS]
```

## Description

The crab staging area (`.crab/staging/`) is where chunk data is stored
locally between `crab add` and `git push`. The `crab staging` command
provides subcommands to inspect and clean this area.

The SQLite index records immutable recipes, content-addressed payload locators,
native-byte path leases, and add batches. Rollback removes only the failed
batch's leases. A push pins an immutable recipe snapshot so another push cannot
retire payloads while they are being packed. Optional prepared xorbs are derived
cache entries; segment payloads remain the recovery source.

## Subcommands

### crab staging stats

Print staging area statistics.

```bash
crab staging stats
```

Uses a read-only (shared lock) handle so it never blocks concurrent writers.

Example output:

```
Staging area: .crab/staging
  Sealed segments:       3
  Current segment bytes: 1234567
  Total staged bytes:    45678901
  Live bytes:            40000000
  Dead bytes:            5678901
  Dead ratio:            12.43%
  Chunk count:           847
  File count:            12
  Inflight markers:      0
```

| Field | Description |
|-------|-------------|
| Sealed segments | Number of completed segment files |
| Current segment bytes | Bytes in the active (unsealed) segment |
| Total staged bytes | Total bytes across all segments |
| Live bytes | Bytes referenced by current pointer files |
| Dead bytes | Bytes no longer referenced (eligible for cleanup) |
| Dead ratio | Percentage of dead bytes |
| Chunk count | Total number of chunks stored |
| File count | Number of files with staged data |
| Inflight markers | Number of in-progress operations |

## What Lives in Staging

The staging root contains three important classes of data:

| Path | Role |
|------|------|
| `index.db` | SQLite metadata for schema migration, batches, recipes, occurrences, native-byte path leases, payload locators, push snapshots, and optional prepared-xorb leases |
| `segments/` | Append-only chunk payload files. These are the authoritative local bytes until push retirement or staging cleanup removes them |
| `push-plans/xorbs/` | Derived prepared-xorb payload files referenced by indexed add-time plans |

When manifest CAS succeeds, push marks its recipe snapshot committed and retires
published leases only when no open snapshot still needs them. A crash leaves
marker/snapshot state for `crab doctor` and `crab staging clean` to report or
prune; sole quarantined payload bytes are never deleted by migration.

### crab staging clean

Purge stale staging data.

```bash
crab staging clean [--force]
```

| Option | Default | Description |
|--------|---------|-------------|
| `--force` | `false` | Force-break a stale lock held by a dead process |

Cleaning performs:
- Removes stale inflight markers from crashed processes.
- Sweeps orphan segments that are no longer referenced.
- Optionally compacts remaining segments.

Example output:

```
Staging clean complete:
  Segments removed:      2
  Segments compacted:    1
  Bytes reclaimed:       12345678
  Chunks reclaimed:      234
  Stale markers removed: 1
```

## When to Use

### `staging stats`

- To understand how much data is staged locally.
- To check the dead ratio — a high dead ratio means `staging clean` would
  reclaim significant space.
- To verify that inflight markers are zero (no stuck operations).

### `staging clean`

- After a push to clean up data that's been uploaded to the remote.
- After `crab reset` to remove orphaned staging data.
- When the staging area is consuming too much disk space.
- After a crash, with `--force` to break stale locks.

## Force Mode

The `--force` flag attempts to break a stale lock held by a dead process. This
is safe because:
- The lock is advisory (`flock`).
- A PID liveness check ensures only locks from dead processes are broken.

Use `--force` only when `staging clean` reports a lock error and you're sure no
other crab process is running.

## Related Commands

- [`crab add`](crab-add.md) — stage files (writes to the staging area).
- [`crab reset`](crab-reset.md) — unstage files and clean staging data.
- [`crab du`](crab-du.md) — disk usage breakdown including staging.
- [`crab stat`](crab-stat.md) — staging area statistics (alternative).
