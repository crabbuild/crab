/**
 * DaemonBridge — thin proxy from TermRouter to DaemonClient.
 *
 * Exposes handleSpawn / handleWrite / handleResize / handleKill /
 * warmReattach / handleSessionData / handleSessionExit / dispose.
 *
 * Does NOT register IPC handlers — TermRouter owns that.
 */

import type { DaemonClient } from "./daemon-client";
import { snapshotReplayData, type CreateOrAttachResult, type ColdRestorePayload } from "./protocol";
import { reconcileOnStartup } from "./startup-reconciliation";
import type { SessionInfo, TerminalSnapshot } from "./types";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/**
 * Minimal IpcMain interface — only what the bridge needs.
 * Allows testing without pulling in the full Electron dependency.
 */
export interface IpcMainLike {
  handle(channel: string, listener: (...args: unknown[]) => unknown): void;
  removeHandler(channel: string): void;
}

/**
 * Minimal BrowserWindow interface — only what the bridge needs.
 */
export interface BrowserWindowLike {
  isDestroyed(): boolean;
  webContents: {
    send(channel: string, ...args: unknown[]): void;
  };
}

/**
 * Minimal App interface — only what the bridge needs.
 */
export interface AppLike {
  on(event: string, listener: (...args: unknown[]) => void): void;
}

/** Agent metadata forwarded from the renderer spawn payload. */
export interface AgentMeta {
  taskId: string;
  agentType: string;
  worktreePath: string;
}

/** Spawn payload accepted by handleSpawn. */
export interface SpawnPayload {
  cwd: string;
  ownerPath?: string;
  shell?: string;
  env?: Record<string, string>;
  startupCommand?: string;
  agentMeta?: AgentMeta;
}

export interface DaemonBridgeOptions {
  /** The DaemonClient instance for communicating with the daemon. */
  client: DaemonClient;
  /** Returns the current main BrowserWindow (or null if destroyed). */
  getMainWindow: () => BrowserWindowLike | null;
  /** The Electron app instance for hooking before-quit. */
  app: AppLike;
  /** Current workspace path for filtering sessions on reattach. */
  getWorkspacePath: () => string | null;
  /** Session log writer — daemon doesn't own log files, they're app-scoped. */
  writeSessionLog?: (sessionId: string, data: string) => void;
  /**
   * Called when the daemon emits a cold-restore-available event.
   * The bridge translates the daemon session ID to a renderer ID and
   * forwards the event to the renderer via crab:term-cold-restore.
   * When omitted the event is silently dropped.
   */
  onColdRestoreAvailable?: (rendererId: string, payload: ColdRestorePayload) => void;
}

export interface RestoredSession {
  id: string;
  snapshot: TerminalSnapshot | null;
  error?: string;
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

interface BridgeState {
  /** Renderer ID → daemon session ID mapping. */
  rendererToDaemon: Map<string, string>;
  /** Daemon session ID → renderer ID (reverse mapping). */
  daemonToRenderer: Map<string, { rendererId: string; ownerPath?: string }>;
  /** Counter for generating renderer-facing IDs. */
  termCounter: number;
  /** Whether the bridge has been disposed. */
  disposed: boolean;
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/**
 * Create a DaemonBridge that proxies TermRouter calls to DaemonClient.
 *
 * Returns individual handler functions so TermRouter can call them
 * directly without going through IPC. The bridge also wires the
 * before-quit checkpoint + disconnect logic.
 */
export function createDaemonBridge(opts: DaemonBridgeOptions): {
  /** Spawn a new daemon session or reattach to an existing one. */
  handleSpawn: (payload: SpawnPayload, externalRendererId?: string) => Promise<{ id: string }>;
  /** Forward a write to the daemon session. */
  handleWrite: (id: string, data: string) => Promise<void>;
  /** Forward a resize to the daemon session. */
  handleResize: (id: string, cols: number, rows: number) => Promise<void>;
  /** Detach renderer streaming from a daemon session without killing it. */
  handleDetach: (id: string) => Promise<void>;
  /** Kill a daemon session. */
  handleKill: (id: string) => Promise<void>;
  /** Perform warm reattach — call after daemon connection is established. */
  warmReattach: () => Promise<RestoredSession[]>;
  /** Forward daemon session data events to the renderer. */
  handleSessionData: (sessionId: string, data: string) => void;
  /** Forward daemon session exit events to the renderer. */
  handleSessionExit: (sessionId: string, code: number) => void;
  /**
   * Forward a cold-restore-available daemon event to the renderer.
   * Resolves the daemon session ID to a renderer ID and emits
   * crab:term-cold-restore so the renderer can show the restore UX.
   */
  handleColdRestoreAvailable: (sessionId: string, payload: ColdRestorePayload) => void;
  /**
   * Synthesize a crab:term-exit event for a daemon session that disconnected
   * unexpectedly (e.g. daemon crash). Resolves the renderer ID from the
   * daemonToRenderer map and emits the exit event with a structured reason so
   * the renderer can show "Daemon connection lost" instead of a generic close.
   */
  synthesizeExitEvent: (daemonSessionId: string, code: number) => void;
  /** Returns all daemon session IDs currently tracked by the bridge. */
  getDaemonSessionIds: () => string[];
  /** Register a renderer-to-daemon session ID mapping (for reattach). */
  registerMapping: (rendererId: string, daemonSessionId: string, ownerPath?: string) => void;
  /** Dispose the bridge and clean up state. */
  dispose: () => void;
} {
  const { client, getMainWindow, app, getWorkspacePath, writeSessionLog, onColdRestoreAvailable } = opts;

  const state: BridgeState = {
    rendererToDaemon: new Map(),
    daemonToRenderer: new Map(),
    termCounter: 0,
    disposed: false,
  };

  // -------------------------------------------------------------------------
  // Stream event forwarding (daemon → renderer)
  // -------------------------------------------------------------------------

  function handleSessionData(sessionId: string, data: string): void {
    if (state.disposed) return;

    const binding = state.daemonToRenderer.get(sessionId);
    const rendererId = binding?.rendererId;
    if (!rendererId) return;

    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("crab:term-data", { id: rendererId, data, ownerPath: binding?.ownerPath });
    }

    // Preserve session log writing — daemon doesn't own log files
    if (writeSessionLog) {
      writeSessionLog(rendererId, data);
    }
  }

  function handleSessionExit(sessionId: string, code: number): void {
    if (state.disposed) return;

    const binding = state.daemonToRenderer.get(sessionId);
    const rendererId = binding?.rendererId;
    if (!rendererId) return;

    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("crab:term-exit", { id: rendererId, exitCode: code });
    }

    // Clean up mappings
    state.rendererToDaemon.delete(rendererId);
    state.daemonToRenderer.delete(sessionId);
    client.unmapSessionId(rendererId);
  }

  /**
   * Emit a synthetic crab:term-exit for a daemon session that disconnected
   * unexpectedly. The reason field lets the renderer distinguish a daemon
   * disconnect from a normal process exit.
   */
  function synthesizeExitEvent(daemonSessionId: string, code: number): void {
    if (state.disposed) return;

    const rendererId = state.daemonToRenderer.get(daemonSessionId)?.rendererId;
    if (!rendererId) return;

    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("crab:term-exit", {
        id: rendererId,
        exitCode: code,
        reason: "daemon-disconnected",
      });
    }

    // Clean up mappings — the session is gone from the daemon's perspective
    state.rendererToDaemon.delete(rendererId);
    state.daemonToRenderer.delete(daemonSessionId);
    client.unmapSessionId(rendererId);
  }

  // -------------------------------------------------------------------------
  // Handler implementations (called by TermRouter, not IPC directly)
  // -------------------------------------------------------------------------

  /**
   * Forward a cold-restore-available daemon event to the renderer.
   * Translates the daemon session ID to a renderer ID and emits
   * crab:term-cold-restore so the renderer can show the restore UX.
   */
  function handleColdRestoreAvailable(sessionId: string, payload: ColdRestorePayload): void {
    if (state.disposed) return;

    const binding = state.daemonToRenderer.get(sessionId);
    const rendererId = binding?.rendererId;
    if (!rendererId) return;

    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("crab:term-cold-restore", { id: rendererId, ...payload });
    }

    // Also forward to the optional callback (e.g. for init.ts wiring)
    onColdRestoreAvailable?.(rendererId, payload);
  }

  const handleSpawn = async (payload: SpawnPayload, externalRendererId?: string): Promise<{ id: string }> => {
    const { cwd, shell, env: extraEnv, startupCommand, agentMeta } = payload;

    const rendererId = externalRendererId ?? `term-${++state.termCounter}`;
    const workspacePath = payload.ownerPath || getWorkspacePath();
    const sessionId = workspacePath
      ? `${workspacePath}@@${rendererId}`
      : rendererId;

    try {
      const result = await client.request<CreateOrAttachResult>("createOrAttach", {
        sessionId,
        cols: 120,
        rows: 30,
        cwd: cwd || process.env.HOME || "/",
        env: extraEnv,
        shell,
        startupCommand,
        shellReadySupported: true,
        ...(agentMeta != null ? { agentMeta } : {}),
      });

      // Map renderer ID ↔ daemon session ID
      state.rendererToDaemon.set(rendererId, sessionId);
      state.daemonToRenderer.set(sessionId, { rendererId, ownerPath: workspacePath ?? undefined });
      client.mapSessionId(rendererId, sessionId);

      // If reattaching to existing session, send snapshot data immediately
      if (!result.isNew && result.snapshot) {
        const win = getMainWindow();
        if (win && !win.isDestroyed()) {
          const snapshotData = snapshotReplayData(result.snapshot);
          win.webContents.send("crab:term-data", { id: rendererId, data: snapshotData });
        }
      }

      return { id: rendererId };
    } catch (err) {
      // Clean up on failure
      state.rendererToDaemon.delete(rendererId);
      state.daemonToRenderer.delete(sessionId);
      client.unmapSessionId(rendererId);
      throw err;
    }
  };

  const handleWrite = async (id: string, data: string): Promise<void> => {
    const sessionId = state.rendererToDaemon.get(id);
    if (!sessionId) return;

    await client.notify("write", { sessionId, data });
  };

  const handleResize = async (id: string, cols: number, rows: number): Promise<void> => {
    const sessionId = state.rendererToDaemon.get(id);
    if (!sessionId) return;

    await client.notify("resize", { sessionId, cols, rows });
  };

  const handleDetach = async (id: string): Promise<void> => {
    const sessionId = state.rendererToDaemon.get(id);
    if (!sessionId) return;

    try {
      await client.request("detach", { sessionId });
    } catch {
      // Detach is best-effort; a dead/disconnected daemon still leaves the
      // renderer free to hide the tab and stop tracking this mapping.
    }

    state.rendererToDaemon.delete(id);
    state.daemonToRenderer.delete(sessionId);
    client.unmapSessionId(id);
  };

  const handleKill = async (id: string): Promise<void> => {
    const sessionId = state.rendererToDaemon.get(id);
    if (!sessionId) return;

    try {
      await client.request("kill", { sessionId });
    } catch {
      // Session may already be dead — ignore errors
    }

    // Clean up mappings immediately (don't wait for exit event)
    state.rendererToDaemon.delete(id);
    state.daemonToRenderer.delete(sessionId);
    client.unmapSessionId(id);
  };

  // -------------------------------------------------------------------------
  // before-quit: checkpoint + close sockets, don't kill daemon
  // -------------------------------------------------------------------------

  const handleBeforeQuit = async (): Promise<void> => {
    if (state.disposed) return;

    try {
      if (client.isConnected) {
        // Notify the daemon to checkpoint all live sessions, including
        // sessions hidden from the renderer session manager.
        const sessionIds = new Set(state.daemonToRenderer.keys());
        try {
          const sessions = await client.request<SessionInfo[]>("listSessions");
          for (const session of sessions) {
            if (session.isAlive) sessionIds.add(session.sessionId);
          }
        } catch {
          // Fall back to currently mapped sessions.
        }
        for (const sessionId of sessionIds) {
          try {
            await client.notify("checkpoint", { sessionId });
          } catch {
            // Best-effort — don't block quit
          }
        }

        // Close sockets gracefully — don't kill the daemon
        client.disconnect();
      }
    } catch {
      // Don't block app quit on errors
    }
  };

  app.on("before-quit", () => {
    void handleBeforeQuit();
  });

  // -------------------------------------------------------------------------
  // Warm reattach
  // -------------------------------------------------------------------------

  async function warmReattach(): Promise<RestoredSession[]> {
    if (state.disposed) return [];

    const workspacePath = getWorkspacePath();
    if (!workspacePath) return [];

    try {
      // Kill orphaned sessions whose worktree has been removed before reattaching.
      // Only the current workspace path is considered valid — sessions from
      // removed worktrees are cleaned up so they don't accumulate in the daemon.
      await reconcileOnStartup(new Set([workspacePath]), {
        listSessions: () => client.request<SessionInfo[]>("listSessions"),
        killSession: (sessionId) => client.request("kill", { sessionId }),
      });

      // List all sessions from the daemon (post-reconciliation)
      const sessions = await client.request<SessionInfo[]>("listSessions");

      // Filter to sessions belonging to the current workspace
      const workspaceSessions = sessions.filter((s) =>
        s.sessionId.startsWith(workspacePath + "@@"),
      );

      if (workspaceSessions.length === 0) return [];

      // Reattach to each session — handle partial failures
      const results: RestoredSession[] = [];

      for (const session of workspaceSessions) {
        const rendererId = `term-${++state.termCounter}`;

        try {
          const result = await client.request<CreateOrAttachResult>("createOrAttach", {
            sessionId: session.sessionId,
            cols: session.cols,
            rows: session.rows,
            cwd: session.cwd ?? undefined,
            // Omit `command` — we are reattaching, not spawning a new process
          });

          // Map renderer ID ↔ daemon session ID
          state.rendererToDaemon.set(rendererId, session.sessionId);
          state.daemonToRenderer.set(session.sessionId, { rendererId, ownerPath: workspacePath });
          client.mapSessionId(rendererId, session.sessionId);

          results.push({
            id: rendererId,
            snapshot: result.snapshot,
          });
        } catch (err) {
          // Partial failure — surface error per-session, don't block others
          results.push({
            id: rendererId,
            snapshot: null,
            error: err instanceof Error ? err.message : String(err),
          });
        }
      }

      // Emit crab:term-restored to renderer with all results
      const win = getMainWindow();
      if (win && !win.isDestroyed()) {
        win.webContents.send("crab:term-restored", {
          sessions: results,
        });
      }

      return results;
    } catch {
      // If listSessions fails entirely, return empty — don't crash
      return [];
    }
  }

  // -------------------------------------------------------------------------
  // Dispose
  // -------------------------------------------------------------------------

  function getDaemonSessionIds(): string[] {
    return [...state.daemonToRenderer.keys()];
  }

  /** Register a renderer↔daemon session ID mapping for reattach. */
  function registerMapping(rendererId: string, daemonSessionId: string, ownerPath?: string): void {
    state.rendererToDaemon.set(rendererId, daemonSessionId);
    state.daemonToRenderer.set(daemonSessionId, { rendererId, ownerPath });
    client.mapSessionId(rendererId, daemonSessionId);
  }

  function dispose(): void {
    state.disposed = true;
    state.rendererToDaemon.clear();
    state.daemonToRenderer.clear();
  }

  return {
    handleSpawn,
    handleWrite,
    handleResize,
    handleDetach,
    handleKill,
    warmReattach,
    handleSessionData,
    handleSessionExit,
    handleColdRestoreAvailable,
    synthesizeExitEvent,
    getDaemonSessionIds,
    registerMapping,
    dispose,
  };
}

// ---------------------------------------------------------------------------
// Legacy alias — keeps existing callers compiling until they migrate
// ---------------------------------------------------------------------------

/**
 * @deprecated Use createDaemonBridge instead. This alias exists for
 * backwards compatibility while callers migrate to the new API.
 */
export type IpcBridgeOptions = DaemonBridgeOptions & {
  ipcMain: IpcMainLike;
};

/**
 * @deprecated Use createDaemonBridge instead. This function registers
 * IPC handlers directly, which is the old pattern. TermRouter now owns
 * IPC registration.
 */
export function registerDaemonTerminalIpc(opts: IpcBridgeOptions): {
  warmReattach: () => Promise<RestoredSession[]>;
  handleSessionData: (sessionId: string, data: string) => void;
  handleSessionExit: (sessionId: string, code: number) => void;
  dispose: () => void;
} {
  const { ipcMain, ...bridgeOpts } = opts;

  const bridge = createDaemonBridge(bridgeOpts);

  // Register IPC handlers (legacy pattern — TermRouter will own this in Task 1.4)
  ipcMain.handle(
    "crab:term-spawn",
    ((_evt: unknown, cwd: string, shell?: string, extraEnv?: Record<string, string>, startupCommand?: string, agentMeta?: AgentMeta) =>
      bridge.handleSpawn({ cwd, shell, env: extraEnv, startupCommand, agentMeta })) as (...args: unknown[]) => unknown,
  );
  ipcMain.handle(
    "crab:term-write",
    ((_evt: unknown, id: string, data: string) => bridge.handleWrite(id, data)) as (...args: unknown[]) => unknown,
  );
  ipcMain.handle(
    "crab:term-resize",
    ((_evt: unknown, id: string, cols: number, rows: number) => bridge.handleResize(id, cols, rows)) as (...args: unknown[]) => unknown,
  );
  ipcMain.handle(
    "crab:term-kill",
    ((_evt: unknown, id: string) => bridge.handleKill(id)) as (...args: unknown[]) => unknown,
  );

  const originalDispose = bridge.dispose;
  const dispose = () => {
    originalDispose();
    ipcMain.removeHandler("crab:term-spawn");
    ipcMain.removeHandler("crab:term-write");
    ipcMain.removeHandler("crab:term-resize");
    ipcMain.removeHandler("crab:term-kill");
  };

  return {
    warmReattach: bridge.warmReattach,
    handleSessionData: bridge.handleSessionData,
    handleSessionExit: bridge.handleSessionExit,
    dispose,
  };
}
