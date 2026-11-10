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
| Initial/manual/reconnect bootstrap | `electron/ssh-manager.ts` | One connection owns deployment, channel, capability handshake, repository restoration and readiness publication |
| Runtime acquisition | `electron/ssh-runtime.ts` | Exact app version and remote target; verified private cache; concurrent borrowers share downloads without sharing cancellation |
| Remote installation | `electron/ssh-deployer.ts` | Matching CLI/sidecar, private staging, acknowledged writes, remote hashes, atomic publication and owned cleanup |
| Repository RPC | `electron/ssh-router.ts` | Local requests use the local agent; `ssh://profileId/path` requests use that profile's channel; batches become independent NDJSON requests |
| Native stdio transport | `electron/ssh-native-process.ts`, `agent/src/native_process.rs` | One UUID and exact channel own stdin, output credit and confirmed process-group closure; task lifecycle integration remains open |
| Capability negotiation | `electron/ssh-capabilities.ts` | Read actual capabilities after channel attachment; clear stale results when the owner changes |
| Progress UI | `src/components/ssh/DeployProgressIndicator.tsx` | Typed, profile-scoped events; starting completes only after the capability handshake |

The production open handler uses `RemoteFileBrowser.openAsRepo`, which supplies
the required `repo_id`, registers the provider before returning, and projects
the server-local workspace as an SSH handle for renderer worktree routing.

The router owns UTF-8 framing and outstanding requests per exact channel. EOF,
error or close retires that channel's requests. Reconnect queues only inspected
reads; writes and unclassified calls fail immediately, so uncertain writes are
never replayed. Acknowledged repository opens are retained in memory; successful
closes remove them. Each new sidecar reopens those repositories before readiness
and queued reads. Failed or uncertain opens cannot enter this set, and changing
the profile host, port or username drops the old endpoint's identities. A connected SSH transport without an attached sidecar advertises
no repository RPC capabilities.

## Native provider process transport

Codex app-server and Claude Code share their existing Electron protocol adapters
across local Node processes and the private remote stdio factory. Six
`native_process_*` RPCs own open, stdin write, stdin close, output acknowledgement,
stop and release. Provider protocol parsing, controls and approvals stay in the
native adapters. The factory sends executable, arguments and working directory;
credentials and PATH come from the server login. Local environment overrides
never cross this transport.

Each output stream has one 16 KiB frame of credit with exact sequence
acknowledgement. Stdin is chunked separately and acknowledged after pipe writes.
Neither receipt establishes provider turn acceptance; that remains the native
protocol's responsibility. Replacement channels cannot inherit a process's
writes. Transport loss rejects closure as unconfirmed and never replays stdin.

The Unix sidecar owns a process group through normal exit or bounded TERM/KILL
cancellation. It drains supervisors on stdin EOF or TERM/HUP/INT before runtime
shutdown. Release requires a confirmed close receipt. macOS zombie-only groups
can return EPERM; a bounded wait must observe ESRCH rather than treating a
permission failure as success. SIGKILL/crash recovery, session-detached children
and persistent orphan ownership remain release gates.

Opt-in real SSH checks discover models and complete file-writing turns with
both installed providers using the existing server CLI login. They also verify
owned child closure when the sidecar receives TERM/HUP. This proves the native
transport foundation, not remote task UI integration. The current orchestrator
still calls missing `task_spawn`/`task_iterate`; remote baseline, cancellation,
recovery and Apply/Discard must join the shared lifecycle before that path works.
The extended service `processManagement` capability remains disabled because
its different public handler contract is still unimplemented.

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
to `PATH`. The session manager attaches its sole RPC reader, negotiates capabilities,
restores its acknowledged repositories and then publishes connected readiness. A transport failure or cancellation
retires the owned channel and capability cache before recovery.

The background indexer owns a private derived cache per sidecar process. Its
SlateDB instance closes before directory cleanup. Independent profiles and
replacement sidecars therefore do not open the same writer path; cache contents
rebuild for each process lifetime. `indexer_status.db_path` reports that current
private location. The former account-wide cache is no longer opened.

## Task workspace creation

`worktree_create` validates a task UUID and resolves the requested baseline to a
commit before changing the filesystem. New worktrees reserve
`$HOME/.crab/worktrees/<full-task-uuid>` atomically; existing paths are rejected.
Creation never removes another task's worktree or a directory that looks stale.
The existing core `crab worktree add` contract creates the checkout and branch.
Failure releases only an empty reservation; a partially created checkout remains
available for recovery. Missing HOME or invalid refs fail without a fallback.

`worktree_create` returns the task branch, the resolved `base_ref` commit and
the repository's `base_branch` at creation (`null` for detached HEAD). Electron
persists the branch as the Apply destination and the commit as the comparison
baseline. Apply rejects an absent or deleted destination before staging source
changes. Existing saved paths stay opaque and are not relocated or read through
a compatibility path.

## Disposable task workspace ownership

Creation records the task UUID, branch, canonical root, shared Git directory and
private Git directory in `crab-desktop-task-owner.json` inside the worktree's
private Git administration directory. Apply, Discard and `worktree_destroy`
validate this record against the opened repository and live Git identity before
mutation. Recreated directories, primary checkouts, changed branches and foreign
task/repository identities cannot authorize cleanup. Old or damaged records
require explicit ownership recovery; no path-based adoption is performed.

Native process factories can carry the task UUID. Such opens require the saved
owner even when the directory's `.git` pointer disappeared; they cannot become
unowned commands during removal. Native writers take shared file leases, while
task mutations take exclusive leases. Independent sidecars therefore reject
cleanup while a native writer remains owned. Confirmed physical closure releases
its lease before publishing the receipt; uncertain closure retains it while
the sidecar lives. The
contract uses [Rust file locks](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock),
which map to nonblocking flock on supported Unix targets. It coordinates Crab's
owners, not external editors or arbitrary user commands.

Cleanup delegates to the existing core `crab worktree remove` contract. Git
refusal preserves files and branches; the sidecar does not recursively delete
directories or prune other worktrees. Apply propagates status, staging and commit
errors before merging. A successful merge remains confirmed if later cleanup
fails: `cleanup_error` and removal flags describe the retained resources. Both
Electron callers report that warning, and only acknowledged mutations change
task status. Discard failures and missing remote ownership reach the caller.

The Unix sidecar starts an independent worker for each accepted Apply. The
worker owns the workspace lease and supervises the Git command group, including
hooks. Graceful shutdown closes the worker's lifetime pipe and waits for it to
exit. Killing the sidecar also closes that pipe, so the worker can stop Git
while it still holds the lease. Apply creates `crab-desktop-task-apply.pending`
in the task's private Git directory before staging and clears it after a
completed operation with confirmed writer closure. If the operation is
interrupted or closure is uncertain, a replacement sidecar rejects Apply,
Discard, Destroy and native writer opens for that workspace. To recover it,
confirm its Git writers have stopped, inspect both refs and the source and target
indexes, then remove the pending file from the task's verified private Git
directory. Recovery must determine whether Git already published a ref update.

Real SSH proof covers unrelated directories, wrong task/branch/root identities,
two sidecars competing with a native writer, a recreated directory's rejected
native open, successful Discard/Destroy, and a confirmed merge whose locked
worktree remains intact. Full remote startup/snapshot/cancellation integration,
captured Apply-target ownership, durable mutation recovery, recovery for older
records, process-name orphan checks and crash/detached-child ownership remain
release gates. Leases live with Rust owners; canceled mutation RPCs and orphaned
Git subprocesses still need ownership integration before release.

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
A renderer fixture checks progress isolation between profiles. Native checks
also prove concurrent indexer cache ownership and cleanup on macOS.
Remote native chat, a published release endpoint, packaged main-process bootstrap,
older Linux ABI support and cross-OS indexer ownership remain qualification gates.
Cross-platform CI builds and bootstraps each remote target; its results must be
reviewed before release. The [manual workspace checklist](../remote-workspace-smoke-test.md)
tracks user-visible remote operations that need separate qualification.
