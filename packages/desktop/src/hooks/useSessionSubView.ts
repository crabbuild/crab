// Persists sub-view state (selectedCommit, selectedBranch)
// to the SessionManager's viewState field.
// This is the write side — saving values as the user interacts so
// they can be restored on next activation.

import { useEffect, useRef } from "react";

import { useSession, useActiveRepoKey } from "../lib/session-context";

/**
 * Persists a sub-view state value to the session's viewState map.
 * Writes only when the value actually changes (skips redundant writes
 * on initial mount when the value was restored from session).
 *
 * @param key - The viewState key (e.g. "selectedCommit", "selectedBranch").
 * @param value - The current value to persist, or null/undefined to clear.
 */
export function useSessionSubView(key: string, value: string | null | undefined): void {
  const sessionManager = useSession();
  const repoPath = useActiveRepoKey();
  const prevValueRef = useRef<string | null | undefined>(value);

  useEffect(() => {
    if (!repoPath) return;

    // Only write when the value actually changes.
    if (prevValueRef.current === value) return;
    prevValueRef.current = value;

    // Read existing viewState to merge (avoid clobbering other keys).
    const session = sessionManager.activate(repoPath);
    const existingViewState = session?.viewState ?? {};

    sessionManager.update(repoPath, {
      viewState: { ...existingViewState, [key]: value ?? null },
    });
  }, [key, value, repoPath, sessionManager]);
}
