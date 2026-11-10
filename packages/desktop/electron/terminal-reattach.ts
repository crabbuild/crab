import * as path from "node:path";
import type { DaemonClient } from "./daemon/daemon-client";
import type { createDaemonBridge } from "./daemon/daemon-bridge";
import type { IpcMainLike, TermRouter } from "./term-router";
import { snapshotReplayData, type CreateOrAttachResult } from "./daemon/protocol";

export interface ReattachCandidateSession {
  sessionId: string;
  isAlive: boolean;
  initialCwd?: string | null;
  cwd: string | null;
}

export interface DaemonSessionSummaryCandidate extends ReattachCandidateSession {
  state: string;
  shellState?: string;
  pid?: number | null;
  cols?: number;
  rows?: number;
  createdAt?: number;
  lastDataAt?: number;
  agentMeta?: unknown;
}

export interface DaemonSessionSummary {
  /** Renderer-facing terminal ID, stable for a daemon session. */
  id: string;
  /** Full daemon session ID, usually `${workspacePath}@@${rendererId}`. */
  daemonSessionId: string;
  state: string;
  shellState?: string;
  isAlive: boolean;
  pid: number | null;
  initialCwd?: string | null;
  cwd: string | null;
  cols: number;
  rows: number;
  createdAt: number;
  lastDataAt: number;
  agentMeta?: unknown;
}

export function pathBelongsToRepo(candidate: string | null | undefined, repoPath: string): boolean {
  if (!candidate || !repoPath) return false;
  if (candidate === repoPath) return true;
  return [path.sep, "/", "\\"].some((sep) => candidate.startsWith(repoPath + sep));
}

export function sessionBelongsToRepoPath(
  session: ReattachCandidateSession,
  repoPath: string,
): boolean {
  if (!session.isAlive || !repoPath) return false;
  const repoPrefix = repoPath + "@@";
  const separator = session.sessionId.lastIndexOf("@@");
  const rendererId = session.sessionId.slice(separator + 2);
  // UUID sessions have explicit repository/task ownership. Cwd changes must
  // not make them appear in another repository's session list.
  if (separator >= 0 && /^term-d-[0-9a-f-]{36}$/.test(rendererId)) {
    return session.sessionId.slice(0, separator) === repoPath;
  }
  // v2.0.0-alpha.1 used workspace prefixes and numeric IDs; initial cwd is
  // needed to recover those live processes when opening their repository.
  return (
    session.sessionId.startsWith(repoPrefix) ||
    pathBelongsToRepo(session.initialCwd, repoPath) ||
    pathBelongsToRepo(session.cwd, repoPath)
  );
}

export function rendererIdFromDaemonSessionId(sessionId: string, repoPath?: string): string {
  const repoPrefix = repoPath ? `${repoPath}@@` : "";
  if (repoPrefix && sessionId.startsWith(repoPrefix)) {
    return sessionId.slice(repoPrefix.length);
  }
  const sepIdx = sessionId.lastIndexOf("@@");
  return sepIdx >= 0 ? sessionId.slice(sepIdx + 2) : sessionId;
}

export function toDaemonSessionSummary(
  session: DaemonSessionSummaryCandidate,
  repoPath?: string,
): DaemonSessionSummary {
  return {
    id: rendererIdFromDaemonSessionId(session.sessionId, repoPath),
    daemonSessionId: session.sessionId,
    state: session.state,
    shellState: session.shellState,
    isAlive: session.isAlive,
    pid: session.pid ?? null,
    initialCwd: session.initialCwd,
    cwd: session.cwd,
    cols: session.cols ?? 120,
    rows: session.rows ?? 30,
    createdAt: session.createdAt ?? 0,
    lastDataAt: session.lastDataAt ?? session.createdAt ?? 0,
    agentMeta: session.agentMeta,
  };
}

export function registerTerminalSessionIpc(
  ipcMain: IpcMainLike,
  client: DaemonClient,
  bridge: ReturnType<typeof createDaemonBridge>,
  router: TermRouter,
) {
  async function list(repoPath: string) {
    if (!client.isConnected) throw new Error("Terminal service is not connected");
    const sessions = await client.request<DaemonSessionSummaryCandidate[]>("listSessions");
    return sessions.filter(session => sessionBelongsToRepoPath(session, repoPath));
  }
  async function attach(repoPath: string, sessionId: string) {
    const session = (await list(repoPath)).find(session => session.sessionId === sessionId);
    if (!session) throw new Error("Terminal session is not available for this repository");
    const id = rendererIdFromDaemonSessionId(sessionId, repoPath);
    const result = await client.request<CreateOrAttachResult>("createOrAttach", {
      sessionId, cols: session.cols ?? 120, rows: session.rows ?? 30,
      cwd: session.cwd ?? session.initialCwd ?? undefined,
    });
    bridge.registerMapping(id, sessionId, repoPath);
    router.registerSession(id, "daemon");
    return {
      id, daemonSessionId: sessionId,
      snapshot: result.snapshot ? snapshotReplayData(result.snapshot) : null,
      cwd: session.cwd ?? session.initialCwd ?? null,
      summary: toDaemonSessionSummary(session, repoPath),
    };
  }
  ipcMain.handle("crab:term-list-daemon-sessions", async (_event, value) => {
    const { repoPath } = value as { repoPath: string };
    return (await list(repoPath)).map(session => toDaemonSessionSummary(session, repoPath))
      .sort((a, b) => (b.lastDataAt || b.createdAt) - (a.lastDataAt || a.createdAt));
  });
  ipcMain.handle("crab:term-attach-daemon-session", (_event, value) => {
    const { repoPath, daemonSessionId } = value as { repoPath: string; daemonSessionId: string };
    return attach(repoPath, daemonSessionId);
  });
  ipcMain.handle("crab:term-detach-daemon-session", async (_event, value) => {
    const { rendererId } = value as { rendererId: string };
    await bridge.handleDetach(rendererId);
    router.unregisterSession(rendererId);
  });
  ipcMain.handle("crab:term-kill-daemon-session", async (_event, value) => {
    const { repoPath, daemonSessionId } = value as { repoPath: string; daemonSessionId: string };
    const session = (await list(repoPath)).find(session => session.sessionId === daemonSessionId);
    if (!session) throw new Error("Terminal session is not available for this repository");
    await client.request("kill", { sessionId: daemonSessionId });
  });
  // The v2.0.0-alpha.1 preload exposes bulk reattach. Keep that wire contract
  // routed through explicit attach so it shares ownership and snapshot rules.
  ipcMain.handle("crab:term-reattach-sessions", async (_event, value) => {
    const { repoPath } = value as { repoPath: string };
    return Promise.all((await list(repoPath)).map(session => attach(repoPath, session.sessionId)));
  });
}
