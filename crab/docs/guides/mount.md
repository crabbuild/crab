# Virtual Filesystem Mount Guide

Mount a crab repository as a virtual filesystem for instant, on-demand file
access -- no full clone required. Crab uses the native NFS backend by default
when it is compiled in, because it works through the operating system NFS
client and does not require macFUSE. Use `--backend=fuse` when you specifically
want the FUSE implementation.

Mounts are read-write by default; use `--read-only` when you only want to
browse a snapshot.

## Quick Start

```bash
crab mount --repo crab://bucket/ml-models --mountpoint /mnt/models
```

Files are immediately accessible at `/mnt/models/`. Content is fetched from
object storage only when you actually read a file.

```bash
ls /mnt/models/
cat /mnt/models/config.json
```

Unmount when done:

```bash
crab unmount --mountpoint /mnt/models
```

Select a backend explicitly when needed:

```bash
crab mount --repo crab://bucket/repo --mountpoint /mnt/view --backend=nfs
crab mount --repo crab://bucket/repo --mountpoint /mnt/view --backend=fuse
```

## Remote Mount

A remote mount creates a blobless clone of the repository (trees and commits
only, no file content) and presents it as a local filesystem. File content is
hydrated on demand from object storage.

### How It Works

1. The CLI selects a backend. `auto` prefers NFS and falls back to FUSE only
   when NFS is not compiled in.
2. Crab checks the cache at `~/.crab/mounts/repos/<hash>/` for an
   existing blobless clone. If none exists, the current development-line
   implementation runs `git clone --bare --filter=blob:none` against a
   proof-gated v2 remote to bootstrap the tree structure. This path has green
   RustFS qualification but is not a released support claim until provider and
   released-artifact qualification complete; missing proof fails instead of
   downloading a complete pack as a substitute.
3. A snapshot is built from the target ref's tree.
4. NFS starts a loopback NFSv3 server in `crab-nfs-mount` and mounts it through
   the OS NFS client. FUSE starts the FUSE session through `crab-fuse-mount`.
5. File reads trigger on-demand chunk download and reconstruction.
6. File writes land in the local overlay until you publish or discard them.
7. When refresh is enabled, a refresh loop polls the remote every 30 seconds for
   branch updates.

### Examples

```bash
# Mount the default branch
crab mount --repo crab://bucket/ml-models --mountpoint /mnt/models

# Mount a specific branch
crab mount --repo crab://bucket/ml-models --mountpoint /mnt/models --ref=experiment-v2

# Mount from S3 directly
crab mount --repo s3://my-bucket/datasets --mountpoint /mnt/data

# Mount read-only (no overlay, no writes)
crab mount --repo crab://bucket/repo --mountpoint /mnt/browse --read-only

# Disable automatic refresh (static snapshot, useful for CI)
crab mount --repo crab://bucket/repo --mountpoint /mnt/ci --no-refresh
```

## Local Mount

A local mount uses an existing `.git` directory on disk — no clone, no network
required for tree browsing. This is useful for viewing another branch without
switching your working tree.

### How It Works

1. The CLI validates that the local path contains a `.git` directory.
2. The snapshot is built directly from the local object database.
3. For git-native files, reads are served from the local ODB (instant).
4. For pointer-tracked files, hydration still requires access to object
   storage (the crab remote must be configured in the repo).

### Examples

```bash
# View a feature branch without checking it out
crab mount --repo /home/user/my-repo --mountpoint /tmp/feature-view --ref=feature-branch

# Browse main while working on a different branch
crab mount --repo . --mountpoint /tmp/main-view --ref=main

# Mount a bare repo
crab mount --repo /srv/git/project.git --mountpoint /tmp/browse
```

## Multiple Mounts

Run `crab mount` multiple times to mount different repos simultaneously. All
mounts use the same Crab mount cache layout. NFS mounts run one helper process
per mount; FUSE mounts use the coordinator path when backgrounded.

```bash
crab mount --repo crab://bucket/repo-a --mountpoint /mnt/a
crab mount --repo crab://bucket/repo-b --mountpoint /mnt/b
crab mount --repo /home/user/local-repo --mountpoint /mnt/c --ref=main
```

Only one mount can be active for a given repo cache at a time. Use
`crab mount switch` to change branches, or unmount before mounting another view
of the same repo.

No manual daemon management is required. Background NFS mounts start
`crab-nfs-mount`; background FUSE mounts start or contact the coordinator.

## Background vs Foreground

### Background (default)

By default, `crab mount` returns immediately after the mount is established.
NFS runs in a `crab-nfs-mount` helper process. FUSE runs inside the coordinator
process in the background.

```bash
crab mount --repo crab://bucket/repo --mountpoint /mnt/view
# Returns immediately — mount is active in the background
echo "Mount is ready"
```

### Foreground

With `--foreground`, the CLI itself becomes the mount process. It blocks until
you press Ctrl+C. Useful for debugging, one-off browsing, or when you don't
want a persistent coordinator.

```bash
crab mount --repo crab://bucket/repo --mountpoint /mnt/view --foreground
# Blocks here until Ctrl+C
```

Foreground mounts block the current terminal until Ctrl+C. This is useful for
debugging the selected backend.

## Read-Only vs Read-Write

Read-write is the default mode. It lets normal tools create, edit, rename, and
delete files under the mountpoint. Those changes are private to the mount until
you run `crab mount commit`; they are not pushed to Crab automatically.
Crab allows one active mount per repo cache so two mountpoints cannot race on
the same cached Git checkout, snapshot database, or overlay.

Read-only mode is for browsing, CI checks, or any workflow that must not create
local overlay state:

```bash
crab mount --repo crab://bucket/repo --mountpoint /mnt/browse --read-only
```

Mutating filesystem calls in read-only mode fail with the platform read-only
filesystem error.

## Mountpoint Safety

By default, `crab mount` refuses to mount inside an existing git or crab
working tree. This prevents `git status` from seeing thousands of virtual
files as untracked content.

```
$ crab mount --repo crab://bucket/repo --mountpoint ./vfs
error: mountpoint is inside a git repository. Mount outside the repo to
avoid git seeing virtual files as untracked.
```

### Overriding with `--allow-nested`

If you understand the implications (e.g., you've added the path to
`.gitignore`), use `--allow-nested`:

```bash
# Add to .gitignore first
echo "vfs/" >> .gitignore

# Then mount with override
crab mount --repo crab://bucket/data --mountpoint ./vfs --allow-nested
```

## Cache Management

Mount caches (blobless clones, snapshots, overlays) are stored under
`~/.crab/mounts/repos/`. Over time these can accumulate disk usage.

### Clean inactive caches

Remove cached data for mounts that are not currently active:

```bash
crab mount clean
```

Reports how much disk space was freed.

### Clean all caches

Remove everything (requires no active mounts):

```bash
crab mount clean --all
```

## Managing Mounts

### List active mounts

```bash
crab mount list
```

Displays a table:

```
NAME        MOUNTPOINT      SOURCE                  REF     STATE    PID    UPTIME
ml-models   /mnt/models     crab://bucket/ml-mod…   main    running  12345  2h 15m
local-view  /tmp/view       /home/user/my-repo      dev     running  12345  10m
old-mount   /mnt/stale      crab://bucket/old       main    stale    —      —
```

Stale entries (PID no longer running) are marked and can be cleaned up with
`crab unmount`.

For programmatic use:

```bash
crab mount list --json
```

### Check mount status

```bash
crab mount status --mountpoint /mnt/models
```

Reports: HEAD OID, tracked ref, mode (rw/ro), hydration queue depth, cache
usage, overlay dirty count, last refresh timestamp.

```bash
crab mount status --json    # structured output
crab mount status --live-only --json
```

Regular status prefers live backend control data and falls back to persisted
mount metadata for stale-mount diagnosis. Use `--live-only` in health checks
when a missing NFS helper control response or FUSE coordinator response should
fail the command.

### Force refresh

Trigger an immediate fetch + snapshot rebuild without waiting for the 30s timer:

```bash
crab mount refresh --mountpoint /mnt/models
```

### Switch branch

Switch a mount to track a different branch:

```bash
crab mount switch --mountpoint /mnt/models --ref=experiment-v3
```

The snapshot is rebuilt from the new branch's HEAD. Overlay state is reconciled
(stale entries discarded).

### Inspect and publish overlay writes

Writable mounts store changes in a copy-on-write overlay until you explicitly
publish or discard them.

```bash
# Show local overlay mutations and estimated upload size
crab mount diff --mountpoint /mnt/models

# Export overlay files to a normal directory for review
crab mount export --mountpoint /mnt/models --to /tmp/models-overlay

# Commit overlay changes into the mounted repo's tracked ref
crab mount commit --mountpoint /mnt/models -m "Update generated artifacts"

# Commit and push to origin
crab mount commit --mountpoint /mnt/models -m "Update generated artifacts" --push

# Discard overlay changes
crab mount reset --mountpoint /mnt/models --overlay --yes
```

`crab mount commit` freezes overlay writes while it snapshots the overlay,
checks that the mounted base ref has not moved, creates a Git commit, refreshes
the mounted snapshot, and clears the overlay after a successful local commit.

For large crab-tracked files, commit streams overlay file content directly into
Crab staging when `.gitattributes` is stable, then writes pointer files into the
Git index. If `.gitattributes` changed in the overlay, commit first materializes
the publish worktree so Git attributes are resolved against the new tree.

With `--push`, Crab pushes from the detached publish worktree before moving the
local ref. A failed push leaves the overlay and transaction state retryable with
the local commit OID recorded, and a later retry can clean up an already-pushed
transaction only when the recorded commit, ref, and overlay fingerprint still
match.

### Unmount

```bash
# Unmount a specific mount
crab unmount --mountpoint /mnt/models

# Unmount all active mounts
crab unmount --all
```

## Limitations

- **Overlay writes are explicit-publish.** Files written through a mount
  stay local until you run `crab mount commit` or `crab daemon commit`. Writes
  are never transparently pushed back while applications write files.

- **Network required for pointer file hydration (remote mounts).** The first
  read of a pointer-tracked file downloads chunks from object storage. If the
  network is unavailable, uncached reads return EIO. Already-cached files
  continue to work.

- **First-access latency.** Reading a file for the first time incurs download
  latency. Subsequent reads are served from the local chunk cache.

- **NFS uses the OS NFS client.** macOS ships the client. Linux needs
  `mount.nfs` from `nfs-common` or `nfs-utils`, and the kernel mount call may
  require root, passwordless sudo, or container `CAP_SYS_ADMIN` depending on
  host policy. Windows needs Client for NFS and a drive target such as `Z:`;
  Crab serves each Windows mount from a unique loopback IP on the standard NFS
  portmapper port because the Windows client does not expose per-mount NFS port
  options.

- **NFS locks are local to the client mount.** The NFS backend uses
  client-side/no-remote-lock-manager options because Crab serves one local
  loopback export per mount and does not run an NLM service. Advisory locks
  coordinate processes on the same mounted client, not separate clients.

- **FUSE is optional.** `--backend=fuse` requires macFUSE on macOS or fuse3 on
  Linux. Windows FUSE is not supported.

## Troubleshooting

### Stale mounts

If a mount process crashed or was killed without clean unmount, the mountpoint
may be in a stale state (operations hang or return "Transport endpoint is not
connected").

```bash
# Clean up all stale mounts
crab unmount --all

# If that doesn't work, use OS tools directly
# Linux:
fusermount3 -u /mnt/stale-mount

# macOS:
umount -f /mnt/stale-mount
```

### NFS client not installed or not allowed

If the default NFS backend cannot run the OS mount command, Crab prints the
command failure and the missing prerequisite.

**Linux (Ubuntu/Debian):**

```bash
sudo apt-get install nfs-common
```

**Linux (Fedora/RHEL):**

```bash
sudo dnf install nfs-utils
```

In locked-down containers, the Linux mount syscall can be denied even when
`mount.nfs` is installed. Run the container with `CAP_SYS_ADMIN` or use a host
policy that permits the NFS mount syscall.

**macOS:**

macOS includes the NFS client. Run the native smoke from `crab/` to verify the
default NFS backend on the current host:

```bash
make mount-nfs-macos-smoke
```

The smoke builds Crab with only the NFS backend, mounts a local Git repository,
verifies read/write/rename/remove behavior and synthetic `.git` protections,
then unmounts, remounts, and verifies the local overlay state persisted.

For native large-file and object-storage qualification, run:

```bash
make mount-large-macos-nfs-rustfs-smoke
```

This starts an isolated RustFS backend, verifies chunk dedup for identical
large files, range and whole-file reads, writable large-file commit/push and a
fresh byte-identical clone, and enumerates a 10,000-entry directory through the
native NFS client. Evidence is retained under the configured
`CRAB_MOUNT_MACOS_ROOT` run directory.

**Windows:**

Install Client for NFS, then mount to a drive target:

```powershell
crab mount --repo crab://bucket/repo --mountpoint Z:
```

Crab uses a loopback-only NFS server for Windows mounts. If another local NFS
server or portmapper already owns the standard port on all loopback addresses,
stop that service or use macOS/Linux for NFS mounts until the conflict is gone.

Run the native Windows smoke from `crab/` after installing Client for NFS:

```powershell
powershell.exe -ExecutionPolicy Bypass -File scripts/run-mount-nfs-windows-smoke.ps1 -Drive Z:
```

Set `CRAB_NFS_SMOKE_DRIVE` to an unused drive letter if `Z:` is already
assigned. The smoke builds Crab with only the NFS backend, mounts a local Git
repo through Windows Client for NFS, verifies read/write/rename/remove behavior
and synthetic `.git` protections, then unmounts the drive.

### FUSE not installed

```
error: FUSE prerequisites not met.
```

**macOS:**

```bash
brew install --cask macfuse
# Approve macFUSE in System Settings and reboot if prompted.
```

The rest of the Crab CLI works without macFUSE installed. The default NFS
backend also does not require macFUSE. Only `crab mount --backend=fuse` needs
macFUSE.

**Linux (Ubuntu/Debian):**

```bash
sudo apt-get install fuse3 libfuse3-dev
```

**Linux (Fedora/RHEL):**

```bash
sudo dnf install fuse3 fuse3-devel
```

Verify FUSE is available:

```bash
ls /dev/fuse    # should exist
```

### Network errors

If mount fails during the blobless clone step:

1. **Check credentials:** Ensure AWS/GCP/Azure credentials are configured
   in your environment (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, etc.)
   or via `crab config`.

2. **Check connectivity:** Verify you can reach the object storage endpoint.
   ```bash
   crab doctor
   ```

3. **Check the remote URL:** Ensure the `--repo` URL is correct and the
   bucket/prefix exists.

4. **Retry:** Transient network errors are common. The mount command is
   safe to retry — it will resume from the cached clone if one exists.

### Permission denied on mountpoint

Ensure the mountpoint directory is owned by your user and has write
permissions. On Linux FUSE mounts cannot be created at paths you don't own;
NFS mounts may also be blocked by the host's mount policy.

```bash
mkdir -p /mnt/models
# If /mnt requires root:
sudo mkdir -p /mnt/models && sudo chown $USER /mnt/models
```

## CLI Reference

| Command | Description |
|---------|-------------|
| `crab mount --repo <source> --mountpoint <path> [flags]` | Mount a repo |
| `crab unmount --mountpoint <path>` | Unmount |
| `crab unmount --all` | Unmount all |
| `crab mount list [--json]` | List active mounts |
| `crab mount status [--mountpoint <path>] [--live-only] [--json]` | Mount health/stats |
| `crab mount diff [--mountpoint <path>] [--json]` | Show overlay mutations |
| `crab mount export --mountpoint <path> --to <dir> [--json]` | Export overlay files |
| `crab mount commit --mountpoint <path> -m <message> [--push] [--json]` | Commit overlay changes |
| `crab mount reset --mountpoint <path> --overlay --yes [--json]` | Discard overlay changes |
| `crab mount refresh --mountpoint <path>` | Force refresh |
| `crab mount switch --mountpoint <path> --ref=<branch>` | Switch branch |
| `crab mount clean [--all]` | Clean inactive caches |

### Mount flags

| Flag | Default | Description |
|------|---------|-------------|
| `--repo` / `-r` | (required) | Source: remote URL or local path |
| `--mountpoint` / `-m` | (required) | Local mount path, or Windows NFS drive target such as `Z:` |
| `--ref` | HEAD | Branch or tag to mount |
| `--backend` | `auto` | `auto`, `nfs`, or `fuse`; `auto` prefers NFS |
| `--read-only` | false | Disable overlay (no writes) |
| `--foreground` | false | Block instead of backgrounding |
| `--no-refresh` | false | Disable automatic remote polling |
| `--allow-nested` | false | Allow mounting inside a git repo |
| `--name` | (derived) | Human-friendly name for this mount |

## Related Commands

- [`crab hydrate`](hydrate.md) — materialize files to disk without a mount
- [`crab clone`](clone.md) — clone with lazy checkout
- [`crab daemon`](daemon.md) — persistent named multi-repo daemon
