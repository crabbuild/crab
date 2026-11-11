# PTY Daemon

Crab Desktop runs terminal sessions through two backends: a durable
**PTY daemon** that survives Electron restarts, and a lightweight
**in-process** backend whose lifecycle is tied to the app window.
This document explains how the two modes work, how to configure them,
and how to use the Health & Diagnostics panel to inspect daemon state.

## Overview

```
┌─────────────────────────────────────────────────────────────────┐
│  Renderer                                                       │
│  termSpawn({ sessionType: "daemon" | "in-process", ... })       │
└────────────────────────┬────────────────────────────────────────┘
                         │ contextBridge IPC
┌────────────────────────▼────────────────────────────────────────┐
│  Main Process                                                   │
│  TermRouter — dispatches by sessionType                         │
│  ├── DaemonBridge ──► DaemonClient ──► PTY Daemon (detached)    │
│  └── InProcessBridge ──► node-pty (in-process)                  │
└─────────────────────────────────────────────────────────────────┘
                         │ Unix socket
┌────────────────────────▼────────────────────────────────────────┐
│  PTY Daemon (~/.crab/desktop/daemon/daemon-v1.sock)             │
│  ├── HistoryManager  (~/.crab/desktop/terminal-history/)        │
│  ├── CheckpointTimer (5 s active / 60 s idle)                   │
│  ├── ResourceQuotaManager (~/.crab/desktop/daemon/quotas.json)  │
│  └── AuditLogger ($CRAB_DAEMON_AUDIT_LOG)                       │
└─────────────────────────────────────────────────────────────────┘
```

The daemon is a detached Node child process spawned via
`ELECTRON_RUN_AS_NODE=1`. It owns PTY master file descriptors
independently of Electron, so agent CLIs (Claude Code, Codex, Cursor
CLI, OpenHands, etc.) keep running across desktop close/reopen cycles.

## Per-Session Model

Every terminal spawn carries an explicit `sessionType`. The two values
are independent of each other — you can have daemon-backed agent
sessions and in-process user shells open at the same time.

### Daemon sessions (`sessionType: "daemon"`)

- Owned by the detached PTY daemon process.
- Survive Electron quit and relaunch (warm reattach).
- Survive daemon crashes with on-disk checkpoints (cold restore).
- Appear in the agent dashboard with activity state tracking.
- Identified by renderer IDs prefixed `term-d-N`.

### In-process sessions (`sessionType: "in-process"`)

- Spawned directly inside the Electron main process via `node-pty`.
- Closed when the app window closes.
- No persistence, no checkpoints, no quota enforcement.
- Identified by renderer IDs prefixed `term-l-N`.

### Default rules

Ordinary terminal spawns default to `"daemon"`. An explicit
`CRAB_PTY_MODE=in-process` setting retains its ephemeral-shell contract. An agent may be started
interactively after shell creation, so durability cannot depend on detecting an
agent in the initial spawn payload. Explicit `"in-process"` requests remain
available for ephemeral terminals. Unavailable daemon service is a typed error,
not a silent downgrade to a process that dies with the app.

Terminal IDs use a UUID after `term-d-` or `term-l-`, and durable sessions have a
repository/task owner prefix. IDs do not collide with surviving sessions after
an Electron restart. Reattach of v2.0.0-alpha.1 numeric IDs retains its original
cwd-based lookup for that tagged upgrade boundary.

### Manual override

The tab context menu offers **New daemon terminal** and **New
in-process terminal** entries so you can override the default at any
time. The "New daemon terminal" entry is disabled when the daemon is
unavailable or the circuit breaker is open; a tooltip explains why.

Each tab shows a small badge — **daemon** or **local** — next to the
title so you can tell which backend is in use at a glance.

### Error behavior

When `sessionType: "daemon"` is requested but the daemon is
unavailable (or the circuit breaker is open), the main process returns
a typed error to the renderer rather than silently falling back to
in-process. The renderer surfaces this as a toast so you know the
durability guarantee was not honored.

## Environment Variables

### `CRAB_PTY_MODE`

Controls whether the daemon is started at all.

| Value        | Behavior |
|--------------|----------|
| `auto`       | Attempt to start the daemon. Durable spawns fail visibly if unavailable; explicit local terminals remain available. This is the default. |
| `daemon`     | Require the daemon; throw a fatal error if it cannot start. |
| `in-process` | Skip the daemon; ordinary shells are ephemeral. Explicit durable/agent requests fail visibly. |

```bash
# Force daemon-only mode (useful for CI or scripted environments)
CRAB_PTY_MODE=daemon crab-desktop

# Disable the daemon entirely (legacy mode)
CRAB_PTY_MODE=in-process crab-desktop
```

The resolved mode is published as `crab:pty-mode` IPC on startup and
is visible in the Health & Diagnostics panel.

### `CRAB_DAEMON_ENTRY`

Override the path to the daemon entry script. Useful for testing a
custom build or diagnosing packaging issues.

```bash
CRAB_DAEMON_ENTRY=/path/to/daemon-main.js crab-desktop
```

Resolution order when `CRAB_DAEMON_ENTRY` is not set:

1. **Packaged app**: `<resourcesPath>/app.asar/dist-electron/daemon/daemon-main.cjs`
2. **Development**: `dist-electron/daemon/daemon-main.cjs`

Packaged JavaScript must load through ASAR. Native modules and the terminal
spawn helper remain in `app.asar.unpacked`; node-pty translates the ASAR path
to that physical helper path. Starting JavaScript directly from the unpacked
directory makes node-pty expand the path twice and prevents terminal startup.

If the resolved path does not exist, the main process emits a fatal
error that includes all searched paths and the `CRAB_DAEMON_ENTRY`
override hint. Durable requests fail visibly; explicit local terminals remain available.

Whether the path came from the env var is surfaced in the diagnostics
panel as `daemonEntryFromEnv` for support debugging.

### `CRAB_DAEMON_AUDIT_LOG`

Set to a file path to enable JSON-lines audit logging of all session
lifecycle events and agent state transitions.

```bash
CRAB_DAEMON_AUDIT_LOG=~/.crab/desktop/daemon/audit.log crab-desktop
```

When unset, the audit logger is a no-op and no files are written.

Log format — one JSON object per line:

```json
{"ts":"2026-05-11T10:30:00.000Z","event":"session.created","sessionId":"abc123","agent":true}
{"ts":"2026-05-11T10:30:05.000Z","event":"agent.state_changed","sessionId":"abc123","state":"working","previousState":"idle"}
{"ts":"2026-05-11T10:45:00.000Z","event":"session.exited","sessionId":"abc123","code":0}
```

Event types:

| Event | Meaning |
|-------|---------|
| `session.created` | New PTY session spawned |
| `session.attached` | Client reconnected to an existing session |
| `session.detached` | Client disconnected (session still running) |
| `session.killed` | Session explicitly killed |
| `session.exited` | Subprocess exited |
| `agent.state_changed` | Agent activity state transition (`working` / `waiting` / `idle` / `exited`) |

The log rotates at 50 MB, keeping 5 rotated files
(`audit.log`, `audit.log.1`, … `audit.log.5`). Write failures are
silently swallowed — the daemon never crashes due to audit log errors.

The current audit log path is shown in the Health & Diagnostics panel
(read-only) so you can confirm whether auditing is active.

## Resource Quotas (`~/.crab/desktop/daemon/quotas.json`)

The daemon enforces resource limits to prevent a forgotten long-running
session from exhausting the host. Limits are read from
`~/.crab/desktop/daemon/quotas.json` at startup and reloaded on
`SIGHUP` without a restart.

### Schema

```json
{
  "maxConcurrentSessions": 50,
  "maxMemoryMb": 1024,
  "maxSessionLifetimeHours": 72
}
```

| Key | Default | Meaning |
|-----|---------|---------|
| `maxConcurrentSessions` | `50` | Maximum number of live PTY sessions. New spawns are refused with a `quota-exceeded` error when this limit is reached. |
| `maxMemoryMb` | `1024` | Maximum daemon heap memory in MB. New spawns are refused when this threshold is exceeded. |
| `maxSessionLifetimeHours` | `72` | Sessions older than this are gracefully killed by the lifetime enforcement timer (runs every 60 s). |

An example file is included at
`crab-desktop/electron/daemon/quotas.example.json`.

### Defaults

When `quotas.json` is missing or cannot be parsed, the defaults above
apply. The daemon logs a warning but continues running.

### Live reload

Send `SIGHUP` to the daemon process to reload quotas without a
restart:

```bash
# Find the daemon PID
cat ~/.crab/desktop/daemon/daemon.pid

# Reload quotas
kill -HUP <pid>
```

The daemon logs the new values after a successful reload.

### Quota errors

When a spawn is refused due to a quota violation, the renderer receives
a structured error:

```
quota-exceeded:<quota-name>: <human-readable message>
```

The renderer surfaces this as a toast: "Cannot start session: \<reason\>".

## Session Persistence

### Checkpoints

The daemon writes terminal snapshots to
`~/.crab/desktop/terminal-history/<sessionId>/checkpoint.json` on a
timer:

- **Active sessions** (data received in the last interval): every 5 s
- **Idle sessions**: every 60 s
- **On graceful shutdown**: a final checkpoint is written before the
  daemon exits

Authenticated shutdown requests, idle expiry and successful daemon replacement
all delegate to the process owner. It closes the socket server, disposes history,
quota and memory timers, then exits; the RPC acknowledgement precedes that work.
Closing the desktop application still detaches its durable terminal sessions.

Checkpoints are atomic (write to a temp file, then rename) and bounded
to a single small JSON file per session. The history directory is
pruned at startup to enforce a 7-day age cap and a 100 MB total size
cap.

### Warm reattach

When Electron restarts while the daemon is still running, the renderer
reconnects to live sessions automatically. Terminal state comes from
`headless-emulator.getSnapshot()` — no disk I/O needed.

### Cold restore

When the daemon itself crashes or the host reboots, the renderer
detects sessions with a `checkpoint.json` but no `endedAt` in
`meta.json`. For each such session, a **"Restored from \<timestamp\>"**
badge appears on the tab.

If environment variables were sanitized before saving (secrets stripped
— see below), a modal prompts you to provide replacement values before
the subprocess is re-spawned.

### Secret sanitization

The daemon never persists known-secret environment variables to disk.
The following patterns are stripped from `meta.json` before writing:

- `AWS_SECRET_ACCESS_KEY`
- Any key matching `*_TOKEN`
- Any key matching `*_KEY`

The list of stripped keys is stored alongside the checkpoint so the
cold-restore prompt can name them explicitly.

## Auto-Respawn and Circuit Breaker

### Auto-respawn

When the daemon process dies unexpectedly, the main process:

1. Synthesizes a `crab:term-exit` event (with `reason: "daemon-disconnected"`)
   for every tracked daemon session so the renderer shows "Daemon
   connection lost" rather than a generic close.
2. Attempts to respawn the daemon up to 3 times with a 1 s delay
   between attempts.
3. Reconnects the `DaemonClient` after a successful respawn.

Mutating IPC handlers (`crab:term-spawn`, `crab:term-write`,
`crab:term-resize`, `crab:term-kill`) are wrapped with
`withRetryOnDisconnect` so a single transient disconnect does not
surface as a user-visible error.

### Circuit breaker

The circuit breaker prevents a crash-looping daemon from hammering the
host. It opens after 3 failures within 60 s and stays open for 5
minutes.

While the breaker is open:

- New `sessionType: "daemon"` spawns are refused with error
  `"daemon-circuit-open"` and a `cooldownRemainingMs` field.
- The "New daemon terminal" tab context menu entry is disabled.
- The Health & Diagnostics panel shows a warning banner with a
  **Restart Daemon** button.

The breaker resets automatically after the cooldown period, or
immediately when you click **Restart Daemon**.

## Health & Diagnostics Panel

The Health & Diagnostics panel is the primary tool for inspecting
daemon state. Open it from **Settings → Advanced → PTY Daemon**.

The panel refreshes on open and every 10 s. It pauses when the tab is
not visible.

### Layout

```
┌─────────────────────────────────────────────────────────────────┐
│  PTY Daemon Health                    [Refresh] [Restart Daemon] │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Daemon Status                                   🟢 OK  │    │
│  │  Uptime: 4h 12m                                         │    │
│  │  Protocol version: 2                                    │    │
│  │  Mode: daemon (auto)                                    │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Sessions                                               │    │
│  │  Active: 3  (daemon: 2, in-process: 1)                  │    │
│  │  Peak: 7                                                │    │
│  │  Total spawned: 42                                      │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Resources                                              │    │
│  │  Memory: 128 MB  [ok]                                   │    │
│  │  High-throughput sessions: 0                            │    │
│  │  File descriptors: 24                                   │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Circuit Breaker                              [closed]  │    │
│  │  Failures (last 60 s): 0                                │    │
│  │  Respawn count: 0                                       │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │  Paths                                                  │    │
│  │  Audit log: ~/.crab/desktop/daemon/audit.log            │    │
│  │  Quotas: ~/.crab/desktop/daemon/quotas.json             │    │
│  │  History: ~/.crab/desktop/terminal-history/             │    │
│  └─────────────────────────────────────────────────────────┘    │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Fields

| Field | Meaning |
|-------|---------|
| **Uptime** | How long the daemon process has been running |
| **Protocol version** | Wire protocol version (current: 2) |
| **Mode** | Resolved PTY mode and configured `CRAB_PTY_MODE` value |
| **Active sessions** | Live PTY sessions, broken down by type |
| **Peak sessions** | Highest concurrent session count since daemon start |
| **Total spawned** | Cumulative session count since daemon start |
| **Memory** | Daemon heap usage with pressure level (ok / warn / critical) |
| **High-throughput sessions** | Sessions where the headless emulator is bypassed due to memory pressure |
| **File descriptors** | Open fd count in the daemon process |
| **Circuit breaker** | Breaker state (closed / open), failure count, cooldown remaining |
| **Respawn count** | Number of times the daemon has been auto-respawned |
| **Audit log** | Path to the audit log file, or "disabled" if `CRAB_DAEMON_AUDIT_LOG` is unset |
| **Quotas** | Path to `quotas.json` |
| **History** | Base path for terminal history and checkpoints |

### Warning banners

**Circuit breaker open:**

```
┌─────────────────────────────────────────────────────────────────┐
│  ⚠️  Daemon circuit breaker is open                             │
│  The daemon crashed 3 times in the last 60 s.                   │
│  Cooldown: 4m 32s remaining.                                    │
│                                          [Restart Daemon]       │
└─────────────────────────────────────────────────────────────────┘
```

Clicking **Restart Daemon** calls `crab:daemon-restart`, which
triggers `autoRespawn.restartDaemon()` and resets the breaker.

**Path mismatch:**

```
┌─────────────────────────────────────────────────────────────────┐
│  ℹ️  Daemon binary path mismatch                                │
│  The running daemon was started from a different app location.  │
│  Previous path: /Applications/Crab Desktop (old).app/...       │
│                                    [Apply Daemon Upgrade]       │
└─────────────────────────────────────────────────────────────────┘
```

This appears when the app was updated but its existing daemon is still running.
Application updates reuse that daemon, so a changed application path does not end
its shells or foreground agents. **Apply Daemon Upgrade** refuses replacement
while terminals are live: inherited PTY descriptors currently cannot be resized
by their successor. End those terminals before requesting daemon replacement.

### Actions

| Button | IPC call | Effect |
|--------|----------|--------|
| Refresh | — | Re-fetches diagnostics immediately |
| Restart Daemon | `crab:daemon-restart` | Kills all daemon sessions, respawns the daemon |
| Apply Daemon Upgrade | `crab:daemon-prepare-upgrade` | Requests replacement only when no terminals are running |

Application restart and daemon replacement have different lifetime contracts.
The restart recovery checks cover reusing the existing daemon. The low-level
fd-handoff implementation is not used to migrate live terminals.

## Troubleshooting

### Daemon fails to start

1. Check `CRAB_PTY_MODE` — if set to `daemon`, the app will refuse to
   start without a working daemon.
2. Check `CRAB_DAEMON_ENTRY` — if set, verify the file exists at that
   path.
3. Open the Health & Diagnostics panel and look at the **Mode** field
   for the fallback reason.
4. In development, run `npm run build:electron` to ensure
   `dist-electron/daemon/daemon-main.cjs` exists.

### Sessions not surviving restart

1. Confirm the session was spawned as `sessionType: "daemon"` (check
   the tab badge — it should say **daemon**, not **local**).
2. Check that `CRAB_PTY_MODE` is not set to `in-process`.
3. Open the Health & Diagnostics panel and verify the daemon is
   running (uptime > 0).

### Quota errors

1. Check `~/.crab/desktop/daemon/quotas.json` — lower limits may have
   been set manually.
2. Kill idle sessions to free capacity.
3. Send `SIGHUP` to the daemon after editing `quotas.json` to reload
   without a restart.

### Audit log not appearing

Verify `CRAB_DAEMON_AUDIT_LOG` is set and points to a writable path.
The Health & Diagnostics panel shows the resolved path (or "disabled"
if the env var is unset).

### High memory pressure

The **Memory** field in the diagnostics panel shows `warn` (> 70% of
the 1 GB default limit) or `critical` (> 90%). When pressure is high,
the daemon enters high-throughput mode for the busiest sessions,
bypassing the headless emulator to reduce heap usage. The
**High-throughput sessions** counter shows how many sessions are
affected.

To reduce pressure:
- Close idle daemon sessions.
- Increase `maxMemoryMb` in `quotas.json` and send `SIGHUP`.
- Restart the daemon from the diagnostics panel to reclaim heap.
