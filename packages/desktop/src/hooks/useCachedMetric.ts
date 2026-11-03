import { useEffect, useRef, useState } from "react";

/**
 * Stale-while-revalidate hook for expensive dashboard metrics.
 *
 * On mount:
 * 1. If cached data exists in localStorage, return it immediately (no flash).
 * 2. Fire the RPC in the background.
 * 3. When fresh data arrives, update state and persist to localStorage.
 *
 * Cache key format: `crab.dashboard.cache.${repoId}.${metric}`
 */
export function useCachedMetric<T>(
  repoId: string,
  metric: string,
  fetcher: () => Promise<T>,
  enabled = true,
): { data: T | null; loading: boolean; stale: boolean } {
  const cacheKey = `crab.dashboard.cache.${repoId}.${metric}`;

  const [data, setData] = useState<T | null>(() => {
    if (!enabled) return null;
    try {
      const raw = localStorage.getItem(cacheKey);
      if (raw) return JSON.parse(raw) as T;
    } catch { /* ignore parse errors */ }
    return null;
  });

  const [loading, setLoading] = useState(!data);
  const [stale, setStale] = useState(!!data);
  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;

  useEffect(() => {
    if (!enabled) return;

    // Reset data to cached value for the new key (or null if no cache).
    try {
      const raw = localStorage.getItem(cacheKey);
      if (raw) {
        setData(JSON.parse(raw) as T);
        setStale(true);
      } else {
        setData(null);
      }
    } catch {
      setData(null);
    }

    let cancelled = false;
    setLoading(true);

    fetcherRef.current()
      .then((fresh) => {
        if (cancelled) return;
        setData(fresh);
        setStale(false);
        setLoading(false);
        try {
          localStorage.setItem(cacheKey, JSON.stringify(fresh));
        } catch { /* quota exceeded — ignore */ }
      })
      .catch(() => {
        if (cancelled) return;
        // Keep stale data if available; just stop loading.
        setLoading(false);
      });

    return () => { cancelled = true; };
  }, [cacheKey, enabled]);

  return { data, loading, stale };
}
