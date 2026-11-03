import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import {
  Copy,
  FlaskConical,
  GitBranch,
  GitCommitHorizontal,
  GitMerge,
  Loader2,
  LogOut,
  RefreshCw,
  RotateCcw,
  Search,
  Undo2,
  X,
} from "lucide-react";
import { useVirtualizer } from "@tanstack/react-virtual";
import { OpenRepo } from "../state";
import { CiCheck, CommitRef, CommitRow, ExperimentRun, ExperimentRunStatus, rpc, gitLogCancellable, isCancelledError } from "../ipc";
import {
  CommitGraphSvg,
  graphWidth,
  ROW_HEIGHT,
} from "../components/CommitGraph";
import { computeTrunk } from "../lib/commit-graph";
import { useCommitGraphAsync } from "../hooks/useCommitGraphAsync";
import { CommitPopover } from "../components/CommitPopover";
import { CiStatusChip } from "../components/CiStatusChip";
import { executeCommand } from "../lib/command-registry";
import type { CommandContext } from "../lib/command-registry";
import { useFileWatcher, getWatchPath } from "../hooks/useFileWatcher";
import { useSessionScroll } from "../hooks/useSessionScroll";

// ── Experiment run chips in Timeline ────────────────────────────────

const RUN_STATUS_COLOR: Record<ExperimentRunStatus, string> = {
  running: "text-gx-info",
  completed: "text-gx-success",
  failed: "text-gx-error",
  killed: "text-gx-warn",
  unknown: "text-gx-muted",
};

/**
 * Lazily fetches experiment runs for a set of commit SHAs.
 * Returns a Map from SHA → runs. Silently skips if the provider
 * isn't configured (RPC errors are swallowed).
 *
 * The map is capped at 500 entries to prevent unbounded memory growth
 * when the user scrolls through a long timeline.
 */
function useExperimentRunsByCommit(
  repoId: string,
  shas: string[],
): Map<string, ExperimentRun[]> {
  const [runMap, setRunMap] = useState<Map<string, ExperimentRun[]>>(new Map());
  const fetchedRef = useRef<Set<string>>(new Set());
  // Stop firing requests after a streak of failures so an unconfigured
  // experiment provider on a 30 k-commit timeline can't queue thousands
  // of doomed RPCs.
  const disabledRef = useRef(false);

  // Cap prevents memory from growing unbounded as the user scrolls.
  const MAX_CACHE_ENTRIES = 500;

  useEffect(() => {
    if (!shas.length || disabledRef.current) return;

    // Only fetch SHAs we haven't already fetched.
    const toFetch = shas.filter((sha) => !fetchedRef.current.has(sha));
    if (!toFetch.length) return;

    // Mark as fetched immediately to avoid duplicate requests.
    for (const sha of toFetch) {
      fetchedRef.current.add(sha);
    }
    // Evict old entries from fetchedRef to prevent unbounded Set growth.
    if (fetchedRef.current.size > MAX_CACHE_ENTRIES * 2) {
      const entries = [...fetchedRef.current];
      fetchedRef.current = new Set(entries.slice(-MAX_CACHE_ENTRIES));
    }

    let cancelled = false;

    // Fetch in background — don't block the timeline render.
    void (async () => {
      const results = new Map<string, ExperimentRun[]>();
      // Batch in small groups to avoid flooding the agent.
      const BATCH = 10;
      let consecutiveFailures = 0;
      for (let i = 0; i < toFetch.length; i += BATCH) {
        if (cancelled || disabledRef.current) return;
        const batch = toFetch.slice(i, i + BATCH);
        const settled = await Promise.allSettled(
          batch.map((sha) =>
            rpc.experimentsRunsForCommit(repoId, sha).then((r) => ({ sha, runs: r.runs })),
          ),
        );
        let batchFailed = 0;
        for (const result of settled) {
          if (result.status === "fulfilled") {
            consecutiveFailures = 0;
            if (result.value.runs.length > 0) {
              results.set(result.value.sha, result.value.runs);
            }
          } else {
            batchFailed += 1;
          }
        }
        if (batchFailed === settled.length) {
          consecutiveFailures += batchFailed;
          if (consecutiveFailures >= BATCH * 2) {
            disabledRef.current = true;
            return;
          }
        }
      }
      if (cancelled) return;
      if (results.size > 0) {
        setRunMap((prev) => {
          const next = new Map(prev);
          for (const [sha, runs] of results) {
            next.set(sha, runs);
          }
          // Evict oldest entries if over cap.
          if (next.size > MAX_CACHE_ENTRIES) {
            const keys = [...next.keys()];
            for (let i = 0; i < keys.length - MAX_CACHE_ENTRIES; i++) {
              next.delete(keys[i]);
            }
          }
          return next;
        });
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [repoId, shas]);

  return runMap;
}

/**
 * Lazily fetches CI checks for a set of commit SHAs.
 * Returns a Map from SHA → checks. Silently skips if the forge
 * isn't configured (RPC errors are swallowed).
 *
 * The map is capped at 500 entries to prevent unbounded memory growth
 * when the user scrolls through a long timeline. Oldest entries are
 * evicted when the cap is exceeded.
 */
function useCiChecksByCommit(
  repoId: string,
  shas: string[],
): Map<string, CiCheck[]> {
  const [checkMap, setCheckMap] = useState<Map<string, CiCheck[]>>(new Map());
  const fetchedRef = useRef<Set<string>>(new Set());
  // Once we hit a string of failures (e.g. rate-limited GitHub API
  // for an unauthenticated request), stop firing more requests for
  // this repo session. Without this, scrolling a 30 k-commit timeline
  // queues thousands of doomed HTTP calls and accumulates their
  // response bodies in memory while they fail.
  const disabledRef = useRef(false);

  // Cap prevents memory from growing unbounded as the user scrolls.
  const MAX_CACHE_ENTRIES = 500;

  useEffect(() => {
    if (!shas.length || disabledRef.current) return;

    // Only fetch SHAs we haven't already fetched.
    const toFetch = shas.filter((sha) => !fetchedRef.current.has(sha));
    if (!toFetch.length) return;

    // Mark as fetched immediately to avoid duplicate requests.
    for (const sha of toFetch) {
      fetchedRef.current.add(sha);
    }
    // Evict old entries from fetchedRef to prevent unbounded Set growth.
    if (fetchedRef.current.size > MAX_CACHE_ENTRIES * 2) {
      const entries = [...fetchedRef.current];
      fetchedRef.current = new Set(entries.slice(-MAX_CACHE_ENTRIES));
    }

    let cancelled = false;

    // Fetch in background — don't block the timeline render.
    // Batch in small groups to avoid flooding the agent.
    void (async () => {
      const results = new Map<string, CiCheck[]>();
      const BATCH = 10;
      let consecutiveFailures = 0;
      for (let i = 0; i < toFetch.length; i += BATCH) {
        if (cancelled || disabledRef.current) return;
        const batch = toFetch.slice(i, i + BATCH);
        const settled = await Promise.allSettled(
          batch.map((sha) =>
            rpc.forgeListChecks(repoId, sha).then((r) => ({ sha, checks: r.checks })),
          ),
        );
        let batchFailed = 0;
        for (const result of settled) {
          if (result.status === "fulfilled") {
            consecutiveFailures = 0;
            if (result.value.checks.length > 0) {
              results.set(result.value.sha, result.value.checks);
            }
          } else {
            batchFailed += 1;
          }
        }
        if (batchFailed === settled.length) {
          consecutiveFailures += batchFailed;
          // Two full failed batches in a row → forge is unavailable
          // (no auth, rate limited, offline). Bail out of this run.
          if (consecutiveFailures >= BATCH * 2) {
            disabledRef.current = true;
            return;
          }
        }
      }
      if (cancelled) return;
      if (results.size > 0) {
        setCheckMap((prev) => {
          const next = new Map(prev);
          for (const [sha, checks] of results) {
            next.set(sha, checks);
          }
          // Evict oldest entries if over cap.
          if (next.size > MAX_CACHE_ENTRIES) {
            const keys = [...next.keys()];
            for (let i = 0; i < keys.length - MAX_CACHE_ENTRIES; i++) {
              next.delete(keys[i]);
            }
          }
          return next;
        });
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [repoId, shas]);

  return checkMap;
}

/** Small chip showing experiment run status next to a commit. */
function RunChip({ runs }: { runs: ExperimentRun[] }) {
  if (!runs.length) return null;

  // Pick the "most important" status for the chip color:
  // running > failed > completed > killed > unknown
  const priority: ExperimentRunStatus[] = ["running", "failed", "completed", "killed", "unknown"];
  const dominantStatus =
    priority.find((s) => runs.some((r) => r.status === s)) ?? "unknown";
  const color = RUN_STATUS_COLOR[dominantStatus];
  const count = runs.length;
  const title = runs.map((r) => `${r.name} (${r.status})`).join(", ");

  return (
    <span
      className={clsx(
        "inline-flex shrink-0 items-center gap-0.5 rounded-full border border-current/20 px-1 py-[1px] text-[9px] font-medium opacity-80 hover:opacity-100",
        color,
      )}
      title={title}
    >
      <FlaskConical size={9} />
      {count > 1 && <span>{count}</span>}
    </span>
  );
}

interface Props {
  repo: OpenRepo;
  selectedSha: string | null;
  onSelectCommit: (sha: string | null) => void;
  onCommitsLoaded?: (commits: CommitRow[]) => void;
  /** Increment to trigger a re-fetch of the timeline (e.g. after a commit). */
  refreshKey?: number;
}

interface ContextMenuState {
  /** Position in viewport coordinates. */
  x: number;
  y: number;
  commit: CommitRow;
}

/**
 * Sidebar timeline: branch graph + commit list + find widget +
 * commit popover + lightweight context menu.
 *
 * Each row paints its own graph cell inline so alignment is
 * guaranteed by construction. Hover (after a short dwell) opens a
 * popover with extra commit context; right-click opens a read-only
 * context menu (Copy sha / subject / branch names).
 *
 * Cmd/Ctrl+F opens the Find widget which highlights matches and
 * supports Enter / Shift+Enter to step through them.
 */
export function TimelineView({
  repo,
  selectedSha,
  onSelectCommit,
  onCommitsLoaded,
  refreshKey,
}: Props) {
  const [commits, setCommits] = useState<CommitRow[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [authorFilter, setAuthorFilter] = useState("");
  const [subjectFilter, setSubjectFilter] = useState("");
  const [findOpen, setFindOpen] = useState(false);
  const [findQuery, setFindQuery] = useState("");
  const [findIdx, setFindIdx] = useState(0);
  const [hover, setHover] = useState<{ commit: CommitRow; rect: DOMRect } | null>(
    null,
  );
  const [menu, setMenu] = useState<ContextMenuState | null>(null);
  const [toast, setToast] = useState<{ kind: "info" | "error"; text: string } | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const sessionScrollRef = useSessionScroll("timeline");
  const mergedScrollRef = useCallback(
    (node: HTMLDivElement | null) => {
      scrollRef.current = node;
      // Assign to the session scroll ref (RefObject has readonly .current
      // but we need to set it from the callback ref pattern).
      (sessionScrollRef as React.MutableRefObject<HTMLDivElement | null>).current = node;
    },
    [sessionScrollRef],
  );
  const hoverTimerRef = useRef<number | null>(null);
  const findInputRef = useRef<HTMLInputElement | null>(null);

  const [total, setTotal] = useState<number | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  // Initial page is 200 commits — enough to fill any screen many times
  // over. The full history is available via "Load more". Keeping this
  // small cuts initial memory, graph layout time, and IPC payload size.
  // For repos with 30k+ commits (like codex), even 500 was too much
  // when combined with the graph layout and SVG rendering overhead.
  const PAGE_SIZE = 200;
  // Hard cap on total commits held in memory. Beyond this the graph
  // layout's O(n × lanes) memory cost and the SVG path string size
  // risk OOM in Electron's renderer. Users can still search the full
  // history via the find widget (which queries the backend).
  const MAX_LOADED_COMMITS = 2000;

  // Cancel the previous in-flight gitLog call when a new refresh fires.
  // Without this, rapid FS events (e.g. `git fetch` updating many refs)
  // cause concurrent gitLog responses to pile up in memory before GC
  // can collect the previous CommitRow arrays — leading to OOM.
  const cancelRef = useRef<(() => void) | null>(null);

  const refresh = useCallback(() => {
    if (repo.deferred) {
      // Repo hasn't been opened via the agent yet — skip the RPC.
      return;
    }
    // Cancel any in-flight request to prevent pileup.
    if (cancelRef.current) {
      console.debug(`[timeline] cancelling previous git_log for ${repo.repo_id}`);
      cancelRef.current();
    }
    cancelRef.current = null;
    setLoading(true);
    setErr(null);
    const t0 = performance.now();
    const { promise, cancel } = gitLogCancellable(repo.repo_id, { max: PAGE_SIZE, all: true });
    cancelRef.current = cancel;
    promise
      .then((r) => {
        const elapsed = performance.now() - t0;
        console.debug(
          `[timeline] git_log resolved: ${r.commits.length} commits, ` +
          `total=${r.total}, took ${elapsed.toFixed(0)}ms`
        );
        setCommits(r.commits);
        setTotal(r.total ?? null);
        onCommitsLoaded?.(r.commits);
      })
      .catch((e: unknown) => {
        if (!isCancelledError(e)) {
          console.error(`[timeline] git_log error:`, e);
          setErr(e instanceof Error ? e.message : String(e));
        } else {
          console.debug(`[timeline] git_log cancelled after ${(performance.now() - t0).toFixed(0)}ms`);
        }
      })
      .finally(() => {
        setLoading(false);
        // Clear ref after completion so stale cancel isn't called on unmount.
        cancelRef.current = null;
      });
  }, [repo.repo_id, repo.deferred]);

  const loadMore = useCallback(() => {
    if (!commits || loadingMore) return;
    // Don't load beyond the safety cap.
    if (commits.length >= MAX_LOADED_COMMITS) return;
    setLoadingMore(true);
    const remaining = MAX_LOADED_COMMITS - commits.length;
    const fetchSize = Math.min(PAGE_SIZE, remaining);
    rpc
      .gitLog(repo.repo_id, { max: fetchSize, skip: commits.length, all: true })
      .then((r) => {
        const merged = [...(commits ?? []), ...r.commits];
        setCommits(merged);
        setTotal(r.total ?? null);
        onCommitsLoaded?.(merged);
      })
      .catch(() => undefined)
      .finally(() => setLoadingMore(false));
  }, [repo.repo_id, commits, loadingMore, onCommitsLoaded]);

  useEffect(() => {
    refresh();
    return () => {
      // Cancel in-flight request on unmount or when refresh identity changes.
      cancelRef.current?.();
      cancelRef.current = null;
    };
  }, [refresh]);

  // Re-fetch when the parent signals a change (e.g. after a commit).
  const prevRefreshKey = useRef(refreshKey);
  useEffect(() => {
    if (refreshKey != null && refreshKey !== prevRefreshKey.current) {
      prevRefreshKey.current = refreshKey;
      refresh();
    }
  }, [refreshKey, refresh]);

  // Auto-refresh timeline when git refs change on disk (new commits,
  // branch operations, etc.). Uses a long debounce because fetching
  // and laying out the commit graph is expensive on large repos —
  // a burst of FS events (e.g. `git fetch` updating thousands of
  // remote-tracking refs) must coalesce into a single refresh, or
  // the renderer drowns in graph-layout work.
  useFileWatcher(getWatchPath(repo), refresh, 2000);

  const filtered = useMemo(() => {
    if (!commits) return [];
    const a = authorFilter.trim().toLowerCase();
    const s = subjectFilter.trim().toLowerCase();
    if (!a && !s) return commits;
    return commits.filter(
      (c) =>
        (!a || c.author.toLowerCase().includes(a)) &&
        (!s || c.subject.toLowerCase().includes(s)),
    );
  }, [commits, authorFilter, subjectFilter]);

  // Find matches. Search across subject / author / sha / ref names.
  const findMatches = useMemo(() => {
    if (!findOpen || !findQuery.trim() || !filtered.length) return [];
    const q = findQuery.trim().toLowerCase();
    const matches: number[] = [];
    for (let i = 0; i < filtered.length; i++) {
      const c = filtered[i];
      if (
        c.subject.toLowerCase().includes(q) ||
        c.author.toLowerCase().includes(q) ||
        c.sha.toLowerCase().startsWith(q) ||
        (c.refs ?? []).some((r) => r.name.toLowerCase().includes(q))
      ) {
        matches.push(i);
      }
    }
    return matches;
  }, [findOpen, findQuery, filtered]);

  // Graph layout runs in a Web Worker so the main thread stays
  // responsive while computing. `graphData` is null until the first
  // result arrives — the list renders immediately without the gutter,
  // then the SVG appears once the worker finishes (~50–200 ms).
  const { graphData, computing: graphComputing } = useCommitGraphAsync(filtered);
  // Always reserve at least 40px for the graph gutter so commit rows
  // don't jump left when the graph finishes computing.
  const gutterWidth = useMemo(
    () => (graphData ? Math.max(graphWidth(graphData), 40) : 40),
    [graphData],
  );
  const trunk = useMemo(() => computeTrunk(filtered), [filtered]);

  // Track which SHAs are currently visible in the virtualised list.
  // Experiment run and CI check fetches are scoped to visible rows so
  // we don't fire hundreds of RPCs for off-screen commits on mount.
  const [visibleShas, setVisibleShas] = useState<string[]>([]);
  const experimentRunMap = useExperimentRunsByCommit(repo.repo_id, visibleShas);

  // Lazily fetch CI checks for visible commits.
  const ciCheckMap = useCiChecksByCommit(repo.repo_id, visibleShas);

  // Tip/tag sets for the graph renderer — keyed by row index (string).
  const { tipShas, tagShas } = useMemo(() => {
    const tips = new Set<string>();
    const tags = new Set<string>();
    for (let i = 0; i < filtered.length; i++) {
      for (const r of filtered[i].refs ?? []) {
        if (r.kind === "branch" || r.kind === "head" || r.kind === "head_target" || r.kind === "remote") {
          tips.add(String(i));
        }
        if (r.kind === "tag") {
          tips.add(String(i));
          tags.add(String(i));
        }
      }
    }
    return { tipShas: tips, tagShas: tags };
  }, [filtered]);

  const selectedIdx = selectedSha
    ? filtered.findIndex((c) => c.sha === selectedSha)
    : -1;

  const onKey = useCallback(
    (e: React.KeyboardEvent) => {
      if (!filtered.length) return;
      if (e.key === "j" || e.key === "ArrowDown") {
        e.preventDefault();
        const next = Math.min(
          selectedIdx < 0 ? 0 : selectedIdx + 1,
          filtered.length - 1,
        );
        onSelectCommit(filtered[next].sha);
      } else if (e.key === "k" || e.key === "ArrowUp") {
        e.preventDefault();
        const prev = Math.max(selectedIdx <= 0 ? 0 : selectedIdx - 1, 0);
        onSelectCommit(filtered[prev].sha);
      }
    },
    [filtered, selectedIdx, onSelectCommit],
  );

  // Scroll-to-selected when selection changes from outside.
  useEffect(() => {
    if (selectedIdx < 0 || !scrollRef.current) return;
    const container = scrollRef.current;
    const top = selectedIdx * ROW_HEIGHT;
    const bottom = top + ROW_HEIGHT;
    if (top < container.scrollTop) container.scrollTop = top;
    else if (bottom > container.scrollTop + container.clientHeight) {
      container.scrollTop = bottom - container.clientHeight;
    }
  }, [selectedIdx]);

  // Global Cmd/Ctrl+F — open find widget when the timeline is
  // visible. Restricted to when a commit list is present so we
  // don't grab the shortcut from other views.
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === "f") {
        e.preventDefault();
        setFindOpen(true);
        // Defer the focus so the input is already in the DOM.
        requestAnimationFrame(() => findInputRef.current?.focus());
      } else if (e.key === "Escape") {
        if (findOpen) {
          e.preventDefault();
          setFindOpen(false);
        } else if (menu) {
          setMenu(null);
        }
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [findOpen, menu]);

  // Navigate find matches when user hits Enter / Shift+Enter.
  const gotoMatch = useCallback(
    (i: number) => {
      if (!findMatches.length) return;
      const normalised = ((i % findMatches.length) + findMatches.length) % findMatches.length;
      setFindIdx(normalised);
      const target = findMatches[normalised];
      if (target == null) return;
      onSelectCommit(filtered[target].sha);
    },
    [findMatches, filtered, onSelectCommit],
  );

  useEffect(() => {
    if (!findOpen) return;
    setFindIdx(0);
  }, [findOpen, findQuery, findMatches.length]);

  // Hover → popover, with a short dwell to avoid flicker while
  // scrolling or mousing past rows.
  const openHover = useCallback(
    (commit: CommitRow, rect: DOMRect) => {
      if (hoverTimerRef.current) window.clearTimeout(hoverTimerRef.current);
      hoverTimerRef.current = window.setTimeout(() => {
        setHover({ commit, rect });
      }, 280);
    },
    [],
  );
  const closeHover = useCallback(() => {
    if (hoverTimerRef.current) {
      window.clearTimeout(hoverTimerRef.current);
      hoverTimerRef.current = null;
    }
    setHover(null);
  }, []);

  // Close popover + menu when the container scrolls.
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const onScroll = () => {
      setHover(null);
      setMenu(null);
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    return () => el.removeEventListener("scroll", onScroll);
  }, []);

  // Click anywhere else → close context menu.
  useEffect(() => {
    if (!menu) return;
    const onClick = () => setMenu(null);
    window.addEventListener("click", onClick);
    return () => window.removeEventListener("click", onClick);
  }, [menu]);

  // Toast auto-dismiss (4 s).
  useEffect(() => {
    if (!toast) return;
    const h = setTimeout(() => setToast(null), 4000);
    return () => clearTimeout(h);
  }, [toast]);

  return (
    <>
      <div className="flex shrink-0 items-center justify-between border-b border-gx-border px-3 py-2 text-[11px] text-gx-muted">
        <span className="flex items-center gap-1">
          <GitBranch size={12} />
          {filtered.length}/{total ?? commits?.length ?? 0}
          {/* Subtle indicator while the graph layout worker is running */}
          {graphComputing && (
            <Loader2 size={10} className="animate-spin opacity-50" />
          )}
        </span>
        <div className="flex items-center gap-2">
          <button
            type="button"
            className="text-gx-muted hover:text-gx-text"
            onClick={() => {
              setFindOpen(true);
              requestAnimationFrame(() => findInputRef.current?.focus());
            }}
            title="Find (⌘F)"
            aria-label="Find commits"
          >
            <Search size={11} />
          </button>
          <button
            type="button"
            className="text-gx-muted hover:text-gx-text"
            onClick={refresh}
            title="Refresh"
            aria-label="Refresh timeline"
          >
            <RefreshCw size={11} />
          </button>
        </div>
      </div>

      {findOpen && (
        <FindBar
          inputRef={findInputRef}
          query={findQuery}
          onQuery={setFindQuery}
          matches={findMatches.length}
          cursor={findIdx}
          onNext={() => gotoMatch(findIdx + 1)}
          onPrev={() => gotoMatch(findIdx - 1)}
          onClose={() => setFindOpen(false)}
        />
      )}

      <div className="shrink-0 space-y-1 border-b border-gx-border px-3 py-2">
        <input
          className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
          placeholder="Filter by subject…"
          value={subjectFilter}
          onChange={(e) => setSubjectFilter(e.target.value)}
        />
        <input
          className="w-full rounded border border-gx-border bg-gx-bg-sub px-2 py-1 text-xs focus-visible:outline-none focus:border-gx-accent"
          placeholder="Filter by author…"
          value={authorFilter}
          onChange={(e) => setAuthorFilter(e.target.value)}
        />
      </div>

      <div
        ref={mergedScrollRef}
        tabIndex={0}
        onKeyDown={onKey}
        className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden focus-visible:outline-none"
        style={{ willChange: "scroll-position", contain: "strict" }}
        role="listbox"
        aria-label="Commits"
      >
        {loading && !commits && (
          <div className="flex items-center gap-2 p-3 text-xs text-gx-muted">
            <Loader2 size={12} className="animate-spin" /> Loading git log…
          </div>
        )}
        {err && (
          <div className="m-3 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
            {err}
          </div>
        )}
        {!loading && filtered.length === 0 && commits && (
          <div className="p-4 text-center text-xs text-gx-muted">
            No commits match the current filters.
          </div>
        )}
        {filtered.length > 0 && (
          <VirtualisedCommitList
            filtered={filtered}
            graphData={graphData}
            gutterWidth={gutterWidth}
            selectedIdx={selectedIdx}
            selectedSha={selectedSha}
            findMatches={findMatches}
            tipShas={tipShas}
            tagShas={tagShas}
            experimentRunMap={experimentRunMap}
            ciCheckMap={ciCheckMap}
            scrollRef={scrollRef}
            onSelectCommit={onSelectCommit}
            openHover={openHover}
            closeHover={closeHover}
            setMenu={setMenu}
            onVisibleShasChange={setVisibleShas}
          />
        )}
        {/* Load more button when there are more commits available. */}
        {commits && total != null && commits.length < total && commits.length < MAX_LOADED_COMMITS && (
          <div className="flex items-center justify-center border-t border-gx-border/40 py-3">
            <button
              type="button"
              className="btn text-xs"
              onClick={loadMore}
              disabled={loadingMore}
            >
              {loadingMore ? (
                <>
                  <Loader2 size={11} className="animate-spin" /> Loading…
                </>
              ) : (
                `Load more (${commits.length} / ${total})`
              )}
            </button>
          </div>
        )}
        {commits && total != null && commits.length >= MAX_LOADED_COMMITS && commits.length < total && (
          <div className="flex items-center justify-center border-t border-gx-border/40 py-3 text-[10px] text-gx-muted">
            Showing {commits.length.toLocaleString()} of {total.toLocaleString()} commits. Use Find (⌘F) to search the full history.
          </div>
        )}
      </div>

      {hover && (
        <CommitPopover
          commit={hover.commit}
          isOnHead={trunk.has(hover.commit.sha)}
          anchor={hover.rect}
        />
      )}

      {menu && (
        <CommitContextMenu
          commit={menu.commit}
          x={menu.x}
          y={menu.y}
          repoId={repo.repo_id}
          onClose={() => setMenu(null)}
          onToast={setToast}
          onRefresh={refresh}
        />
      )}

      {/* Inline toast */}
      {toast && (
        <div className="pointer-events-none fixed inset-x-0 bottom-4 z-50 flex justify-center">
          <button
            type="button"
            onClick={() => setToast(null)}
            className={`pointer-events-auto rounded border px-3 py-1.5 text-xs shadow-lg transition ${
              toast.kind === "error"
                ? "border-gx-error/40 bg-gx-error/10 text-gx-error"
                : "border-gx-border bg-gx-surface text-gx-text"
            }`}
          >
            {toast.text}
            <span className="ml-2 text-gx-muted">dismiss</span>
          </button>
        </div>
      )}
    </>
  );
}

/** Virtualised commit list using @tanstack/react-virtual. */
function VirtualisedCommitList({
  filtered,
  graphData,
  gutterWidth,
  selectedIdx,
  selectedSha,
  findMatches,
  tipShas,
  tagShas,
  experimentRunMap,
  ciCheckMap,
  scrollRef,
  onSelectCommit,
  openHover,
  closeHover,
  setMenu,
  onVisibleShasChange,
}: {
  filtered: CommitRow[];
  graphData: ReturnType<typeof useCommitGraphAsync>["graphData"];
  gutterWidth: number;
  selectedIdx: number;
  selectedSha: string | null;
  findMatches: number[];
  tipShas: Set<string>;
  tagShas: Set<string>;
  experimentRunMap: Map<string, ExperimentRun[]>;
  ciCheckMap: Map<string, CiCheck[]>;
  scrollRef: React.RefObject<HTMLDivElement | null>;
  onSelectCommit: (sha: string | null) => void;
  openHover: (c: CommitRow, rect: DOMRect) => void;
  closeHover: () => void;
  setMenu: (m: ContextMenuState | null) => void;
  onVisibleShasChange: (shas: string[]) => void;
}) {
  const virtualizer = useVirtualizer({
    count: filtered.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 15,
  });

  const findMatchSet = useMemo(() => new Set(findMatches), [findMatches]);

  // Track scroll position to cull off-screen graph dots.
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportHeight, setViewportHeight] = useState(800);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    // Initialise from current state.
    setScrollTop(el.scrollTop);
    setViewportHeight(el.clientHeight);
    // Throttle scroll updates to ~30 fps — the dot culling buffer is
    // 3× the viewport so we don't need pixel-perfect accuracy here.
    let rafId: number | null = null;
    const onScroll = () => {
      if (rafId !== null) return;
      rafId = requestAnimationFrame(() => {
        rafId = null;
        setScrollTop(el.scrollTop);
      });
    };
    const onResize = () => setViewportHeight(el.clientHeight);
    el.addEventListener("scroll", onScroll, { passive: true });
    const ro = new ResizeObserver(onResize);
    ro.observe(el);
    return () => {
      el.removeEventListener("scroll", onScroll);
      ro.disconnect();
      if (rafId !== null) cancelAnimationFrame(rafId);
    };
  }, [scrollRef]);

  // Notify the parent which SHAs are currently visible so it can
  // scope experiment-run and CI-check fetches to the viewport.
  // Stabilised by comparing start/end indices — the virtualItems
  // array reference changes on every scroll event, but we only want
  // to notify when the visible *range* actually shifts.
  const virtualItems = virtualizer.getVirtualItems();
  const prevRangeRef = useRef<string>("");
  useEffect(() => {
    if (!virtualItems.length) return;
    const rangeKey = `${virtualItems[0].index}-${virtualItems[virtualItems.length - 1].index}`;
    if (rangeKey === prevRangeRef.current) return;
    prevRangeRef.current = rangeKey;
    const shas = virtualItems.map((item) => filtered[item.index].sha);
    onVisibleShasChange(shas);
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [virtualItems, filtered, onVisibleShasChange]);

  return (
    <div className="relative" style={{ height: virtualizer.getTotalSize() }}>
      {/* Graph SVG — rendered only after the worker finishes layout.
          The commit list is visible immediately; the gutter appears
          once the off-thread computation completes. */}
      {graphData && (
        <CommitGraphSvg
          data={graphData}
          selectedIndex={selectedIdx}
          tipShas={tipShas}
          tagShas={tagShas}
          scrollTop={scrollTop}
          viewportHeight={viewportHeight}
        />
      )}
      <ul
        style={{
          position: "relative",
          height: virtualizer.getTotalSize(),
        }}
      >
        {virtualItems.map((virtualItem) => {
          const c = filtered[virtualItem.index];
          const isFindHit = findMatchSet.has(virtualItem.index);
          return (
            <li
              key={c.sha}
              role="option"
              aria-selected={c.sha === selectedSha}
              style={{
                position: "absolute",
                top: 0,
                left: gutterWidth + 4,
                right: 0,
                height: ROW_HEIGHT,
                transform: `translateY(${virtualItem.start}px)`,
              }}
            >
              <CommitListItem
                commit={c}
                selected={c.sha === selectedSha}
                findHit={isFindHit}
                experimentRuns={experimentRunMap.get(c.sha)}
                ciChecks={ciCheckMap.get(c.sha)}
                onClick={() => onSelectCommit(c.sha)}
                onHoverEnter={openHover}
                onHoverLeave={closeHover}
                onContextMenu={(x, y) => setMenu({ x, y, commit: c })}
              />
            </li>
          );
        })}
      </ul>
    </div>
  );
}

function CommitListItem({
  commit,
  selected,
  findHit,
  experimentRuns,
  ciChecks,
  onClick,
  onHoverEnter,
  onHoverLeave,
  onContextMenu,
}: {
  commit: CommitRow;
  selected: boolean;
  findHit: boolean;
  experimentRuns?: ExperimentRun[];
  ciChecks?: CiCheck[];
  onClick: () => void;
  onHoverEnter: (c: CommitRow, rect: DOMRect) => void;
  onHoverLeave: () => void;
  onContextMenu: (x: number, y: number) => void;
}) {
  const isMerge = commit.parents.length > 1;
  const rowRef = useRef<HTMLDivElement | null>(null);
  return (
    <div
      ref={rowRef}
      style={{ height: ROW_HEIGHT }}
      className={clsx(
        "flex cursor-pointer items-center text-[12px] transition",
        selected
          ? "bg-gx-accent/8 text-gx-text"
          : findHit
            ? "bg-gx-warn/10 text-gx-text"
            : "text-gx-text hover:bg-gx-surface-hover",
      )}
      onClick={onClick}
      onMouseEnter={() => {
        const rect = rowRef.current?.getBoundingClientRect();
        if (rect) onHoverEnter(commit, rect);
      }}
      onMouseLeave={onHoverLeave}
      onContextMenu={(e) => {
        e.preventDefault();
        onContextMenu(e.clientX, e.clientY);
      }}
    >
      <div className="flex min-w-0 flex-1 flex-col justify-center gap-0.5 border-b border-gx-border/40 px-2 py-1">
        <div className="flex min-w-0 items-center gap-2">
          <AuthorDot name={commit.author} />
          <span className="min-w-0 flex-1 truncate">{commit.subject}</span>
          <div className="flex min-w-0 max-w-[45%] shrink items-center gap-1 overflow-hidden">
            {(commit.refs ?? []).map((r, i) => (
              <RefChipBadge key={i} r={r} />
            ))}
          </div>
          {isMerge && (
            <span className="chip shrink-0 text-[9px] text-gx-accent">
              <GitMerge size={9} /> merge
            </span>
          )}
          {experimentRuns && experimentRuns.length > 0 && (
            <RunChip runs={experimentRuns} />
          )}
          {ciChecks && ciChecks.length > 0 && (
            <CiStatusChip checks={ciChecks} size="sm" />
          )}
        </div>
        <div className="flex min-w-0 items-center gap-2 text-[10.5px] text-gx-muted">
          <code className="shrink-0 font-mono">{commit.sha.slice(0, 7)}</code>
          <span className="min-w-0 truncate">{commit.author}</span>
          <span className="shrink-0">· {formatRelativeDate(commit.date)}</span>
        </div>
      </div>
    </div>
  );
}

function FindBar({
  inputRef,
  query,
  onQuery,
  matches,
  cursor,
  onNext,
  onPrev,
  onClose,
}: {
  inputRef: React.RefObject<HTMLInputElement>;
  query: string;
  onQuery: (s: string) => void;
  matches: number;
  cursor: number;
  onNext: () => void;
  onPrev: () => void;
  onClose: () => void;
}) {
  return (
    <div className="flex shrink-0 items-center gap-1 border-b border-gx-border bg-gx-surface px-3 py-1.5">
      <Search size={11} className="text-gx-muted" />
      <input
        ref={inputRef}
        value={query}
        onChange={(e) => onQuery(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            e.shiftKey ? onPrev() : onNext();
          } else if (e.key === "Escape") {
            e.preventDefault();
            onClose();
          }
        }}
        placeholder="Find in commits (subject / author / sha / ref)…"
        className="flex-1 rounded bg-transparent px-1 py-0.5 text-[11.5px] focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-gx-accent"
      />
      <span className="shrink-0 font-mono text-[10px] text-gx-muted">
        {matches === 0 ? "no matches" : `${cursor + 1}/${matches}`}
      </span>
      <button
        type="button"
        onClick={onPrev}
        className="rounded px-1 py-0.5 text-[10px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
        title="Previous match (Shift+Enter)"
        aria-label="Previous match"
      >
        ▲
      </button>
      <button
        type="button"
        onClick={onNext}
        className="rounded px-1 py-0.5 text-[10px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
        title="Next match (Enter)"
        aria-label="Next match"
      >
        ▼
      </button>
      <button
        type="button"
        onClick={onClose}
        className="rounded px-1 py-0.5 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
        title="Close (Esc)"
        aria-label="Close search"
      >
        <X size={11} />
      </button>
    </div>
  );
}

function CommitContextMenu({
  commit,
  x,
  y,
  repoId,
  onClose,
  onToast,
  onRefresh,
}: {
  commit: CommitRow;
  x: number;
  y: number;
  repoId: string;
  onClose: () => void;
  onToast: (t: { kind: "info" | "error"; text: string }) => void;
  onRefresh: () => void;
}) {
  const refs = commit.refs ?? [];
  const branches = refs.filter(
    (r) => r.kind === "branch" || r.kind === "head_target",
  );
  const tags = refs.filter((r) => r.kind === "tag");

  const MAX_WIDTH = 240;
  const left = Math.min(x, window.innerWidth - MAX_WIDTH - 8);
  const top = Math.min(y, window.innerHeight - 260);

  // Build a minimal CommandContext for executeCommand routing.
  const buildContext = (): CommandContext => ({
    repoId,
    currentBranch: "",
    upstream: null,
    ahead: 0,
    behind: 0,
    dirtyCount: 0,
    hasStagedChanges: false,
    activeOperation: null,
    forgeProvider: null,
  });

  const handleCherryPick = () => {
    onClose();
    void executeCommand("git.cherry-pick", buildContext(), commit.sha).then(() => onRefresh());
  };

  const handleRevert = () => {
    onClose();
    void executeCommand("git.revert", buildContext(), commit.sha).then(() => onRefresh());
  };

  const handleReset = async () => {
    onClose();
    // Prompt for reset mode via a simple inline prompt. The git.reset
    // command expects input in "mode:target" format.
    const mode = await promptResetMode();
    if (!mode) return;
    void executeCommand("git.reset", buildContext(), `${mode}:${commit.sha}`).then(() => onRefresh());
  };

  const handleCreateBranch = async () => {
    onClose();
    const name = window.prompt("New branch name:");
    if (!name?.trim()) return;
    // git.branch.create-at expects "branchName:commitSha" format.
    void executeCommand("git.branch.create-at", buildContext(), `${name.trim()}:${commit.sha}`).then(() => onRefresh());
  };

  const handleCheckoutCommit = () => {
    onClose();
    onToast({ kind: "info", text: `⚠️ Checking out ${commit.sha.slice(0, 7)} — you will be in detached HEAD state` });
    void executeCommand("git.checkout.commit", buildContext(), commit.sha).then(() => onRefresh());
  };

  return (
    <div
      role="menu"
      style={{ position: "fixed", top, left, width: MAX_WIDTH, zIndex: 60 }}
      onClick={(e) => e.stopPropagation()}
      className="overflow-hidden rounded-md border border-gx-border bg-gx-surface p-1 text-[12px] shadow-2xl"
    >
      <MenuItem
        icon={<GitCommitHorizontal size={11} />}
        label="Cherry-Pick"
        onClick={handleCherryPick}
      />
      <MenuItem
        icon={<Undo2 size={11} />}
        label="Revert"
        onClick={handleRevert}
      />
      <Divider />
      <MenuItem
        icon={<RotateCcw size={11} />}
        label="Reset to This Commit"
        onClick={() => void handleReset()}
      />
      <MenuItem
        icon={<GitBranch size={11} />}
        label="Create Branch from Here"
        onClick={() => void handleCreateBranch()}
      />
      <MenuItem
        icon={<LogOut size={11} />}
        label="Checkout Commit"
        onClick={handleCheckoutCommit}
      />
      <Divider />
      <MenuItem
        icon={<Copy size={11} />}
        label="Copy commit hash"
        onClick={() => {
          void navigator.clipboard.writeText(commit.sha);
          onClose();
        }}
      />
      <MenuItem
        icon={<Copy size={11} />}
        label="Copy short hash"
        onClick={() => {
          void navigator.clipboard.writeText(commit.sha.slice(0, 7));
          onClose();
        }}
      />
      <MenuItem
        icon={<Copy size={11} />}
        label="Copy subject"
        onClick={() => {
          void navigator.clipboard.writeText(commit.subject.split("\n")[0] ?? "");
          onClose();
        }}
      />
      {branches.length > 0 && (
        <>
          <Divider />
          <MenuHeader label="Branches" />
          {branches.map((r) => (
            <MenuItem
              key={r.name}
              icon={<GitBranch size={11} />}
              label={r.name
                .replace(/^refs\/heads\//, "")
                .replace(/^refs\/remotes\//, "")}
              hint="copy"
              onClick={() => {
                void navigator.clipboard.writeText(r.name.replace(/^refs\/heads\//, ""));
                onClose();
              }}
            />
          ))}
        </>
      )}
      {tags.length > 0 && (
        <>
          <Divider />
          <MenuHeader label="Tags" />
          {tags.map((r) => (
            <MenuItem
              key={r.name}
              icon={<GitBranch size={11} />}
              label={r.name.replace(/^refs\/tags\//, "")}
              hint="copy"
              onClick={() => {
                void navigator.clipboard.writeText(r.name.replace(/^refs\/tags\//, ""));
                onClose();
              }}
            />
          ))}
        </>
      )}
    </div>
  );
}

/** Simple prompt for reset mode selection. Returns null if cancelled. */
function promptResetMode(): Promise<"soft" | "mixed" | "hard" | null> {
  return new Promise((resolve) => {
    const choice = window.prompt("Reset mode (soft / mixed / hard):", "mixed");
    if (!choice) {
      resolve(null);
      return;
    }
    const normalized = choice.trim().toLowerCase();
    if (normalized === "soft" || normalized === "mixed" || normalized === "hard") {
      resolve(normalized);
    } else {
      resolve(null);
    }
  });
}

function MenuItem({
  icon,
  label,
  hint,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  hint?: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex w-full items-center gap-2 rounded px-2 py-1 text-left text-[12px] text-gx-text transition hover:bg-gx-surface-hover"
    >
      <span className="shrink-0 text-gx-muted">{icon}</span>
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {hint && <span className="shrink-0 text-[9.5px] text-gx-muted">{hint}</span>}
    </button>
  );
}

function MenuHeader({ label }: { label: string }) {
  return (
    <div className="px-2 py-0.5 text-[9px] uppercase tracking-wider text-gx-muted">
      {label}
    </div>
  );
}

function Divider() {
  return <div className="my-1 border-t border-gx-border/60" />;
}

function RefChipBadge({ r }: { r: CommitRef }) {
  const cls =
    r.kind === "tag"
      ? "border-gx-warn/50 text-gx-warn"
      : r.kind === "head_target" || r.kind === "head"
        ? "border-gx-accent/50 text-gx-accent"
        : r.kind === "remote"
          ? "border-gx-muted/40 text-gx-muted"
          : "border-gx-success/40 text-gx-success";
  const short = r.name
    .replace(/^refs\/heads\//, "")
    .replace(/^refs\/tags\//, "")
    .replace(/^refs\/remotes\//, "");
  return (
    <span
      className={`inline-block max-w-[120px] shrink truncate rounded-full border bg-gx-bg-sub px-1.5 py-[1px] align-middle font-mono text-[9px] ${cls}`}
      title={`${r.kind} · ${r.name}`}
    >
      {r.kind === "tag" ? `🏷 ${short}` : short}
    </span>
  );
}

function AuthorDot({ name }: { name: string }) {
  const initial = (name.trim()[0] ?? "?").toUpperCase();
  const color = stringColor(name);
  return (
    <span
      className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full text-[10px] font-semibold text-white"
      style={{ background: color }}
      aria-label={`Author ${name}`}
    >
      {initial}
    </span>
  );
}

function stringColor(s: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  const hue = Math.abs(h) % 360;
  return `hsl(${hue} 55% 45%)`;
}

function formatRelativeDate(iso: string): string {
  try {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    const delta = (Date.now() - d.getTime()) / 1000;
    if (delta < 60) return "just now";
    if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
    if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
    if (delta < 604800) return `${Math.floor(delta / 86400)}d ago`;
    return d.toLocaleDateString();
  } catch {
    return iso;
  }
}
