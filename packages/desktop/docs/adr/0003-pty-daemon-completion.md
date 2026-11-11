# ADR 0003: PTY Daemon Completion

**Status:** Accepted

**Date:** 2025-07
**Deciders:** Crab Desktop team

---

## Context

The PTY daemon (`crab-desktop/electron/daemon/`) was designed to host durable
agent CLI sessions (Claude Code, Codex, Cursor CLI, OpenHands, etc.) that
survive Electron quit/reopen cycles. The original spec landed the wire
protocol, server, terminal host, headless emulator, agent tracker, history
manager, checkpoint timer, quotas, audit logger, auto-respawn, circuit
breaker, and fd-handoff modules — all well-tested in isolation.

The gap was the wiring:

- `daemon-main.ts` constructed only `DaemonServer` and `createNodePtySpawner`;
  all durability subsystems were dormant.
- `main.ts` used a single global mode (`parsePtyMode()`) that removed the
  in-process handlers when daemon mode won, making mixed-use impossible.
- Agent metadata (`taskId`, `agentType`, `worktreePath`) never reached the
  daemon, so the agent dashboard and orchestrator reattach loop were broken.
- The upgrade successor branch (`--handoff --snapshot`) was empty.
- `electron-builder.yml` never copied the daemon binary to `resources/daemon/`,
  so daemon mode silently failed in packaged builds.
- Cold-restore UX (after a daemon crash or OS reboot) was unimplemented.

This ADR records the four architectural decisions that drove the completion
work.

### Structured chat ownership clarification (2026-11)

These durability decisions apply to interactive PTY sessions. Structured Codex
and Claude Code chats use their native stdio adapters and persisted provider
session identities. They do not create daemon-owned PTYs, including in the
`v2.0.0-alpha.1` adapter factories. Terminal-only launches return before creating
an orchestrator task row and retain their own daemon metadata and restoration.

The main-process task-to-PTY reattach loop was removed: a completed native chat
has no matching daemon session, so that loop incorrectly changed `reviewing`
to `failed` after startup. Native task recovery belongs to the orchestrator and
chat journal; terminal restoration remains with TermRouter and TerminalView.

---

## Decision 1: Per-Session Routing Replaces Global Mode

### Problem

The old model called `parsePtyMode()` once at startup and then called
`ipcMain.removeHandler("crab:term-spawn")` to swap backends. This made it
impossible to run durable agent sessions alongside ephemeral user shells in
the same app instance.

### Decision

`Mode_Resolution` (is the daemon available?) and `Session_Type` (does this
spawn want daemon?) become independent axes.

- A new `TermRouter` class registers `crab:term-spawn`, `crab:term-write`,
  `crab:term-resize`, and `crab:term-kill` **once** for the lifetime of the
  app. Both backends (`DaemonBridge` and `InProcessBridge`) stay registered
  permanently — no `removeHandler` race.
- `resolveSessionType(payload, modeResolution)` defaults all spawns to
  `"daemon"`: an agent can be started interactively after shell creation.
  An explicit `"in-process"` request creates an ephemeral terminal.
- Renderer IDs are partitioned: `term-d-UUID` for daemon sessions, `term-l-UUID`
  for local sessions. IDs remain unique across app restarts, preventing a new
  terminal from accidentally attaching to a surviving background process.
- When `sessionType: "daemon"` is requested but the daemon is unavailable or
  the circuit breaker is open, the router returns a typed error
  (`"daemon-unavailable"` / `"daemon-circuit-open"`) rather than silently
  falling back. The renderer can then offer an explicit "Use in-process
  instead?" action so the user knows their durability guarantee was not
  honored.

### Consequences

- The renderer must pass `sessionType` on every spawn. A positional shim in
  `preload.ts` keeps existing call sites compiling during migration.
- `DaemonClient.getMode()` is deleted; `diagnostics-ui-ipc.ts` accepts a
  `getMode: () => string` callback from `init.ts` instead.
- The `InProcessBridge` refuses new spawns after `app.before-quit` fires to
  avoid orphaned processes.

---

## Decision 2: Daemon-Side Durability Wired via a Hooks Object

### Problem

`DaemonServer` needed to invoke side effects (history writes, audit log
entries, quota checks, activity tracking) without importing those modules
directly — that would couple the server to every subsystem and make it
untestable in isolation.

### Decision

`DaemonServer` accepts a `DaemonServerHooks` object at construction time:

```typescript
interface DaemonServerHooks {
  beforeSpawn?: () => void;                    // throws to refuse
  onSessionCreated?: (id, opts) => void;
  onSessionData?: (id, byteLength) => void;    // length only, no buffer copy
  onSessionExit?: (id, code) => void;
  onSessionAttached?: (id) => void;
  onSessionKilled?: (id) => void;
  onAgentState?: (id, state, prev) => void;
  onCheckpointRequested?: (id) => void;
}
```

`daemon-main.ts` constructs all subsystems and wires them into the hooks:

- `beforeSpawn` → `quotaManager.checkCanCreateSession(host.sessionCount)`
- `onSessionCreated` → `historyManager.openSession` + `auditLogger.sessionCreated`
  + `activityTracker.register`
- `onSessionData` → `activityTracker.markDirty` (byte length only — avoids
  copying the data buffer)
- `onSessionExit` → `historyManager.closeSession` + `auditLogger.sessionExited`
  + `activityTracker.remove` + `checkpointTimer.removeSession`
- `onCheckpointRequested` → `historyManager.markDirty` + `historyManager.checkpoint`
- `onAgentState` → `auditLogger.agentStateChanged`

`gracefulShutdown` calls `checkpointTimer.finalCheckpoint()` before
`server.shutdown()` so the most recent state is always persisted.

### Consequences

- `DaemonServer` remains independently testable; hooks are fakes in unit tests.
- Adding a new subsystem (e.g., a future rate limiter) requires only a new
  hook field — no changes to the server's core routing logic.
- The `onSessionData` hook receives byte length, not the buffer, to avoid an
  extra allocation on every data event. Subsystems that need the actual data
  (e.g., a future content scanner) would need a separate hook.

---

## Decision 3: Cold-Restore UX Is Renderer-Driven

### Problem

After a daemon crash or OS reboot, `HistoryManager` may hold checkpoints for
sessions whose subprocesses are gone. Automatically re-spawning those
subprocesses on daemon startup would be surprising and could expose stale
environment variables (including secrets that were sanitized before saving).

### Decision

The daemon detects cold-restore candidates at startup (sessions with
`meta.json.endedAt === undefined` and a `checkpoint.json`) and emits a
`cold-restore-available` stream event to connected clients. It does **not**
auto-spawn anything.

The flow is:

1. Daemon emits `cold-restore-available` with `{ sessionId, snapshot, savedAt,
   sanitizedKeys, cwd, cols, rows }`.
2. The main process translates this to a `crab:term-cold-restore` IPC event
   to the renderer.
3. The renderer shows a tab with a "Restored from \<timestamp\>" badge and a
   confirm dialog.
4. If `sanitizedKeys` is non-empty, the renderer prompts: "These environment
   variables were stripped on save: \<keys\>. Provide values now or use
   defaults?" before proceeding.
5. On confirm, the renderer calls `crab:term-cold-restore-confirm` with the
   user-provided env. The daemon spawns a fresh subprocess in the persisted
   `cwd` (falling back to `os.homedir()` if the directory no longer exists),
   then writes `snapshot.snapshotAnsi + snapshot.rehydrateSequences` to the
   renderer xterm via `crab:term-data` before any keystrokes flow.
6. On cancel, the renderer calls `crab:term-cold-restore-discard`; the daemon
   removes the history directory.

The "Restored from \<timestamp\>" badge remains on the tab so the user always
knows the state came from a checkpoint rather than a live process.

### Consequences

- Secret hygiene is preserved: secrets are never persisted, and the user
  explicitly provides replacements before re-spawn.
- The renderer drives the UX timing, so it can batch multiple cold-restore
  prompts and let the user dismiss them individually.
- If the daemon crashes before the user responds, the checkpoint files remain
  on disk and the flow repeats on the next daemon start.
- The `cold-restore-resume` daemon request validates that the persisted `cwd`
  exists; `cwdMissing: true` in the result triggers a one-line notice in the
  renderer.

---

## Decision 4: Production Packaging via `resolveDaemonEntryPath`

### Problem

`main.ts` hardcoded two paths for the daemon entry point:
`path.join(__dirname, "daemon", "daemon-main.cjs")` for development and
`path.join(process.resourcesPath, "daemon", "daemon-main.cjs")` for packaged
builds. `electron-builder.yml` never included an `extraResources` entry to
copy the daemon bundle, so packaged builds silently fell back to in-process
mode.

### Decision

- `main.ts` calls `resolveDaemonEntryPath(app.isPackaged, process.resourcesPath,
  path.join(__dirname, "daemon"))` instead of the hardcoded paths. The
  resolver also checks the `CRAB_DAEMON_ENTRY` environment variable for
  support debugging.
- `electron-builder.yml` includes `dist-electron/daemon` in ASAR. The resolver
  uses `<resourcesPath>/app.asar/dist-electron/daemon/daemon-main.cjs` on every
  platform. Native dependencies remain unpacked; loading JavaScript through
  ASAR keeps node-pty's helper path translation correct.
- `scripts/rename-to-cjs.mjs` gains a build-time check that fails the build
  if `dist-electron/daemon/daemon-main.cjs` is missing, catching packaging
  regressions before they reach CI.
- On resolve failure, `main.ts` emits a fatal error that includes the searched
  paths and the `CRAB_DAEMON_ENTRY` override hint, then falls back to
  in-process mode rather than crashing the app.
- The daemon runs under `ELECTRON_RUN_AS_NODE=1` so it shares the same Node
  runtime and native bindings (including `node-pty`) as the Electron main
  process — no separate `electron-rebuild` step needed.

### Consequences

- Packaged builds now correctly host daemon sessions; the silent fallback is
  replaced by an explicit error with actionable guidance.
- The `CRAB_DAEMON_ENTRY` override allows QA and support to point at a
  specific daemon binary without rebuilding the app.
- A Playwright end-to-end smoke test (`tests/e2e/daemon-survives-restart.spec.ts`)
  packages the app, spawns a daemon-typed terminal, kills the renderer, relaunches,
  and asserts warm reattach — catching missing-file regressions on every release.

---

## Alternatives Considered

### A. Keep global mode selection

Rejected. A single global mode cannot serve the mixed-use case (durable agent
session + ephemeral user shell open simultaneously). The `removeHandler` swap
also introduced a race window where IPC calls could arrive between the remove
and the re-register.

### B. Auto-spawn on cold restore

Rejected. Silently re-spawning subprocesses with potentially stale or
sanitized environments would be surprising and could expose the user to
unexpected behavior (e.g., a re-spawned agent CLI picking up a wrong
worktree). Renderer-driven restore keeps the user in control.

### C. Inline durability logic in `DaemonServer`

Rejected. Importing `HistoryManager`, `AuditLogger`, `ResourceQuotaManager`,
and `CheckpointTimer` directly into `DaemonServer` would make the server
untestable in isolation and couple unrelated concerns. The hooks pattern keeps
each module independently testable and the server's routing logic clean.

### D. Ship daemon as a separate npm package / standalone binary

Rejected for v1. The `ELECTRON_RUN_AS_NODE=1` approach reuses the same Node
runtime and native bindings already bundled with Electron, avoiding a separate
build pipeline and `electron-rebuild` step. A standalone binary remains an
option for future versions if the daemon needs to run outside Electron.

---

## References

- `crab-desktop/electron/daemon/daemon-main.ts` — daemon entry point
- `crab-desktop/electron/term-router.ts` — per-session routing
- `crab-desktop/electron/daemon/daemon-bridge.ts` — daemon IPC bridge
- `crab-desktop/electron/in-process-bridge.ts` — in-process IPC bridge
- `crab-desktop/electron/daemon/init.ts` — daemon subsystem initialization
- `crab-desktop/electron/daemon/daemon-server.ts` — hooks contract
- `crab-desktop/docs/architecture/terminal-subsystem.md` — terminal architecture overview
- `.kiro/specs/pty-daemon-completion/design.md` — full design document

## Session recovery ownership

The repository or task persistence key owns the daemon session prefix and saved
layout. Repository paths, rather than transient repository IDs, key kept-alive
renderers. Layout writes happen on changes and use atomic replacement; recovery
does not depend on Electron delivering a quit event before closing its windows.

Warm attach enters an alternate screen before painting its snapshot. Normal
and cold replay use the same snapshot composer, so mode restoration cannot erase
the recovered agent display. Shell selection, login environment and startup
commands flow through the daemon; shell-ready wrappers gate initial input.

SSH channels attach to remote tmux sessions on a separate Crab socket. A saved
SSH identity is checked and attached, never recreated implicitly. Renderer
cleanup detaches; explicit SSH tab termination destroys that session. Missing
tmux is a visible failure rather than a fallback to a shell with weaker lifetime.
