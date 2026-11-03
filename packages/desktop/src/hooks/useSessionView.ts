// Reports the current view name to the SessionManager whenever it
// changes. This is the write side of view persistence — saving the
// active view as the user navigates so it can be restored on next
// activation.

import { useEffect, useRef } from "react";

import { useSession, useActiveRepoPath } from "../lib/session-context";

/**
 * Persists the active view name to the session whenever it changes.
 * Call once in the component that owns the view state (RepoDetail).
 *
 * @param viewName - The current sidebar view identifier (e.g. "explorer", "timeline").
 */
export function useSessionView(viewName: string): void {
  const sessionManager = useSession();
  const repoPath = useActiveRepoPath();
  const prevViewRef = useRef<string | null>(null);

  useEffect(() => {
    if (!repoPath) return;

    // Only write when the view actually changes (skip the initial
    // mount if the view was restored from session — avoids a
    // redundant write of the same value).
    if (prevViewRef.current === viewName) return;
    prevViewRef.current = viewName;

    sessionManager.update(repoPath, { activeView: viewName });
  }, [viewName, repoPath, sessionManager]);
}
