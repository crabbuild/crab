# SSH Subsystem Architecture

Crab Desktop routes remote repository RPCs over an authenticated SSH channel to
`crab-desktop-agent`. The sidecar and its Crab CLI come from the same release.
Remote native Codex/Claude chat dispatch is still an open implementation gate;
local native-provider proof does not establish remote chat support. See
[agent chat compatibility](../agent-chat-compatibility.md) for evidence and gaps.

## Ownership and routing

| Surface | Owner | Contract |
| --- | --- | --- |
| Authentication and host trust | `electron/ssh-manager.ts`, `ssh-known-hosts.ts` | Verify every direct/jump host before authentication; disconnect cancels pending trust decisions and opening work |
| Initial/manual/reconnect bootstrap | `electron/ssh-manager.ts` | One connection owns its deployment, channel, capability handshake and readiness publication |
| Runtime acquisition | `electron/ssh-runtime.ts` | Exact app version and remote target; verified private cache; concurrent borrowers share downloads without sharing cancellation |
| Remote installation | `electron/ssh-deployer.ts` | Matching CLI/sidecar, private staging, acknowledged writes, remote hashes, atomic publication and owned cleanup |
| Repository RPC | `electron/ssh-router.ts` | Local requests use the local agent; `ssh://profileId/path` requests use that profile's channel; batches become independent NDJSON requests |
| Capability negotiation | `electron/ssh-capabilities.ts` | Read actual capabilities after channel attachment; clear stale results when the owner changes |
| Progress UI | `src/components/ssh/DeployProgressIndicator.tsx` | Typed, profile-scoped events; starting completes only after the capability handshake |

The router owns UTF-8 framing and outstanding requests per exact channel. EOF,
error or close retires that channel's requests. Reconnect queues only inspected
reads; writes and unclassified calls fail immediately, so uncertain writes are
never replayed. A connected SSH transport without an attached sidecar advertises
no repository RPC capabilities.

## Runtime acquisition and installation

Supported remote targets are Linux and macOS on x64 or arm64. Development uses
`resources/native/<mac|linux>-<arch>/runtime.json` and its matching binaries.
Packaged apps obtain these dedicated assets from the app's exact GitHub release:

```
Crab-<version>-runtime-<darwin|linux>-<x64|arm64>.json
Crab-<version>-runtime-<darwin|linux>-<x64|arm64>-crab
Crab-<version>-runtime-<darwin|linux>-<x64|arm64>-crab-desktop-agent
```

Dedicated remote assets have their own checksums: signing an Electron bundle can
change its embedded binaries. Release assembly requires all four remote targets
and verifies identity, regular files, size limits and SHA-256 before publication.
The downloader restricts redirects to approved HTTPS release hosts, bounds sizes
and duration, and verifies hashes before publishing its cache. Invalid completed
caches fail with an explicit removal/reconnect instruction.

Each remote pair installs under the authenticated user's home directory:

```
.crab/desktop/runtimes/<version>/<platform>-<arch>/<content-hash>/
  crab
  crab-desktop-agent
  git-remote-crab
```

The Git helper is a hard link to that runtime's CLI. Parent directories and
executables are checked without accepting symbolic-link leaves. An unpredictable
upload directory has an SSH stdin-owned cleanup guard. Uploads wait for SFTP
CLOSE acknowledgements before remote checksums and directory publication. An
upload without progress for 30 seconds fails; cancellation retires SFTP before
releasing the cleanup guard, preventing a pending write from racing cleanup.
Published directories are outside that guard's scope. Reuse verifies all three
executables again; deployment never replaces a running session's binary or kills
another session by process name.

The sidecar starts with `CRAB_BIN` pointing to this CLI and its runtime prepended
to `PATH`. The session manager attaches its sole RPC reader, negotiates capabilities
and then publishes connected readiness. A transport failure or cancellation
retires the owned channel and capability cache before recovery.

## Limits and qualification

The sidecar implements core repository RPCs and task comparison. Its capability
handshake explicitly disables unsupported extended execution, watch and session
services. The Electron health/audit/diagnostics modules and related design goals
do not by themselves establish a working remote service; in particular the
legacy audit bridge is not wired into the production main process.

Real loopback SSH/SFTP proof covers immutable installation, reuse, checksum
failure, owner cancellation and stalls, including quoted/Unicode home paths.
The built release pair also creates a worktree through its uploaded CLI and
returns native comparison bytes through the real session manager and router.
A renderer fixture checks progress isolation between profiles. These checks do
not qualify remote native chat, concurrent native indexer ownership, a published
release endpoint, packaged main-process bootstrap or older Linux ABI support.
Cross-platform CI builds and bootstraps each remote target; its results must be
reviewed before release. The [manual workspace checklist](../remote-workspace-smoke-test.md)
tracks user-visible remote operations that need separate qualification.
