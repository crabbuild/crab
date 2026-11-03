/**
 * Agent Dashboard IPC — exposes agent session state to the renderer.
 *
 * Registers:
 * - `crab:agent-sessions` IPC handler: returns AgentSessionInfo[] with
 *   elapsed working time for the agent dashboard view.
 * - `crab:agent-state-changed` event forwarding: relays DaemonClient's
 *   onAgentStateChange callback to the renderer process.
 */

import type { DaemonClient } from "./daemon-client";
import type { AgentActivityState } from "./types";
import type { IpcMainLike, BrowserWindowLike } from "./ipc-bridge";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface AgentDashboardInfo {
  sessionId: string;
  taskId: string;
  agentType: string;
  worktreePath: string;
  state: AgentActivityState;
  stateChangedAt: number;
  elapsedWorkingMs: number;
  exitCode?: number;
}

export interface AgentDashboardIpcOptions {
  /** Electron ipcMain instance. */
  ipcMain: IpcMainLike;
  /** The DaemonClient instance for querying agent sessions. */
  client: DaemonClient;
  /** Returns the current main BrowserWindow (or null if destroyed). */
  getMainWindow: () => BrowserWindowLike | null;
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/**
 * Calculate elapsed working time for an agent session.
 * Returns the time in ms since stateChangedAt if the session is in "working" state,
 * otherwise returns 0.
 */
export function calculateElapsedWorkingMs(
  state: AgentActivityState,
  stateChangedAt: number,
  now?: number,
): number {
  if (state !== "working") return 0;
  const currentTime = now ?? Date.now();
  return Math.max(0, currentTime - stateChangedAt);
}

/**
 * Register agent dashboard IPC handlers.
 *
 * Wires:
 * - `crab:agent-sessions` → queries DaemonClient for agent sessions
 *   and enriches with elapsed working time
 * - Provides a `handleAgentStateChange` callback to forward state-change
 *   events to the renderer via `crab:agent-state-changed`
 *
 * Returns a dispose function and the state-change handler to wire into
 * the DaemonClient's onAgentStateChange callback.
 */
export function registerAgentDashboardIpc(opts: AgentDashboardIpcOptions): {
  /** Forward agent state changes to the renderer. Wire into DaemonClient.onAgentStateChange. */
  handleAgentStateChange: (sessionId: string, state: AgentActivityState, previousState: AgentActivityState) => void;
  /** Dispose the IPC handlers. */
  dispose: () => void;
} {
  const { ipcMain, client, getMainWindow } = opts;

  // -------------------------------------------------------------------------
  // IPC Handler: crab:agent-sessions
  // -------------------------------------------------------------------------

  const handleGetAgentSessions = async (): Promise<AgentDashboardInfo[]> => {
    const sessions = await client.getAgentSessions();
    const now = Date.now();

    return sessions.map((session) => {
      const agentMeta = session.agentMeta!;
      return {
        sessionId: session.sessionId,
        taskId: agentMeta.taskId,
        agentType: agentMeta.agentType,
        worktreePath: agentMeta.worktreePath,
        state: agentMeta.activityState,
        stateChangedAt: agentMeta.stateChangedAt,
        elapsedWorkingMs: calculateElapsedWorkingMs(
          agentMeta.activityState,
          agentMeta.stateChangedAt,
          now,
        ),
        ...(session.agentMeta!.activityState === "exited" ? { exitCode: (session as any).exitCode } : {}),
      };
    });
  };

  ipcMain.handle("crab:agent-sessions", handleGetAgentSessions);

  // -------------------------------------------------------------------------
  // Event Forwarding: crab:agent-state-changed
  // -------------------------------------------------------------------------

  function handleAgentStateChange(
    sessionId: string,
    state: AgentActivityState,
    previousState: AgentActivityState,
  ): void {
    const win = getMainWindow();
    if (win && !win.isDestroyed()) {
      win.webContents.send("crab:agent-state-changed", {
        sessionId,
        state,
        previousState,
        timestamp: Date.now(),
      });
    }
  }

  // -------------------------------------------------------------------------
  // Dispose
  // -------------------------------------------------------------------------

  function dispose(): void {
    ipcMain.removeHandler("crab:agent-sessions");
  }

  return {
    handleAgentStateChange,
    dispose,
  };
}
