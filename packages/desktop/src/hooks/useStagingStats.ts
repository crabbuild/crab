import { useCallback, useEffect, useRef, useState } from "react";
import { rpc, StagingStats } from "../ipc";
import { useFileWatcher } from "./useFileWatcher";

export interface UseStagingStats {
  stats: StagingStats | null;
  loading: boolean;
  error: string | null;
  refresh: () => void;
}

/**
 * Subscribe to the repo's `.crab/staging` area. Refetches on mount,
 * on a 5-second interval, and on file watcher events touching the
 * staging tree — so the desktop reflects `crab add` runs from the
 * CLI without the user having to refresh manually.
 *
 * Staging failures remain visible through `error`, including repositories
 * that have not initialized a staging area.
 */
export function useStagingStats(repoId: string, repoPath: string | undefined): UseStagingStats {
  const [stats, setStats] = useState<StagingStats | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Re-entrant fetch guard. The watcher can fire bursts of events
  // when CDC chunking writes hundreds of segment files; we don't
  // want to launch a fresh RPC per event.
  const inFlightRef = useRef(false);

  const refresh = useCallback(() => {
    if (inFlightRef.current) return;
    inFlightRef.current = true;
    setLoading(true);
    setError(null);
    rpc
      .stagingStats(repoId)
      .then((s) => setStats(s))
      .catch((e: Error) => setError(e.message))
      .finally(() => {
        inFlightRef.current = false;
        setLoading(false);
      });
  }, [repoId]);

  // Initial fetch + slow polling for cases where the watcher misses
  // an event (network drives, FUSE mounts, etc.). 5 seconds matches
  // the existing SSH cache TTL so we don't double-poll over remote
  // connections.
  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 5_000);
    return () => clearInterval(id);
  }, [refresh]);

  // Refresh when files change inside the repo. The watcher already
  // debounces (300 ms) and we add a touch of additional debounce so
  // a `crab add foo.bin` writing many segment files only fires one
  // RPC.
  useFileWatcher(repoPath, refresh, 800);

  return { stats, loading, error, refresh };
}
