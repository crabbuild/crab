/**
 * Startup Reconciliation — kills orphaned PTY sessions on app launch.
 *
 * After the daemon is confirmed running and sessions are listed, this
 * module compares each session's worktree ID (extracted from the session
 * ID format `<worktreeId>@@<shortUuid>`) against the set of currently
 * valid worktree IDs. Sessions whose worktree no longer exists are
 * killed as orphans. Sessions with malformed IDs (not matching the
 * expected format) are also killed since they cannot be attributed to
 * any worktree.
 */

import type { SessionInfo } from "./types";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Separator between worktreeId and shortUuid in session IDs. */
export const SESSION_ID_SEPARATOR = "@@";

/** Separator between repoId and path within a worktreeId. */
const WORKTREE_ID_SEPARATOR = "::";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface ReconcileResult {
  /** Session IDs that are alive and belong to valid worktrees. */
  alive: string[];
  /** Session IDs that were killed (orphans or malformed). */
  killed: string[];
}

export interface ReconcileDeps {
  /** List all live sessions from the daemon. */
  listSessions: () => Promise<SessionInfo[]>;
  /** Kill a session by ID. Should not throw if already dead. */
  killSession: (sessionId: string) => Promise<void>;
  /** Optional logger. */
  log?: (level: "info" | "warn" | "error", message: string) => void;
}

// ---------------------------------------------------------------------------
// Session ID Parsing
// ---------------------------------------------------------------------------

/**
 * Parse a session ID to extract the worktree component.
 *
 * Valid format: `<worktreeId>@@<shortUuid>` where worktreeId itself
 * contains `::` (format: `<repoId>::<path>`).
 *
 * Returns `null` if the session ID does not match the expected format.
 */
export function parseSessionWorktreeId(sessionId: string): string | null {
  const idx = sessionId.lastIndexOf(SESSION_ID_SEPARATOR);
  if (idx <= 0) {
    return null;
  }

  const candidate = sessionId.slice(0, idx);

  // Require the worktreeId to contain `::` with non-empty halves
  const sepIdx = candidate.indexOf(WORKTREE_ID_SEPARATOR);
  if (sepIdx <= 0 || sepIdx + WORKTREE_ID_SEPARATOR.length >= candidate.length) {
    return null;
  }

  return candidate;
}

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

/**
 * Reconcile daemon sessions on startup.
 *
 * Iterates all live sessions, parses their session IDs to extract the
 * worktree component, and kills any session whose worktree is not in
 * the provided valid set or whose ID is malformed.
 *
 * @param validWorktreeIds - Set of worktree IDs that currently exist.
 * @param deps - Dependencies for listing and killing sessions.
 * @returns Summary of alive and killed sessions.
 */
export async function reconcileOnStartup(
  validWorktreeIds: Set<string>,
  deps: ReconcileDeps,
): Promise<ReconcileResult> {
  const { listSessions, killSession, log } = deps;

  const sessions = await listSessions();
  const alive: string[] = [];
  const killed: string[] = [];

  for (const session of sessions) {
    if (!session.isAlive) {
      continue;
    }

    const worktreeId = parseSessionWorktreeId(session.sessionId);

    if (worktreeId === null) {
      // Malformed session ID — cannot attribute to any worktree
      log?.("warn", `Killing session with malformed ID: ${session.sessionId}`);
      try {
        await killSession(session.sessionId);
      } catch {
        // Already dead — ignore
      }
      killed.push(session.sessionId);
    } else if (!validWorktreeIds.has(worktreeId)) {
      // Orphan — worktree no longer exists
      log?.("info", `Killing orphan session (worktree removed): ${session.sessionId}`);
      try {
        await killSession(session.sessionId);
      } catch {
        // Already dead — ignore
      }
      killed.push(session.sessionId);
    } else {
      alive.push(session.sessionId);
    }
  }

  log?.("info", `Reconciliation complete: ${alive.length} alive, ${killed.length} killed`);
  return { alive, killed };
}
