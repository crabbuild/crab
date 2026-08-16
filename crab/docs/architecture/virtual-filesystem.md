# Virtual Filesystem

## Overview

The VFS module presents crab-tracked files with on-demand content
materialization through NFS or FUSE. Files appear at their full size in
directory listings but are fetched from the remote store only when actually
read.

Mounts are read-write by default. Local mutations are captured in a
copy-on-write overlay and become repository commits only after an explicit
`crab mount commit`. `--read-only` mounts expose the snapshot without an overlay
write path.

Source: `crates/crab-vfs/src/` (compiled with `--features nfs` and/or
`--features fuse`)

## Architecture

```
┌─────────────────────────────────────────────────────┐
│  User Process                                       │
│  cat /mnt/repo/model.safetensors                    │
│  editor /mnt/repo/generated.bin                     │
└──────────────┬──────────────────────────────────────┘
               │ read(), write(), rename() syscalls
               ▼
┌──────────────────────────────────────────────────────┐
│  Kernel NFS Client or FUSE Driver                    │
└──────────────┬───────────────────────────────────────┘
               │ NFSv3 RPCs or /dev/fuse messages
               ▼
┌──────────────────────────────────────────────────────┐
│  crab NFS/FUSE Handler (nfs.rs or fuse.rs)           │
│  ├── lookup(parent, name) → inode                    │
│  ├── getattr(inode) → stat                           │
│  ├── readdir(inode) → entries                        │
│  ├── read(inode, offset, size) → data                │
│  └── write/create/unlink/rename/... → overlay        │
│       │                                              │
│       ▼                                              │
│  VFS Engine (engine.rs)                              │
│  ├── Snapshot: point-in-time tree from git ref       │
│  ├── Resolver: path → resolved node + metadata       │
│  ├── Hydration: on-demand chunk download             │
│  └── Overlay: local write layer + publish/reset      │
└──────────────────────────────────────────────────────┘
```

## Components

### Snapshot (`snapshot.rs`)

A snapshot captures the directory tree at a specific git ref (branch, tag, or
SHA). It resolves the ref to a commit, walks the tree, and records path-indexed
file metadata (size, mode, pointer hash).

Snapshots are immutable once created. To see new commits, a new snapshot is
taken (see Refresh below). Snapshot publication streams the Git tree into one
SQLite transaction, and a parent/name index supports bounded directory pages;
mount startup and directory listing therefore do not require a second
whole-repository metadata copy in memory.

### Resolver (`resolver.rs`)

The resolver maps filesystem paths to snapshot or overlay nodes and provides
the metadata needed for NFS/FUSE operations (getattr, lookup, readdir). It
maintains:
- Current snapshot generation
- Overlay-first lookup precedence
- Directory children merged from snapshot and overlay entries

Directory merges use name-cursor pages from both snapshot and overlay stores.
NFS consumes those pages lazily as the client accepts entries, bounding memory
for directories with very large child counts.

### Hydration (`hydration.rs`)

When a `read()` call hits a pointer file, the hydration module:
1. Parses the pointer to extract file_hash and shard_hint
2. Resolves the shard (from cache or remote)
3. Determines which xorb chunks cover the requested byte range
4. Downloads and decompresses the required chunks
5. Returns the requested bytes to the protocol handler

Hydration is lazy and range-aware: only the chunks covering the requested
byte range are fetched, not the entire file.

### Overlay (`overlay.rs`)

The overlay module provides the write layer for read-write mounts. Local
modifications are stored in an overlay database plus an upper directory and are
merged with the immutable snapshot view. The overlay tracks creates, writes,
truncates, mode changes, symlinks, unlinks, and directory renames.

Overlay writes remain local until the user runs a publish operation:
- `crab mount diff` inspects the live overlay.
- `crab mount export` copies overlay changes to a normal directory.
- `crab mount commit` converts the overlay into a Git commit.
- `crab mount commit --push` commits and pushes the tracked ref.
- `crab mount reset --overlay --yes` discards overlay changes.

Read-only mounts configure the protocol handler as read-only and reject
mutating operations with the platform read-only filesystem error.

### NFS Handler (`nfs.rs`)

The NFS handler exposes the same resolver, hydration, and overlay engine
through an in-process NFSv3 server bound to loopback. The native OS NFS client
mounts that local export:

| Operation | Description |
|-----------|-------------|
| `lookup` / `getattr` | Resolve paths and return stable NFS file attributes |
| `readdirplus` | List directories with attributes and handles; enqueue file-child prefetch |
| `read` | Read file data at offset (triggers hydration for pointers) |
| `write` | Write file data into the overlay; track unstable writes until `commit` or shutdown |
| `create` / `mkdir` / `symlink` | Create overlay entries |
| `setattr` | Apply overlay truncate, mode, and mtime changes |
| `remove` / `rename` | Mutate overlay entries and update NFS file-handle mappings |
| `commit` | Flush pending overlay data to stable local storage |

NFS is the default backend because it uses the built-in OS client on macOS and
Windows and the standard `mount.nfs` client on Linux. It runs in a
`crab-nfs-mount` helper for background mounts.

macOS and Linux mounts use a random loopback port and pass that port to the
native mount command. Windows mounts use a generated 127.88.x.x loopback
address on the standard portmapper port, because Client for NFS supports drive
targets but does not document per-mount NFS port options.

NFS mount commands use local client-side locking or disable remote locking
because the embedded NFS server does not provide an NLM service. Locks are
therefore advisory within one mounted client, not cross-client coordination.

The long-term performance and correctness target for this backend is documented
in [NFS Mount Architecture](nfs-mount-architecture.md).

### FUSE Handler (`fuse.rs`)

The FUSE handler implements the low-level FUSE operations:

| Operation | Description |
|-----------|-------------|
| `lookup` | Resolve a name within a directory → inode |
| `getattr` | Return file/directory attributes (size, mode, times) |
| `readdir` | List directory contents |
| `read` | Read file data at offset (triggers hydration for pointers) |
| `open` | Open a file handle |
| `write` | Write file data into the overlay |
| `create` | Create a regular overlay file |
| `setattr` | Apply overlay truncate and mode changes |
| `unlink` / `rmdir` | Record overlay removals and whiteouts |
| `mkdir` | Create an overlay directory |
| `readlink` / `symlink` | Read or create symlink entries |
| `rename` | Move overlay entries or record base-tree renames |
| `release` | Close a file handle |

Both handlers enqueue regular file children for speculative hydration when a
directory is listed. The prefetch path is best-effort and never changes visible
filesystem behavior; it only warms chunk reads for the common list-then-open
workflow.

NFSv3 has no close/flush RPC. Crab tracks paths that accepted unstable NFS
writes, clears them when a client sends `COMMIT`, carries them across
rename/remove operations, and drains the remaining set before the
`crab-nfs-mount` helper exits.

### Mount Lifecycle (`mount.rs`, `nfs_mount.rs`)

```
crab mount /mnt/repo
  1. Select backend (`auto` prefers NFS)
  2. Resolve the target git ref (default: HEAD)
  3. Build a snapshot of the tree
  4. Open the overlay store unless the mount is read-only
  5. Start the NFS loopback server or create the FUSE session
  6. Mount through the OS NFS client or FUSE kernel driver
  7. Enter the mount event loop (foreground or background helper/coordinator)

crab unmount --mountpoint /mnt/repo
  1. Normalize the mount target (`Z:` drive targets stay drive targets on Windows)
  2. Look up the helper/coordinator PID in the mount registry
  3. Ask the platform client to unmount (`umount`, `fusermount`, or Windows `umount.exe`)
  4. Wait for the helper to observe unmount and drain dirty NFS writes
  5. Force-unmount or terminate the helper if graceful shutdown does not finish
  6. Clean the registry entry and legacy PID file
```

### Refresh (`refresh.rs`)

For long-running mounts, the refresh module periodically checks the remote
for new commits on the tracked ref. When a new commit is detected:
1. Take a new snapshot at the updated ref
2. Atomically swap the active snapshot
3. Invalidate cached inodes for changed paths

The refresh interval is configurable per-repo via the daemon.

## Overlay Publish and Large Files

`crab mount commit` freezes the overlay while it snapshots pending changes and
checks that the mounted base ref has not moved. It then builds a detached
publish worktree, stages normal Git changes, converts crab-tracked overlay files
into pointer files, and updates the tracked ref after the commit succeeds.

For large crab-tracked files, publish avoids copying the file through the
temporary worktree when `.gitattributes` is unchanged. It streams the overlay
backing file directly into the Crab staging pipeline, writes the resulting
pointer into the Git index, and uploads chunked xorb data through the same
streaming path used by `crab add`. If `.gitattributes` changed in the overlay,
publish falls back to applying the worktree first so Git attributes are resolved
against the new tree.

With `--push`, the detached publish worktree pushes the new commit to origin
before the local ref is moved. If the push fails, the overlay and transaction
record remain retryable. If a previous commit/push succeeded but cleanup was
interrupted, the next commit can finalize cleanup only when the recorded commit,
ref name, and overlay fingerprint still match the live overlay.

## Daemon Mode (`daemon.rs`)

The daemon manages multiple repository mounts with a shared cache and
hydration pool:

```
Daemon
├── Registry: registered repos with mount config
├── Mount Pool: active mounts
├── Hydration Pool: shared worker tasks for chunk downloads
├── Cache: shared local cache across all mounts
└── Refresh: per-repo periodic ref check
```

### Daemon Actions

| Action | Description |
|--------|-------------|
| `add-repo` | Register a repo with name, remote URL, branch, mount root |
| `remove-repo` | Deregister and unmount a repo |
| `list` | List all registered repos |
| `status` | Report per-repo mount state and hydration progress |
| `set-refresh` | Tune the refresh interval for a repo |

### Daemon State

State is persisted in `~/.crab/daemon/` (or `--root`):
- `repos.json`: registered repository configurations
- `{name}/mount.pid`: PID file per active mount
- `{name}/state.json`: mount state and last-seen ref

## Platform Support

| Platform | NFS Backend | FUSE Backend |
|----------|-------------|--------------|
| macOS | `mount_nfs`, supported by default | macFUSE, optional |
| Linux | `mount.nfs` from `nfs-common`/`nfs-utils`; host mount policy applies | fuse3, optional |
| Windows | Client for NFS with drive targets such as `Z:` | Not supported |

The NFS and FUSE features are gated behind `--features nfs` and
`--features fuse` at compile time. Without the requested backend, mount
commands print a backend-specific configuration error.
