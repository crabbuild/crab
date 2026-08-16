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

## Backend discipline

Keep FUSE and NFS behavior behind the same source and overlay contracts while
respecting platform-specific error and permission semantics. Do not make a
backend look healthy by swallowing mount, refresh, or hydration errors. Never
run an unsafe nested mount without an explicit allow flag.

## Verification

Test listing, stat, open, sequential read, random read, missing object, stale
revision, read-only write rejection, overlay write, refresh, unmount, daemon
restart, and interrupted cleanup. Compare read bytes with a direct hydrated
file and verify no worker, lock, or mount remains after teardown.
