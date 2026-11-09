# Health and Diagnostics

The Health & Diagnostics section provides system-level diagnostics for troubleshooting
issues with Crab Desktop. It shows agent status, cache health, disk
usage, and provides tools for generating diagnostic reports.

## Accessing Health & Diagnostics

- Open **Settings** from the activity bar or press `Cmd+,` / `Ctrl+,`.
- Select **Diagnostics**. The health report appears at the top, followed by
  notification settings, logs, PTY daemon diagnostics, and telemetry.

## Layout

```
┌─────────────────────────────────────────────────────────────────┐
│  Health & Diagnostics                          [Refresh] [Copy] │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Agent Status                                    🟢 OK  │    │
│  │  Version: 0.1.0                                         │    │
│  │  CLI Version: crab 0.6.0                              │    │
│  │  Uptime: 2h 34m                                         │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Cache                                                  │    │
│  │  Directory: ~/.cache/crab                             │    │
│  │  Used: 2.4 GiB                                          │    │
│  │  Free: 48.2 GiB                                         │    │
│  │  [Clear Cache]                                          │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Staging                                                │    │
│  │  Size: 156 MiB                                          │    │
│  │  [Clean Staging]                                        │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Background Indexer                                     │    │
│  │  Status: idle                                           │    │
│  │  Indexed files: 12,450                                  │    │
│  │  Last run: 5 minutes ago                                │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Warnings                                               │    │
│  │  ⚠️  [cache] Cache directory > 5 GiB                    │    │
│  │  ⚠️  [disk] Free disk space < 10 GiB                    │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Sections

### Agent Status

Shows the health of the Rust sidecar agent:
- **Status indicator** — 🟢 OK, 🟡 Degraded, 🔴 Error
- **Agent version** — the sidecar binary version
- **CLI version** — installed `crab` CLI version (or "not installed")
- **Uptime** — how long the agent has been running

### Cache

Information about the local chunk cache:
- **Directory** — filesystem path to the cache
- **Used** — current cache size on disk
- **Free** — available disk space on the cache volume
- **Clear Cache** — button to purge all cached chunks

Clearing the cache is safe — chunks will be re-downloaded from cloud
storage on next access. It frees disk space at the cost of slower
first access to previously-cached files.

### Staging

The Crab staging area (`.crab/staging/`):
- **Size** — total size of staged chunks awaiting push
- **Clean Staging** — remove abandoned staging data

Cleaning staging removes chunks from failed or interrupted push
operations. Active staging data (from a recent `crab add`) is
preserved.

### Background Indexer

Status of the SlateDB-backed file indexer:
- **Status** — idle, indexing, or error
- **Indexed files** — total files in the index
- **Last run** — when the indexer last completed a pass

### Warnings

The agent reports warnings for degraded conditions:
- Cache directory exceeding size thresholds
- Low disk space
- Stale CLI version
- Missing git configuration
- Expired forge tokens

Each warning includes:
- Severity level (warning or critical)
- Category code (cache, disk, auth, config)
- Human-readable message

## Copy Diagnostics

The **Copy** button generates a comprehensive diagnostic report and
copies it to the clipboard. The report includes:

```
=== Crab Desktop Diagnostics ===
Timestamp: 2026-05-11T10:30:00Z

Agent version: 0.1.0
CLI version: crab 0.6.0
Cache dir: /Users/dev/.cache/crab
Cache used: 2.4 GiB
Cache free: 48.2 GiB
Staging size: 156 MiB
Last GC: 2026-05-10T22:00:00Z

Warnings:
  [warning] cache: Cache directory exceeds 2 GiB

=== Open Repositories ===
  /Users/dev/project (repo_id: abc123)

=== Recent Logs (last 500 lines) ===
  [2026-05-11T10:29:55Z] INFO list_refs completed in 12ms
  [2026-05-11T10:29:50Z] INFO git_status_porcelain completed in 45ms
  ...
```

This report is useful for:
- Filing bug reports
- Sharing with support
- Diagnosing performance issues

## LSP Health

When language servers are active, their status appears here:
- Server name and language
- Running/stopped/error state
- Memory usage (if reported)
- Restart button for error recovery

## Actions

| Action | Description |
|--------|-------------|
| Refresh | Re-fetch all health data |
| Copy Diagnostics | Copy full report to clipboard |
| Clear Cache | Purge chunk cache |
| Clean Staging | Remove abandoned staging data |
| Open Cache Dir | Reveal cache directory in Finder |
| Restart Agent | Force-restart the sidecar agent |

## When to Use

The Health view is useful when:
- Operations are slower than expected (check cache/disk)
- The agent seems unresponsive (check agent status)
- Disk space is running low (check cache size)
- Filing a bug report (copy diagnostics)
- After a crash (check warnings for root cause)
