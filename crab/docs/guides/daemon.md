# crab daemon

Multi-repo daemon: shared cache, hydration pool, multiple mounts.

## Synopsis

```
crab daemon [OPTIONS]
crab daemon add-repo --name <NAME> --remote <URL> --mount-root <PATH> [--backend <BACKEND>]
crab daemon remove-repo --name <NAME>
crab daemon list
crab daemon status --name <NAME>
crab daemon set-refresh --name <NAME> --interval <SECONDS>
crab daemon commit --name <NAME> -m <MESSAGE> [--push]
```

## Description

`crab daemon` runs a long-lived service that manages multiple Crab repository
mounts with a shared cache and hydration pool. Each registered repository can
use the FUSE or native NFS backend. It is a supported operating mode, not a
legacy replacement for `crab mount`. Use it when repos should be registered by
name and reconciled by a persistent service.

For ad-hoc mounts, branch views, and most interactive workflows, use
`crab mount`. For persistent named multi-repo workflows, use `crab daemon`.

The daemon automatically refreshes mounts when the remote ref is updated.
An unpushed commit created by `crab daemon commit` remains the mounted HEAD and
is never replaced by the refresh loop.

## Daemon Options

| Option | Default | Description |
|--------|---------|-------------|
| `--root` | `~/.crab/daemon` | Root directory for daemon state |
| `--hydration-concurrency` | `4` | Number of hydration worker tasks |

## Subcommands

### crab daemon (no subcommand)

Start the daemon in the foreground. It runs until interrupted (Ctrl+C).

```bash
crab daemon
```

### crab daemon add-repo

Register a repository for the daemon to mount.

| Option | Required | Default | Description |
|--------|----------|---------|-------------|
| `--name` | Yes | | Unique name for this repo |
| `--remote` | Yes | | Remote URL to clone from |
| `--branch` | No | `main` | Branch to track |
| `--mount-root` | Yes | | Root directory for the mount (`{mount-root}/{name}/`) |
| `--backend` | No | `fuse` | Filesystem backend: `fuse` or `nfs` |

### crab daemon remove-repo

Deregister and unmount a repository.

| Option | Required | Description |
|--------|----------|-------------|
| `--name` | Yes | Name of the repo to remove |

### crab daemon list

List all registered repositories.

### crab daemon status

Report per-repo status.

| Option | Required | Description |
|--------|----------|-------------|
| `--name` | Yes | Name of the repo to query |

Status reports whether an NFS daemon control plane is live and lists every
dirty overlay path, so it can be used before committing or stopping a daemon.

### crab daemon set-refresh

Tune the refresh interval for a repo.

| Option | Required | Description |
|--------|----------|-------------|
| `--name` | Yes | Name of the repo |
| `--interval` | Yes | Refresh interval in seconds |

### crab daemon commit

Commit copy-on-write overlay mutations for a daemon-managed repository.

| Option | Required | Description |
|--------|----------|-------------|
| `--name` | Yes | Name of the repo |
| `--message` / `-m` | Yes | Git commit message |
| `--push` | No | Push the new commit to origin |
| `--json` | No | Emit structured output |

The daemon commit path uses the repo's daemon state directory, freezes overlay
writes during publish, checks that the tracked branch has not moved since the
mounted snapshot, and clears the overlay only after a successful local commit.
It stages the base tree directly in Git's index and materializes only changed
overlay files, so committing a small edit does not check out every blob in a
large or blobless repository. If `--push` fails, the local commit OID is
recorded in the publish transaction and the overlay remains available for
recovery.

## Examples

### Start the daemon

```bash
crab daemon
```

### Register a repository

```bash
crab daemon add-repo \
  --name ml-models \
  --remote crab://my-bucket/ml-models \
  --branch main \
  --mount-root /mnt/repos \
  --backend nfs
```

The repository will be mounted at `/mnt/repos/ml-models/`.

### Register multiple repositories

```bash
crab daemon add-repo --name dataset-a --remote crab://bucket/dataset-a --mount-root /mnt/data
crab daemon add-repo --name dataset-b --remote crab://bucket/dataset-b --mount-root /mnt/data
```

### List registered repos

```bash
crab daemon list
```

### Check repo status

```bash
crab daemon status --name ml-models
```

### Change refresh interval

```bash
crab daemon set-refresh --name ml-models --interval 60
```

### Commit overlay changes

```bash
crab daemon commit --name ml-models -m "Update generated artifacts"
crab daemon commit --name ml-models -m "Update generated artifacts" --push
```

Stable NFS writes are synced to the local overlay journal before they are
acknowledged, and shutdown drains pending writes. Overlay edits survive a
clean daemon restart but remain local until explicitly committed. The first
command creates a local Git commit and keeps it mounted; the second also pushes
the commit to `origin`. If a local commit already cleaned the overlay, a later
`commit --push` pushes that existing daemon commit instead of creating another
commit. Use `status` to inspect dirty paths before publishing:

```bash
crab daemon status --name ml-models
crab daemon commit --name ml-models -m "Update generated artifacts" --push
```

### Remove a repository

```bash
crab daemon remove-repo --name ml-models
```

## Daemon State

The daemon stores its state in `~/.crab/daemon/` (or the path specified by
`--root`). This includes:

- Repository registration records
- Mount state
- Refresh configuration
- A private per-repository NFS control endpoint while the mount is live

Registrations created before backend selection was introduced retain the
`fuse` backend. Re-run `add-repo` with the same name and `--backend nfs` to
switch one; the running daemon detects the configuration change and remounts
the repository.

## Backend Prerequisites

Run the matching preflight before registering a repository:

```bash
crab mount doctor --backend=nfs --mountpoint /mnt/repos/ml-models
crab mount doctor --backend=fuse --mountpoint /mnt/repos/ml-models
```

NFS starts one loopback NFSv3 server per repository inside the daemon and
mounts it through the operating system's native NFS client. FUSE uses an
in-process FUSE session. Both backends share the daemon's clone, snapshot,
overlay, hydration, refresh, and publish pipeline.

Each registered name owns an independent clone, snapshot, overlay, and mount.
Registering the same remote and branch more than once is supported but repeats
fetch, snapshot, and mount work; `add-repo` warns when it detects that setup.

Directory listings and metadata use the local snapshot. For a partial Git
clone, the first read of an ordinary Git blob can still fetch that blob from
the remote; later reads use the local Git object database. Crab pointer files
similarly hydrate their chunks on first access and then use the shared chunk
cache.

## Related Commands

- [`crab mount`](crab-mount.md) — mount a single repository.
- [`crab clone`](crab-clone.md) — clone a repository.
- [`crab fetch`](crab-fetch.md) — pre-fetch objects.
