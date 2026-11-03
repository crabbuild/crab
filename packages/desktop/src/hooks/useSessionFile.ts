// Reports the currently active file path to the SessionManager whenever
// it changes. This is the write side of activeFile persistence — saving
// the file path as the user opens files so it can be restored on next
// activation.

import { useEffect, useRef } from "react";

import { useSession, useActiveRepoPath } from "../lib/session-context";

/**
 * Persists the active file path to the session whenever it changes.
 * Call once in the component that owns the file/tab state (RepoDetail).
 *
 * @param activeFile - The currently active file path, or null if no file is open.
 */
export function useSessionFile(activeFile: string | null): void {
  const sessionManager = useSession();
  const repoPath = useActiveRepoPath();
  const prevFileRef = useRef<string | null>(null);

  useEffect(() => {
    if (!repoPath) return;

    // Only write when the file actually changes (skip the initial
    // mount if the file was restored from session — avoids a
    // redundant write of the same value).
    if (prevFileRef.current === activeFile) return;
    prevFileRef.current = activeFile;

    sessionManager.update(repoPath, { activeFile: activeFile ?? undefined });
  }, [activeFile, repoPath, sessionManager]);
}
