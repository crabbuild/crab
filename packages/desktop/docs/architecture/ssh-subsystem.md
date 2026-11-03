# SSH Subsystem Architecture

## Overview

The SSH subsystem enables Crab Desktop to manage repositories on remote
servers as if they were local. It uses the **Remote Agent** pattern: the
same `crab-desktop-agent` binary runs on the remote server, and the
local Electron app tunnels JSON-RPC 2.0 over an SSH channel.

## Key Principle

The renderer is completely unaware of whether a repo is local or remote.
All routing happens in the main process via the RPC Router.

## Components

### Main Process (Electron)

```
┌─────────────────────────────────────────────────────────┐
│  SSH Session Manager (ssh-manager.ts)                    │
│  • Connection pool (one TCP conn, many channels)        │
│  • Reconnection state machine                           │
│  • Suspend/resume detection                             │
├─────────────────────────────────────────────────────────┤
│  RPC Router (ssh-router.ts)                             │
│  • repo_id → local agent OR remote agent                │
│  • Transparent to renderer                              │
│  • Batch RPC support                                    │
├─────────────────────────────────────────────────────────┤
│  Response Cache (ssh-cache.ts)                          │
│  • TTL-based caching with adaptive TTL                  │
│  • Invalidation on write operations                     │
│  • Prefetching on repo open                             │
├─────────────────────────────────────────────────────────┤
│  Agent Deployer (ssh-deployer.ts)                       │
│  • Detect remote arch, upload binary via SFTP           │
│  • Version check + hash-based skip                      │
├─────────────────────────────────────────────────────────┤
│  Port Forward Manager (ssh-port-forward.ts)             │
│  • Local → Remote forwards                              │
│  • Dynamic detection from terminal output               │
├─────────────────────────────────────────────────────────┤
│  File Transfer Manager (ssh-file-transfer.ts)           │
│  • SFTP upload/download with progress                   │
│  • Transfer queue with concurrency control              │
├─────────────────────────────────────────────────────────┤
│  System Health Monitor (ssh-health.ts)                  │
│  • Polls disk, memory, GPU metrics                      │
│  • Alerts on threshold breaches                         │
├─────────────────────────────────────────────────────────┤
│  Connection Diagnostics (ssh-diagnostics.ts)            │
│  • Latency test, bandwidth estimate                     │
│  • Recommendations engine                               │
├─────────────────────────────────────────────────────────┤
│  Resilience Manager (ssh-resilience.ts)                 │
│  • Session persistence across restarts                  │
│  • Graceful degradation during reconnection             │
│  • Audit logging                                        │
├─────────────────────────────────────────────────────────┤
│  Performance Manager (ssh-performance.ts)               │
│  • Prefetching, delta updates                           │
│  • Connection pooling, lazy subscriptions               │
│  • Response streaming                                   │
└─────────────────────────────────────────────────────────┘
```

### Remote Server

```
~/.crab/bin/crab-desktop-agent
  • Same binary, same JSON-RPC protocol
  • Full access to local filesystem + git + crab
  • Spawns CLI tools (crab, git, claude, codex)
  • System health reporting (disk, mem, GPU)
```

## Data Flow

### RPC Call (e.g., git status)

1. Renderer calls `window.crab.rpc("git_status_porcelain", { repo_id })`
2. Main process receives via IPC
3. RPC Router checks `repo_id` prefix:
   - Local: forward to local agent process
   - Remote (`ssh://profileId/path`): forward to remote agent channel
4. Remote agent processes request, returns JSON-RPC response
5. Router returns result to renderer via IPC

### Connection Lifecycle

```
DISCONNECTED → CONNECTING → DEPLOYING → CONNECTED
                                              ↓
                                         (network error)
                                              ↓
                                        RECONNECTING
                                         ↙        ↘
                                   CONNECTED    DISCONNECTED
                                   (success)    (60s timeout)
```

## Security Model

- Private keys never leave the local machine
- Credentials stored only in OS keychain
- Host key verification prevents MITM attacks
- Remote agent runs as the user (no privilege escalation)
- Audit log records all connection events (no credentials)
- Port forwards are explicit and user-configured

## Error Codes

| Code | Name | Meaning |
|------|------|---------|
| 2001 | SSH_AUTH_FAILED | Authentication rejected |
| 2002 | SSH_HOST_UNREACHABLE | Cannot connect to host |
| 2003 | SSH_HOST_KEY_CHANGED | Host key mismatch (MITM) |
| 2004 | SSH_AGENT_DEPLOY_FAILED | Binary upload/chmod failed |
| 2005 | SSH_AGENT_START_FAILED | Agent process won't start |
| 2006 | SSH_CHANNEL_CLOSED | SSH channel unexpectedly closed |
| 2007 | SSH_TIMEOUT | Operation timed out |
| 2008 | SSH_RECONNECTING | Temporarily unavailable |
| 2009 | SSH_DISK_FULL | Remote disk full |
| 2010 | SSH_PERMISSION_DENIED | Remote filesystem permission error |
| 2011 | SSH_PORT_IN_USE | Port forward failed (port occupied) |

## Performance Targets

| Operation | Target | Strategy |
|-----------|--------|----------|
| File tree expand | < 200ms | Single RPC, lazy load |
| Git status | < 300ms | Cached 5s, invalidated on write |
| Open file preview | < 500ms | Remote agent parses |
| Terminal keystroke | < 50ms | Direct SSH channel |
| Diff computation | < 1s | Remote agent computes |
| File save | < 200ms | Direct write via agent |

## Caching Strategy

Adaptive TTL based on connection latency:

| RTT | Status TTL | Refs TTL | Summary TTL |
|-----|-----------|---------|------------|
| <50ms | 3s | 5s | 5s |
| 50-100ms | 5s | 10s | 10s |
| 100-200ms | 10s | 15s | 15s |
| >200ms | 15s | 30s | 30s |
