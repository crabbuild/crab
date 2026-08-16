---
name: crab-mount
description: Use and implement Crab virtual filesystem mounts and daemon-managed repositories. Use whenever a request mentions `crab mount`, unmounting, FUSE, NFS, on-demand file access, writable overlays, mount refresh/switch/commit, or the mount daemon and coordinator.
compatibility: Platform-specific Crab build with FUSE or NFS support and the appropriate OS mount permissions.
---

# Crab mounts

Own the VFS boundary: remote or local repository snapshots, on-demand
hydration, writable overlays, the coordinator, and daemon-managed mounts.
Keep platform readiness distinct from repository content correctness.

## Command scope

`mount`, `unmount`, `daemon`, and hidden `coordinator` lifecycle commands.

## Mount lifecycle

1. Run the backend doctor and inspect platform features, mountpoint state,
   permissions, existing mounts, repository/ref, and read-only intent.
2. Mount the smallest requested repository/ref. Confirm whether reads should
   hydrate on demand and whether writes go to an overlay.
3. Use `status`, `list`, and `refresh` to distinguish a live backend state from
   persisted fallback state. Use `switch` only with an explicit ref.
4. Treat overlay `diff`, `export`, `reset`, and `commit` as separate state
   transitions. `reset --overlay --yes` discards mutations; verify the target
   mount and confirmation before running it.
5. For `commit --push`, verify both the Git commit and remote push using the
   `crab-git-sync` boundary. Do not report a successful mount commit from a
   local overlay alone.
6. For daemon-managed repos, manage registration, enable/disable, refresh,
   remount, fetch, and commit through the daemon namespace. Keep mount root,
   repo name, backend, and branch explicit.

## Platform and concurrency rules

- Read the feature gates and platform-specific implementation before
  recommending FUSE or NFS. A command can parse while the backend is absent.
- Preserve coordinator lock, socket, PID, idle shutdown, and graceful mount
  cleanup behavior. Do not kill a coordinator to repair an ordinary overlay
  issue.
- Verify that active mounts are absent before cleaning mount caches.
- Test read-only, dirty overlay, refresh, switch, and shutdown paths separately.

## Read first

- `crab/docs/guides/mount.md`
- `crab/docs/guides/daemon.md`
- `crab/docs/architecture/virtual-filesystem.md`
- `crab/docs/architecture/vfs-coordinator.md`
- `crab/docs/architecture/nfs-mount-architecture.md`
- `crab/src/cmd/mount.rs`
- `crab/src/vfs/`
- `.codex/skills/crab-cli-core/references/contracts.md`
