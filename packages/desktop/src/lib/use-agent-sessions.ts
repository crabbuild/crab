import { useCallback, useEffect, useState } from "react";

export interface AgentSessionInfo {
  sessionId: string;
  taskId: string;
  agentType: string;
  worktreePath: string;
  state: string;
  stateChangedAt: number;
  elapsedWorkingMs: number;
  exitCode?: number;
}

/**
 * Fetches agent sessions from the daemon and keeps them up to date.
 *
 * Calls `crab:agent-sessions` on mount and re-fetches whenever a
 * `crab:agent-state-changed` event arrives from the main process.
 *
 * Returns the current list of agent sessions and a manual refresh
 * function for callers that want to trigger an explicit reload.
 */
export function useAgentSessions(): {
  sessions: AgentSessionInfo[];
  loading: boolean;
  error: string | null;
  refresh: () => void;
} {
  const [sessions, setSessions] = useState<AgentSessionInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const fetchSessions = useCallback(async () => {
    try {
      const result = await window.crab.agentSessions();
      setSessions(result);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, []);

  // Fetch on mount.
  useEffect(() => {
    void fetchSessions();
  }, [fetchSessions]);

  // Re-fetch whenever an agent state change event arrives.
  useEffect(() => {
    if (!window.crab.onAgentStateChange) return;
    const off = window.crab.onAgentStateChange(() => {
      void fetchSessions();
    });
    return off;
  }, [fetchSessions]);

  return { sessions, loading, error, refresh: fetchSessions };
}
