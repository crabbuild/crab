# crab recovery

Internal recovery scan for inflight operations.

## Synopsis

This command is used internally and is not directly exposed as a CLI subcommand.
It is documented here for completeness and for advanced users who may encounter
recovery-related log messages.

## Description

The recovery module scans for inflight operation markers left by crashed or
interrupted crab processes. When a push or upload is interrupted mid-flight,
markers are left in the remote store to track the incomplete operation. The
recovery scan identifies these markers and either retries the operation or
cleans up the stale state.

## How It Works

1. Lists all inflight markers in the remote store.
2. For each marker:
   - Checks if the upload was actually committed (completed despite the marker).
   - If committed: cleans the marker.
   - If the marker is stale (from a dead process): cleans the marker.
   - If the marker is live (from a running process): retries the upload.
3. After recovery, runs post-ref cleanup:
   - Moves staging data to the cache.
   - Installs shard metadata.
   - Deletes files-DB rows for completed uploads.
   - Clears the inflight marker.

## When Does Recovery Run?

Recovery is triggered automatically:
- During push operations, before starting a new push.
- During `crab fsck --repair`, which detects and cleans stale markers.

## Related Commands

- [`crab fsck`](crab-fsck.md) — detect and repair stale markers.
- [`crab staging clean`](crab-staging.md) — clean local staging state.
