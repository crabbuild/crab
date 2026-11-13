import { useCallback, useSyncExternalStore } from "react";
import { isStorageRepositoryUrl, type HydrationEntry, type HydrationSummary } from "../ipc";
import type { OpenRepo } from "../state";
import { hydrationStatusStore } from "../lib/hydration-status-store";

export interface UseHydrationStatus {
  summary: HydrationSummary | null;
  byPath: Map<string, HydrationEntry>;
  refresh: () => void;
  loading: boolean;
  error: string | null;
  storageSnapshot: boolean;
}

/**
 * Shared hook for hydration-state decorations. Callers that care
 * about per-path state (the file tree) and callers that care only
 * about the repo-wide summary (the status bar) both subscribe to the
 * same snapshot — we fetch once per repo, then broadcast via the
 * returned state.
 *
 * The hook eagerly refreshes on mount and exposes `refresh()` for
 * explicit reads. Native hydrate/dehydrate completion refreshes every
 * subscribed view; the repository watcher invalidates this same owner.
 */
export function useHydrationStatus(repo: OpenRepo | null): UseHydrationStatus {
  const repoId = repo?.repo_id ?? "";
  // Storage snapshots have no worktree inventory. SSH worktrees still do;
  // neither is_remote nor mutation capability flags distinguish these modes.
  const storageSnapshot = !!repo && isStorageRepositoryUrl(repo.path ?? repo.url);
  const enabled = !!repo && !repo.deferred && !storageSnapshot;
  const subscribe = useCallback((listener: () => void) => enabled ? hydrationStatusStore.subscribe(repoId, listener) : () => undefined, [repoId, enabled]);
  const snapshot = useCallback(() => hydrationStatusStore.snapshot(enabled ? repoId : undefined), [repoId, enabled]);
  const data = useSyncExternalStore(subscribe, snapshot);
  const refresh = useCallback(() => {
    if (enabled) hydrationStatusStore.refresh(repoId);
  }, [repoId, enabled]);
  return { ...data, refresh, storageSnapshot };
}
