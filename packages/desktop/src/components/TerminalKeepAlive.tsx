import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from "react";
import { TerminalView, type SshTerminalRequest, type TerminalStartupRequest } from "../views/TerminalView";
import type { OpenRepo } from "../state";

// --- Memory API typing (Chrome/Electron only) ---

interface PerformanceMemory {
  usedJSHeapSize: number;
  totalJSHeapSize: number;
  jsHeapSizeLimit: number;
}

interface PerformanceWithMemory extends Performance {
  memory?: PerformanceMemory;
}

// --- Constants ---

/** Interval (ms) between memory pressure checks. */
const MEMORY_CHECK_INTERVAL_MS = 30_000;

/** Heap usage ratio above which we start evicting terminals. */
const MEMORY_PRESSURE_THRESHOLD = 0.8;

// --- Types ---

export interface TerminalKeepAliveProps {
  startupRequest?: TerminalStartupRequest | null;
  onStartupRequestHandled?: () => void;
  sshRequest?: SshTerminalRequest | null;
  onSshRequestHandled?: () => void;
  /** All open repos in the store. */
  repos: OpenRepo[];
  /** The currently active repo ID (or null if none). */
  activeRepoId: string | null;
  /** Maximum number of repos whose terminals are kept alive simultaneously. */
  maxKeptAlive: number;
  /** Optional callback forwarded to the active TerminalView. */
  onSendErrorToAgent?: (feedback: string) => void;
}

export interface TerminalKeepAliveHandle {
  /** Add a repo to the keep-alive set (promotes it to MRU). */
  addToKeepAlive: (repoId: string) => void;
  /** Evict a repo from the keep-alive set (unmounts its TerminalView). */
  evict: (repoId: string) => void;
  /**
   * Pin a repo so its terminal is never evicted by LRU or memory pressure.
   * Used for repos with active daemon/agent sessions that must survive
   * across task switches.
   */
  pin: (repoId: string) => void;
  /** Unpin a repo, making it eligible for LRU eviction again. */
  unpin: (repoId: string) => void;
  /**
   * Returns the repo paths that currently have kept-alive terminals.
   * Used by WorkspaceSessionManager to persist terminal state.
   */
  getKeptAliveRepoPaths: () => string[];
}

/** Keeps a bounded set of terminal renderers keyed by repository path.
 * Eviction releases UI resources; durable sessions and layouts remain recoverable.
 * Repository IDs change on workspace reopen, so they cannot own terminal lifetime.
 */
export const TerminalKeepAlive = forwardRef<TerminalKeepAliveHandle, TerminalKeepAliveProps>(
  function TerminalKeepAlive({ repos, activeRepoId, maxKeptAlive, onSendErrorToAgent, sshRequest, onSshRequestHandled, startupRequest, onStartupRequestHandled }, ref) {
    // LRU order: index 0 = most recently used, last = least recently used.
    const [lruOrder, setLruOrder] = useState<string[]>([]);

    // Pinned repos — these are never evicted by LRU or memory pressure.
    // Typically repos with active daemon/agent sessions.
    const [pinnedRepos, setPinnedRepos] = useState<Set<string>>(new Set());

    // Cache of repo objects — keeps repo references alive even after they
    // leave state.repos (e.g., during workspace switches). This prevents
    // terminals from unmounting when their workspace is suspended.
    const repoCacheRef = useRef<Map<string, OpenRepo>>(new Map());

    // Keep a ref to lruOrder for use in imperative methods without stale closures.
    const lruOrderRef = useRef(lruOrder);
    lruOrderRef.current = lruOrder;

    // Update the repo cache whenever repos change — always keep the latest
    // version of each repo object, and retain entries for suspended workspaces.
    useEffect(() => {
      for (const repo of repos) {
        repoCacheRef.current.set(repo.path ?? repo.repo_id, repo);
      }
    }, [repos]);

    const repoKey = useCallback((repoId: string) => {
      const repo = repos.find(repo => repo.repo_id === repoId)
        ?? [...repoCacheRef.current.values()].find(repo => repo.repo_id === repoId);
      return repo?.path ?? repoId;
    }, [repos]);

    /**
     * Promote a repo to the front of the LRU list. If it's not in the
     * list yet, prepend it. If it's already present, move it to front.
     */
    const addToKeepAlive = useCallback((id: string) => {
      const repoId = repoKey(id);
      setLruOrder((prev) => {
        if (prev[0] === repoId) return prev; // already MRU
        const without = prev.filter((id) => id !== repoId);
        return [repoId, ...without];
      });
    }, [repoKey]);

    /**
     * Remove a repo from the keep-alive set entirely. This unmounts
     * its TerminalView, triggering normal PTY cleanup.
     */
    const evict = useCallback((id: string) => {
      const repoId = repoKey(id);
      setLruOrder((prev) => prev.filter((id) => id !== repoId));
      setPinnedRepos((prev) => {
        if (!prev.has(repoId)) return prev;
        const next = new Set(prev);
        next.delete(repoId);
        return next;
      });
      repoCacheRef.current.delete(repoId);
    }, [repoKey]);

    /**
     * Pin a repo so its terminal is never evicted by LRU or memory pressure.
     */
    const pin = useCallback((id: string) => {
      const repoId = repoKey(id);
      setPinnedRepos((prev) => {
        if (prev.has(repoId)) return prev;
        const next = new Set(prev);
        next.add(repoId);
        return next;
      });
    }, [repoKey]);

    /**
     * Unpin a repo, making it eligible for LRU eviction again.
     */
    const unpin = useCallback((id: string) => {
      const repoId = repoKey(id);
      setPinnedRepos((prev) => {
        if (!prev.has(repoId)) return prev;
        const next = new Set(prev);
        next.delete(repoId);
        return next;
      });
    }, [repoKey]);

    /**
     * Returns the repo paths that currently have kept-alive terminals.
     */
    const getKeptAliveRepoPaths = useCallback((): string[] => {
      const paths: string[] = [];
      for (const repoId of lruOrderRef.current) {
        const repo = repoCacheRef.current.get(repoId);
        if (repo?.path) {
          paths.push(repo.path);
        }
      }
      return paths;
    }, []);

    // Expose imperative methods via ref.
    useImperativeHandle(ref, () => ({
      addToKeepAlive, evict, pin, unpin,
      getKeptAliveRepoPaths,
    }), [addToKeepAlive, evict, pin, unpin, getKeptAliveRepoPaths]);

    // When activeRepoId changes and is non-null, promote it to MRU.
    useEffect(() => {
      if (activeRepoId) {
        addToKeepAlive(activeRepoId);
      }
    }, [activeRepoId, addToKeepAlive]);

    useEffect(() => {
      if (startupRequest && activeRepoId) pin(activeRepoId);
    }, [startupRequest, activeRepoId, pin]);

    // Track the effective cap — starts at maxKeptAlive but may be reduced
    // by memory pressure.
    const [effectiveCap, setEffectiveCap] = useState(maxKeptAlive);

    // Reset effective cap when the user-configured max changes.
    useEffect(() => {
      setEffectiveCap(maxKeptAlive);
    }, [maxKeptAlive]);

    // Memory guard: check heap usage every 30s and reduce the effective
    // cap if memory pressure is detected (Chrome/Electron only).
    useEffect(() => {
      const perf = performance as PerformanceWithMemory;
      if (!perf.memory) return; // API not available — rely on static cap

      const intervalId = setInterval(() => {
        const mem = perf.memory;
        if (!mem) return;

        const usage = mem.usedJSHeapSize / mem.jsHeapSizeLimit;
        if (usage > MEMORY_PRESSURE_THRESHOLD) {
          setEffectiveCap((prev) => {
            const next = Math.max(1, prev - 1);
            if (next < prev) {
              console.warn(
                `[TerminalKeepAlive] Memory pressure detected ` +
                  `(${(usage * 100).toFixed(1)}% heap used). ` +
                  `Reducing keep-alive cap from ${prev} to ${next} and evicting oldest unpinned terminal.`,
              );
            }
            return next;
          });
        }
      }, MEMORY_CHECK_INTERVAL_MS);

      return () => clearInterval(intervalId);
    }, []);

    // Enforce the cap: keep all pinned repos plus the first `effectiveCap`
    // unpinned entries from the LRU list. Pinned repos are never evicted.
    const keptAlive = (() => {
      const pinned: string[] = [];
      const unpinned: string[] = [];
      for (const repoId of lruOrder) {
        if (pinnedRepos.has(repoId)) {
          pinned.push(repoId);
        } else {
          unpinned.push(repoId);
        }
      }
      // Keep all pinned + up to effectiveCap unpinned (in LRU order)
      return [...pinned, ...unpinned.slice(0, effectiveCap)];
    })();

    // If trimming happened, update state so evicted entries are truly gone.
    // We compare lengths to avoid running on every render.
    const unpinnedCount = lruOrder.filter((id) => !pinnedRepos.has(id)).length;
    useEffect(() => {
      if (unpinnedCount > effectiveCap) {
        setLruOrder((prev) => {
          const pinned: string[] = [];
          const unpinned: string[] = [];
          for (const id of prev) {
            if (pinnedRepos.has(id)) {
              pinned.push(id);
            } else {
              unpinned.push(id);
            }
          }
          return [...pinned, ...unpinned.slice(0, effectiveCap)];
        });
      }
      // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [unpinnedCount, effectiveCap, pinnedRepos]);

    return (
      <>
        {keptAlive.map((repoId) => {
          // Look up repo from the live repos first, fall back to cache
          // (for terminals from suspended workspaces).
          const repo = repos.find((r) => (r.path ?? r.repo_id) === repoId)
            ?? repoCacheRef.current.get(repoId);
          if (!repo) return null;

          const isActive = activeRepoId !== null && repoId === repoKey(activeRepoId);
          return (
            <div
              key={repoId}
              style={{ display: isActive ? "contents" : "none" }}
            >
              <TerminalView
                repo={repo}
                persistenceKey={repo.path ?? repo.repo_id}
                visible={isActive}
                startupRequest={isActive ? startupRequest : null}
                onStartupRequestHandled={onStartupRequestHandled}
                sshRequest={isActive ? sshRequest : null}
                onSshRequestHandled={onSshRequestHandled}
                onSendErrorToAgent={isActive ? onSendErrorToAgent : undefined}
              />
            </div>
          );
        })}
      </>
    );

  },
);
