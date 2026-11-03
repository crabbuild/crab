import { useCallback, useEffect, useMemo, useState } from "react";
import { HydrationEntry, HydrationStatusResult, HydrationSummary, rpc } from "../ipc";

export interface UseHydrationStatus {
  summary: HydrationSummary | null;
  byPath: Map<string, HydrationEntry>;
  refresh: () => void;
  loading: boolean;
  error: string | null;
}

/**
 * Shared hook for hydration-state decorations. Callers that care
 * about per-path state (the file tree) and callers that care only
 * about the repo-wide summary (the status bar) both subscribe to the
 * same snapshot — we fetch once per repo, then broadcast via the
 * returned state.
 *
 * The hook eagerly refreshes on mount and exposes `refresh()` for
 * callers that just ran a hydrate/dehydrate and want to see the new
 * state reflected immediately.
 */
export function useHydrationStatus(repoId: string, enabled = true): UseHydrationStatus {
  const [data, setData] = useState<HydrationStatusResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    if (!enabled) return;
    setLoading(true);
    setError(null);
    rpc
      .hydrationStatus(repoId)
      .then((d) => setData(d))
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [repoId, enabled]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const byPath = useMemo(() => {
    const m = new Map<string, HydrationEntry>();
    for (const e of data?.items ?? []) m.set(e.path, e);
    return m;
  }, [data]);

  return { summary: data?.summary ?? null, byPath, refresh, loading, error };
}
