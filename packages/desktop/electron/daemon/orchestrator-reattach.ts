/**
 * Orchestrator Reattach — reconnects agent tasks to daemon sessions on app restart.
 *
 * On launch, the orchestrator finds tasks in `running` or `reviewing` state.
 * This module queries the daemon for agent sessions, matches them by taskId,
 * and either:
 *   - Reattaches the event stream (session alive)
 *   - Reports exit codes (session already exited)
 *   - Transitions tasks to `failed` (session missing entirely)
 *
 * Retries 3× with 2s intervals if the daemon is unreachable.
 * The AgentAdapter contract is unchanged — the adapter still receives events
 * through the same callback interface.
 */

import type { SessionInfo } from "./types";
import type { CreateOrAttachResult } from "./protocol";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface RunningTask {
  id: string;
  status: "running" | "reviewing";
  worktreePath: string;
  sessionId?: string;
}

export interface ReattachResult {
  /** Tasks successfully reattached to live daemon sessions. */
  reattached: Array<{ taskId: string; sessionId: string }>;
  /** Tasks whose sessions already exited — includes exit code. */
  exited: Array<{ taskId: string; sessionId: string; exitCode: number }>;
  /** Tasks with no matching daemon session — should transition to failed. */
  missing: Array<{ taskId: string }>;
  /** Whether the daemon was reachable at all. */
  daemonReachable: boolean;
}

export interface ReattachDeps {
  /** Query all agent sessions from the daemon. */
  getAgentSessions: () => Promise<SessionInfo[]>;
  /** Reattach to an existing session (receives snapshot, begins streaming). */
  createOrAttach: (sessionId: string, cols: number, rows: number) => Promise<CreateOrAttachResult>;
  /** Callback when a task's session has already exited. */
  onSessionExited: (taskId: string, exitCode: number) => void;
  /** Callback when a task's session is missing (transition to failed). */
  onSessionMissing: (taskId: string) => void;
  /** Callback when a task is successfully reattached. */
  onReattached: (taskId: string, sessionId: string) => void;
  /** Optional logger. */
  log?: (level: "info" | "warn" | "error", message: string) => void;
}

export interface ReattachOptions {
  /** Maximum retry attempts if daemon is unreachable. Default: 3. */
  maxRetries?: number;
  /** Delay between retries in ms. Default: 2000. */
  retryDelayMs?: number;
  /** Default terminal dimensions for reattach. */
  cols?: number;
  rows?: number;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const DEFAULT_MAX_RETRIES = 3;
const DEFAULT_RETRY_DELAY_MS = 2000;
const DEFAULT_COLS = 120;
const DEFAULT_ROWS = 30;

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/**
 * Attempt to reattach running/reviewing tasks to their daemon sessions.
 *
 * Queries the daemon for agent sessions, matches by taskId in session
 * metadata, and handles each case (alive, exited, missing).
 *
 * Retries up to `maxRetries` times with `retryDelayMs` intervals if
 * the daemon is unreachable.
 */
export async function orchestratorReattach(
  runningTasks: RunningTask[],
  deps: ReattachDeps,
  opts: ReattachOptions = {},
): Promise<ReattachResult> {
  const maxRetries = opts.maxRetries ?? DEFAULT_MAX_RETRIES;
  const retryDelayMs = opts.retryDelayMs ?? DEFAULT_RETRY_DELAY_MS;
  const cols = opts.cols ?? DEFAULT_COLS;
  const rows = opts.rows ?? DEFAULT_ROWS;
  const log = deps.log ?? (() => {});

  if (runningTasks.length === 0) {
    log("info", "No running tasks to reattach");
    return { reattached: [], exited: [], missing: [], daemonReachable: true };
  }

  // Attempt to query agent sessions with retries
  let agentSessions: SessionInfo[] | null = null;

  for (let attempt = 1; attempt <= maxRetries; attempt++) {
    try {
      agentSessions = await deps.getAgentSessions();
      break;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      log("warn", `Attempt ${attempt}/${maxRetries} to reach daemon failed: ${message}`);

      if (attempt < maxRetries) {
        await delay(retryDelayMs);
      }
    }
  }

  if (agentSessions === null) {
    log("error", `Daemon unreachable after ${maxRetries} attempts, marking all tasks as missing`);

    // Daemon unreachable — transition all running tasks to failed
    const missing: Array<{ taskId: string }> = [];
    for (const task of runningTasks) {
      deps.onSessionMissing(task.id);
      missing.push({ taskId: task.id });
    }

    return { reattached: [], exited: [], missing, daemonReachable: false };
  }

  log("info", `Found ${agentSessions.length} agent sessions in daemon, matching against ${runningTasks.length} running tasks`);

  // Build a lookup map: taskId → session info
  const sessionByTaskId = new Map<string, SessionInfo>();
  for (const session of agentSessions) {
    if (session.agentMeta?.taskId) {
      sessionByTaskId.set(session.agentMeta.taskId, session);
    }
  }

  const reattached: Array<{ taskId: string; sessionId: string }> = [];
  const exited: Array<{ taskId: string; sessionId: string; exitCode: number }> = [];
  const missing: Array<{ taskId: string }> = [];

  for (const task of runningTasks) {
    const session = sessionByTaskId.get(task.id);

    if (!session) {
      // No matching session — task's agent is gone
      log("warn", `No daemon session found for task ${task.id}, marking as missing`);
      deps.onSessionMissing(task.id);
      missing.push({ taskId: task.id });
      continue;
    }

    if (session.state === "exited") {
      // Session already exited — report exit code
      // The exit code is not directly on SessionInfo but we can infer from
      // the session state. For exited sessions, we use a default of 1 (failure)
      // unless the session metadata provides more info.
      const resolvedExitCode = getExitCode(session);
      log("info", `Task ${task.id} session ${session.sessionId} already exited with code ${resolvedExitCode}`);
      deps.onSessionExited(task.id, resolvedExitCode);
      exited.push({ taskId: task.id, sessionId: session.sessionId, exitCode: resolvedExitCode });
      continue;
    }

    if (session.isAlive) {
      // Session is alive — reattach event stream
      try {
        await deps.createOrAttach(session.sessionId, cols, rows);
        log("info", `Reattached task ${task.id} to session ${session.sessionId}`);
        deps.onReattached(task.id, session.sessionId);
        reattached.push({ taskId: task.id, sessionId: session.sessionId });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        log("error", `Failed to reattach task ${task.id} to session ${session.sessionId}: ${message}`);
        deps.onSessionMissing(task.id);
        missing.push({ taskId: task.id });
      }
    } else {
      // Session exists but is not alive and not in "exited" state — treat as exited
      const resolvedExitCode = getExitCode(session);
      log("info", `Task ${task.id} session ${session.sessionId} is not alive (state: ${session.state}), exit code ${resolvedExitCode}`);
      deps.onSessionExited(task.id, resolvedExitCode);
      exited.push({ taskId: task.id, sessionId: session.sessionId, exitCode: resolvedExitCode });
    }
  }

  log("info", `Reattach complete: ${reattached.length} reattached, ${exited.length} exited, ${missing.length} missing`);
  return { reattached, exited, missing, daemonReachable: true };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/**
 * Extract exit code from a session. Since SessionInfo doesn't carry exitCode
 * directly, we infer from the state. Exited sessions default to exit code 1
 * (failure) unless we can determine otherwise.
 */
function getExitCode(_session: SessionInfo): number {
  // If the session has pid === null and state is "exited", it has terminated.
  // We don't have the exit code in SessionInfo directly, so we default to 1.
  // The daemon's stream socket would have delivered the actual exit code via
  // the exit event — this is a fallback for sessions that exited while the
  // app was disconnected.
  return 1;
}

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
