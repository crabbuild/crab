import * as path from "node:path";

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
