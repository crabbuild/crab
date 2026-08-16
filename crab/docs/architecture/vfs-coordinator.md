# VFS Coordinator (Auto-Daemon)

## Overview

The VFS coordinator is a background process that manages multiple FUSE mounts
with shared resources. It replaces the explicit `crab daemon` workflow with an
invisible, auto-managed process that starts on the first `crab mount` and exits
when the last mount is unmounted.

Source: `crates/crab-vfs/src/coordinator.rs`

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│  CLI Process (crab mount --repo ... --mountpoint ...)               │
│  1. Try connect to ~/.crab/mounts/daemon.sock                       │
│  2. If refused → spawn coordinator, retry with backoff              │
│  3. Send JSON mount request                                         │
│  4. Wait for response, print result, exit                           │
└────────────────────────────┬────────────────────────────────────────┘
                             │ Unix socket
                             ▼
┌─────────────────────────────────────────────────────────────────────┐
│  Coordinator Process (single instance per user)                     │
│                                                                     │
│  ┌───────────────────────────────────────────────────────────────┐  │
│  │  IPC Server (daemon.sock)                                     │  │
│  │  • Accepts connections, reads newline-delimited JSON           │  │
│  │  • Dispatches: mount, unmount, list, status, refresh, switch  │  │
│  │  • Returns JSON responses                                     │  │
│  └───────────────────────────────────────────────────────────────┘  │
│                                                                     │
│  ┌─────────────────────┐  ┌──────────────────────────────────────┐  │
│  │  Shared Resources   │  │  Mount Registry                      │  │
│  │                     │  │                                      │  │
│  │  • ChunkCache       │  │  Mount A: /mnt/models (running)      │  │
│  │    (LRU, bounded)   │  │  Mount B: /mnt/code   (running)      │  │
│  │                     │  │  Mount C: /tmp/browse  (running)      │  │
│  │  • HydrationService │  │                                      │  │
│  │    (worker pool)    │  │  ref_count = 3                       │  │
│  └─────────────────────┘  └──────────────────────────────────────┘  │
│                                                                     │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐              │
│  │  Mount A     │  │  Mount B     │  │  Mount C     │              │
│  │  Snapshot    │  │  Snapshot    │  │  Snapshot    │              │
│  │  Overlay     │  │  Overlay     │  │  Overlay     │              │
│  │  Resolver    │  │  Resolver    │  │  Resolver    │              │
│  │  Engine      │  │  Engine      │  │  Engine      │              │
│  │  RefreshLoop │  │  RefreshLoop │  │  RefreshLoop │              │
│  │  FUSE Session│  │  FUSE Session│  │  FUSE Session│              │
│  └──────────────┘  └──────────────┘  └──────────────┘              │
└─────────────────────────────────────────────────────────────────────┘
```

## IPC Protocol

Communication uses JSON-over-Unix-socket with newline-delimited messages.
Each request is a single JSON object terminated by `\n`. Each response is a
single JSON object terminated by `\n`.

Socket path: `~/.crab/mounts/daemon.sock`

### Request Schemas

#### Mount

```json
{
  "op": "mount",
  "remote": "crab://bucket/repo",
  "mountpoint": "/mnt/view",
  "ref": "main",
  "read_only": false,
  "no_refresh": false,
  "name": "my-mount"
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `op` | string | yes | Always `"mount"` |
| `remote` | string | yes | Source URL or local path |
| `mountpoint` | string | yes | Absolute path for FUSE mount |
| `ref` | string | no | Branch/tag (default: HEAD) |
| `read_only` | bool | no | Disable overlay (default: false) |
| `no_refresh` | bool | no | Disable polling (default: false) |
| `name` | string | no | Human-friendly name (default: derived) |

#### Unmount

```json
{
  "op": "unmount",
  "mountpoint": "/mnt/view"
}
```

#### List

```json
{
  "op": "list"
}
```

#### Status

```json
{
  "op": "status",
  "mountpoint": "/mnt/view"
}
```

#### Refresh

```json
{
  "op": "refresh",
  "mountpoint": "/mnt/view"
}
```

#### Switch Ref

```json
{
  "op": "switch_ref",
  "mountpoint": "/mnt/view",
  "ref": "feature-branch"
}
```

### Response Schemas

#### Success (mount)

```json
{
  "ok": true,
  "mountpoint": "/mnt/view",
  "pid": 12345,
  "head_oid": "abc123def456...",
  "ref": "main",
  "mode": "rw"
}
```

#### Success (unmount)

```json
{
  "ok": true,
  "mountpoint": "/mnt/view"
}
```

#### Success (list)

```json
{
  "ok": true,
  "mounts": [
    {
      "name": "ml-models",
      "mountpoint": "/mnt/models",
      "source": "crab://bucket/ml-models",
      "ref": "main",
      "head_oid": "abc123...",
      "mode": "rw",
      "state": "running",
      "start_time": "2024-01-15T10:30:00Z"
    }
  ]
}
```

#### Success (status)

```json
{
  "ok": true,
  "mountpoint": "/mnt/models",
  "head_oid": "abc123def456...",
  "ref": "main",
  "mode": "rw",
  "hydration_queue_depth": 3,
  "cache_usage_bytes": 104857600,
  "overlay_dirty_count": 2,
  "last_refresh": "2024-01-15T12:00:30Z"
}
```

#### Success (refresh)

```json
{
  "ok": true,
  "mountpoint": "/mnt/models",
  "head_oid": "def789...",
  "changed": true
}
```

#### Success (switch_ref)

```json
{
  "ok": true,
  "mountpoint": "/mnt/models",
  "ref": "feature-branch",
  "head_oid": "789abc..."
}
```

#### Error

```json
{
  "ok": false,
  "error": "mountpoint /mnt/view is not an active mount"
}
```

### Connection Handling

- Idle connections are closed after 30 seconds.
- Multiple CLI processes can connect simultaneously; requests are serialized
  internally via a command channel.
- The coordinator never initiates messages — it only responds to requests.

## Shared Resource Model

### ChunkCache

A bounded, LRU-evicting cache for downloaded chunks. Shared across all mounts
so that overlapping repositories (or different branches of the same repo)
benefit from each other's downloads.

```
ChunkCache
├── Storage: content-addressed on disk (~/.crab/mounts/cache/chunks/)
├── Index: in-memory LRU map (chunk_hash → file offset)
├── Capacity: configurable (default: 10 GB)
└── Eviction: LRU when capacity exceeded
```

Key properties:
- Content-addressed: same chunk from different mounts is stored once.
- Bounded: won't fill the disk. Oldest unused chunks are evicted first.
- Shared: a chunk downloaded for Mount A is immediately available to Mount B.

### HydrationService

A shared worker pool that downloads and reconstructs file content on demand.
All mounts submit hydration requests to the same pool, which manages
concurrency and prioritization.

```
HydrationService
├── Worker Pool: N async tasks (default: 8)
├── Priority Queue: pending hydration requests
│   ├── Priority 0: active read() calls (user is waiting)
│   ├── Priority 1: readahead / prefetch
│   └── Priority 2: background pre-warming
├── Rate Limiter: per-remote request throttling
└── Retry Logic: exponential backoff for transient failures
```

Key properties:
- Unified queue prevents thundering herd across mounts.
- Priority ensures interactive reads are served before background work.
- Worker count is configurable via `CRAB_HYDRATION_WORKERS` env var.

### Per-Mount Resources

Each mount owns its own isolated state:

| Resource | Description |
|----------|-------------|
| Snapshot | Point-in-time tree from the tracked ref |
| Overlay | SQLite DB + upper directory for local writes |
| Resolver | Inode table merging snapshot + overlay |
| Engine | Wires resolver, overlay, hydration, ODB reader |
| RefreshLoop | Periodic fetch + snapshot rebuild |
| FUSE Session | Kernel FUSE registration for this mountpoint |

## Lifecycle

### Auto-Start

```
CLI: crab mount --repo crab://bucket/repo --mountpoint /mnt/view
  │
  ├─ Try connect to ~/.crab/mounts/daemon.sock
  │   └─ Connection refused (no coordinator running)
  │
  ├─ Acquire ~/.crab/mounts/daemon.lock (advisory flock)
  │   └─ Prevents race if multiple `crab mount` run simultaneously
  │
  ├─ Spawn coordinator as background process
  │   └─ Coordinator: bind socket, write daemon.pid, release lock
  │
  ├─ Retry connect with exponential backoff (up to 5s)
  │   └─ Connected
  │
  ├─ Send mount request
  │
  └─ Receive response, print confirmation, exit
```

### Ref-Counting

The coordinator maintains an active mount count:

```
mount request  → ref_count += 1
unmount request → ref_count -= 1
                  if ref_count == 0 → initiate shutdown
```

### Auto-Stop

When the last mount is unmounted (`ref_count` reaches 0):

1. Close the IPC socket (stop accepting new connections).
2. Unmount any remaining FUSE sessions (shouldn't be any, but defensive).
3. Flush and close the ChunkCache.
4. Remove `daemon.sock`, `daemon.pid`, `daemon.lock`.
5. Exit process.

The coordinator also shuts down on SIGTERM (sent by `crab unmount --all` or
system shutdown).

### Crash Recovery

If the coordinator crashes:
- Active mounts become stale (FUSE operations return EIO or hang).
- `crab mount list` detects stale entries (PID not running).
- `crab unmount --all` force-unmounts via OS tools and cleans up state files.
- The next `crab mount` starts a fresh coordinator.

## Cache Layout

```
~/.crab/mounts/
├── daemon.sock              # Unix domain socket for IPC
├── daemon.lock              # Advisory flock (prevents duplicate coordinators)
├── daemon.pid               # Coordinator process ID
├── mounts.json              # Active mount registry
│                            #   [{mountpoint, source, ref, pid, start_time, ...}]
├── cache/
│   └── chunks/              # Shared chunk cache (content-addressed)
│       ├── ab/              # First 2 hex chars of chunk hash
│       │   ├── ab3f...      # Chunk data files
│       │   └── ab91...
│       └── cd/
│           └── cd12...
└── repos/
    ├── a1b2c3d4e5f6/        # Hash of remote URL (first 12 hex of SHA-256)
    │   ├── .git/            # Bare blobless clone (trees + commits only)
    │   ├── snapshot.sqlite    # Current snapshot state
    │   ├── overlay.db       # SQLite overlay metadata
    │   └── overlay/
    │       └── upper/       # Overlay write layer (modified files)
    └── f7e8d9c0b1a2/        # Another remote
        ├── .git/
        ├── snapshot.sqlite
        ├── overlay.db
        └── overlay/
            └── upper/
```

### Directory naming

The `<hash>` for each repo directory is computed as:

```
hash = SHA-256(normalized_url)[0..12]  (first 12 hex characters)
```

Where `normalized_url` is the source URL with trailing slashes removed and
scheme lowercased. This ensures:
- Same remote → same cache directory (branch switches reuse the clone).
- Different remotes → isolated state.
- No path-length issues from long URLs.

For local sources, the hash is computed from the absolute path of the
repository.

### mounts.json schema

```json
[
  {
    "name": "ml-models",
    "mountpoint": "/mnt/models",
    "source": "crab://bucket/ml-models",
    "ref": "main",
    "pid": 12345,
    "start_time": "2024-01-15T10:30:00Z",
    "read_only": false,
    "cache_dir": "~/.crab/mounts/repos/a1b2c3d4e5f6"
  }
]
```

## Configuration

The coordinator respects these environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `CRAB_MOUNT_CACHE_DIR` | `~/.crab/mounts/` | Root directory for all mount state |
| `CRAB_HYDRATION_WORKERS` | `8` | Number of concurrent hydration workers |
| `CRAB_CHUNK_CACHE_SIZE` | `10737418240` (10 GB) | Maximum chunk cache size in bytes |
| `CRAB_REFRESH_INTERVAL` | `30` | Seconds between remote polling |

## Cross-References

- Guide: [`crab/docs/guides/mount.md`](../guides/mount.md)
- Architecture: [`crab/docs/architecture/virtual-filesystem.md`](virtual-filesystem.md)
- Source: `crates/crab-vfs/src/coordinator.rs`, `crates/crab-vfs/src/ipc.rs`
- Design context: this document and `crab/docs/architecture/virtual-filesystem.md`.
