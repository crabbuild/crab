---
name: crab-mount
description: Use and implement Crab virtual filesystem mounts and daemon-managed repositories. Use for mount, unmount, daemon, NFS/FUSE backends, hydration-on-read, overlays, refresh, and mount lifecycle failures.
---

# Crab virtual mounts

Treat a mount as a long-lived boundary between a filesystem client and a
versioned Crab snapshot. Keep source identity, overlay writes, hydration, and
daemon ownership explicit.

## Mount modes

- Remote source: read metadata and hydrate content on demand from a Crab or
  object-storage URL.
- Local source: expose another branch or revision without changing the active
  checkout.
- Read-only: reject writes with the platform’s read-only error and never create
  an overlay.
- Writable overlay: keep local changes separate from the source snapshot and
  define how commit, discard, and remount handle them.
- Static snapshot: disable refresh intentionally; a live mount must report
  refresh failures rather than silently serving an unknown revision.
- Foreground: keep the process attached for supervision. Background mode must
  persist enough state for `unmount`, daemon status, and cleanup.

`crab mount` is the interactive mountpoint-oriented workflow. `crab daemon`
is a separate persistent named-repository service with a registry, shared
cache, reconciliation loop, and explicit repository control subcommands. Do
not mix their state or assume one command manages the other mode.

## Lifecycle

1. Validate source URL/path, revision, backend, mountpoint, permissions, and
   nested-mount policy before creating a mount.
2. Create the coordinator and mount state, then publish readiness only after
   metadata can be read.
3. Resolve directory listings from metadata. Hydrate file bytes on read and
   verify identity before returning them to the kernel.
4. Serialize overlay writes, refreshes, hydration, unmount, and daemon cleanup.
5. On every failure, release locks, stop workers, close databases, remove
   temporary state, and leave the mountpoint in a known state.

## Overlay publication

- Review `mount status --live-only --verbose` and `mount diff`; ordinary
  status may fall back to persisted metadata and is not live-health proof.
- Pause writers before `mount export`, `commit`, or `reset`. Independent paths
  may mutate concurrently, but intersecting paths serialize and publication
  takes an exclusive overlay snapshot.
- `mount commit` checks that the base ref did not move, creates a Git commit,
  refreshes the snapshot, then clears the overlay. With `--push`, a failed push
  retains both overlay and transaction identity for a matching retry.
- `mount reset --overlay --yes`, daemon `remount --clean-overlay`, and
  `mount clean --all` are explicit destructive boundaries.

## Backend discipline

Keep FUSE and NFS behavior behind the same source and overlay contracts while
respecting platform-specific error and permission semantics. Do not make a
backend look healthy by swallowing mount, refresh, or hydration errors. Never
run an unsafe nested mount without an explicit allow flag.

`--backend=auto` prefers a ready NFS backend and otherwise uses FUSE when
available. NFS is a local loopback export, not a shared network filesystem.

## Verification

Test listing, stat, open, sequential read, random read, missing object, stale
revision, read-only write rejection, overlay write, refresh, unmount, daemon
restart, and interrupted cleanup. Compare read bytes with a direct hydrated
file and verify no worker, lock, or mount remains after teardown.
