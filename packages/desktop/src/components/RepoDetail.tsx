import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { OpenRepo, repositoryKey } from "../state";
import { CommitRow, DirEntry, HydratedWorktreeEntry, WorktreeStatus, isStorageRepositoryUrl, rpc } from "../ipc";
import { PanelGroup, Panel, PanelResizeHandle } from "react-resizable-panels";
import { WorkspaceShell } from "./WorkspaceShell";
import { isSidebarView, type SidebarView } from "../lib/navigation";
import { useSession } from "../lib/session-context";
import { useSetting } from "../settings";
import { ExplorerView } from "../views/ExplorerView";
import { ChangesView, useGitStatusMap } from "../views/ChangesView";
import { ChangesAndStashesView } from "../views/ChangesAndStashesView";
import { SearchView } from "../views/SearchView";
import { SettingsView } from "../views/SettingsView";
import { WorktreeDetailPanel } from "../views/WorktreeDetailPanel";
import { BranchesView } from "../views/BranchesView";
import { WorktreeView } from "../views/WorktreeView";
import { TimelineAndConflictsView } from "../views/TimelineAndConflictsView";
import { DashboardView } from "../views/DashboardView";
import { CommitDetailView } from "../views/CommitDetailView";
import { BranchDetailView } from "../views/BranchDetailView";
import { SshView } from "../views/SshView";
import { CrabFlowView } from "../views/FlowView";
import { PullRequestsView } from "../views/PullRequestsView";
import { PullRequestView } from "../views/PullRequestView";
import { WorkflowEditorView } from "../views/WorkflowEditorView";
import { TimeMachineView } from "../views/TimeMachineView";
import { WorktreeCompareView } from "../views/WorktreeCompareView";
import { CrossRepoBranchCompareView, CrossRepoCompareTarget } from "../views/CrossRepoBranchCompareView";
import { AgentTasksView } from "../views/AgentTasksView";
import { MergeEditor } from "./MergeEditor";
import { EditorPane, EditorTab } from "./EditorPane";
import { DiffView } from "../views/DiffView";
import { QuickOpen } from "./QuickOpen";
import { StatusBar } from "./StatusBar";
import { useHydrationStatus } from "../hooks/useHydrationStatus";
import { useFileWatcherRoot, useFileWatcher, getWatchPath } from "../hooks/useFileWatcher";
import { useSessionView } from "../hooks/useSessionView";
import { useSessionFile } from "../hooks/useSessionFile";
import { useSessionSubView } from "../hooks/useSessionSubView";

import { NavEntry } from "../hooks/useNavHistory";

interface RepoDetailProps {
  repo: OpenRepo;
  restoreEntry?: NavEntry;
  onNavigate?: (entry: NavEntry) => void;
}

/**
 * Per-repo workspace:
 *   - Activity bar + collapsible sidebar via <WorkspaceShell/>.
 *   - Main pane = tabs of open files OR an active diff view.
 *
 * Tabs are represented as a tagged union so the same tab strip can
 * render editor tabs (path+rev) and diff tabs (path+base/head) side
 * by side. That mirrors VS Code's mixed "editor group" behaviour.
 */
export function RepoDetail({ repo, restoreEntry, onNavigate }: RepoDetailProps) {
  const sessionManager = useSession();
  const sessionKey = repositoryKey(repo);
  const defaultView = isStorageRepositoryUrl(repo.path ?? repo.url) ? "explorer" : "dashboard";
  const [reopening, setReopening] = useState(false);
  const [reopenError, setReopenError] = useState<string | null>(null);
  useEffect(() => setReopenError(null), [repo.native_session_id]);
  async function reopenRepository() {
    setReopening(true); setReopenError(null);
    try { await rpc.reopen(repo.path ?? repo.url, repo); }
    catch (error) { setReopenError(error instanceof Error ? error.message : String(error)); }
    finally { setReopening(false); }
  }

  const [rev, setRev] = useState<string | null>(null);
  const [activeView, setActiveViewRaw] = useState<SidebarView>(() => {
    // Priority: 1) repo.initialView (deferred open), 2) saved session, 3) "dashboard"
    if (repo.initialView && isSidebarView(repo.initialView)) return repo.initialView;
    if (repo.path) {
      const session = sessionManager.activate(sessionKey);
      if (session?.activeView && isSidebarView(session.activeView)) {
        const savedView = session.activeView;
        // If the saved view is a non-full-pane view (e.g. "explorer") but
        // there was no active file, the user was likely seeing the dashboard
        // fallback. Restore to "dashboard" so the indicator matches reality.
        const fullPaneViews: SidebarView[] = [
          "settings", "dashboard", "terminal", "crab",
          "workflows", "orchestrator", "changes", "timeline",
          "pull-requests", "branches", "worktrees",
        ];
        if (!fullPaneViews.includes(savedView) && !session.activeFile) {
          return defaultView;
        }
        return savedView;
      }
    }
    return defaultView;
  });
  const [timelineConflictsTab, setTimelineConflictsTabRaw] = useState<"timeline" | "conflicts">(() => {
    const stored = localStorage.getItem("crab.timelineConflicts.tab");
    return stored === "conflicts" ? "conflicts" : "timeline";
  });

  const handleTimelineConflictsTabChange = useCallback((tab: "timeline" | "conflicts") => {
    setTimelineConflictsTabRaw(tab);
  }, []);

  const setActiveView = useCallback((view: SidebarView) => {
    if (view !== "timeline") setConflictPath(null);
    setActiveViewRaw(view);
  }, []);

  // Start the native file watcher for this repo. Individual views
  // subscribe to `fs/changed` events via `useFileWatcher` hook.
  // Remote repos pass repo_id; local repos pass the filesystem path.
  const watchPath = getWatchPath(repo);
  useFileWatcherRoot(watchPath);

  // Hide the pull-requests tab when no forge provider is detected.
  // Hide the SSH tab when the feature flag is disabled.
  const features = useSetting("features");
  const hiddenViews = useMemo<SidebarView[]>(() => {
    const hidden: SidebarView[] = [];
    if (!repo.forge_provider) hidden.push("pull-requests");
    if (!features.sshRemoteEnabled) hidden.push("ssh");
    return hidden;
  }, [repo.forge_provider, features.sshRemoteEnabled]);

  // Timeline state: the currently focused commit + a cached
  // `git log` snapshot shared between the sidebar and the main
  // CommitDetailView so prev/next navigation doesn't refetch.
  const [selectedCommit, setSelectedCommit] = useState<string | null>(() => {
    // Restore selectedCommit from session if the view is "timeline"
    // so the user returns to the exact commit they were looking at.
    if (activeView === "timeline" && repo.path) {
      const session = sessionManager.activate(sessionKey);
      const savedCommit = session?.viewState?.selectedCommit;
      if (typeof savedCommit === "string" && savedCommit.length > 0) {
        return savedCommit;
      }
    }
    return null;
  });
  const [timelineCommits, setTimelineCommits] = useState<CommitRow[]>([]);
  const [timelineRefreshKey, setTimelineRefreshKey] = useState(0);
  const [selectedBranch, setSelectedBranch] = useState<string | null>(() => {
    // Branch selection belongs to its own route, independently of worktrees.
    if (repo.path) {
      const session = sessionManager.activate(sessionKey);
      const savedBranch = session?.viewState?.selectedBranch;
      if (typeof savedBranch === "string" && savedBranch.length > 0) {
        return savedBranch;
      }
    }
    return null;
  });
  const [selectedPr, setSelectedPr] = useState<number | null>(null);

  // Push navigation entries when the user changes views or selects
  // a commit. Skip pushing when we're restoring from back/forward.
  useEffect(() => {
    onNavigate?.({ view: activeView, commit: selectedCommit });
  }, [activeView, selectedCommit]);

  // Notify the App shell when the active view changes so global
  // shortcuts (e.g. Cmd+Enter for commit) can be scoped correctly.
  useEffect(() => {
    window.dispatchEvent(
      new CustomEvent("crab:active-view-changed", { detail: { view: activeView } }),
    );
  }, [activeView]);

  // Persist the active view to the session on every change so the
  // next deactivation snapshot is always up-to-date.
  useSessionView(activeView);

  // Secondary write path: also persist on the custom event in case
  // external code triggers a view change outside the React state flow.
  useEffect(() => {
    const handler = (e: Event) => {
      const view = (e as CustomEvent<{ view: string }>).detail?.view;
      if (view && repo.path) {
        sessionManager.update(sessionKey, { activeView: view });
      }
    };
    window.addEventListener("crab:active-view-changed", handler);
    return () => window.removeEventListener("crab:active-view-changed", handler);
  }, [sessionKey, sessionManager]);

  const terminalReturnView = useRef<SidebarView>("dashboard");
  useEffect(() => {
    if (activeView !== "terminal") terminalReturnView.current = activeView;
  }, [activeView]);

  useEffect(() => {
    const openTerminal = () => setActiveView("terminal");
    const onKey = (event: KeyboardEvent) => {
      if (event.ctrlKey && !event.metaKey && !event.altKey && !event.shiftKey && event.key === "`") {
        event.preventDefault();
        setActiveView(activeView === "terminal" ? terminalReturnView.current : "terminal");
      }
    };
    window.addEventListener("crab:open-terminal", openTerminal);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("crab:open-terminal", openTerminal);
      window.removeEventListener("keydown", onKey);
    };
  }, [activeView, setActiveView]);

  useEffect(() => {
    const showOverview = () => setActiveView("dashboard");
    window.addEventListener("crab:show-repository-overview", showOverview);
    return () => window.removeEventListener("crab:show-repository-overview", showOverview);
  }, [setActiveView]);

  // The initial view belongs to this repo's session. Only later restore
  // commands may replace it; the shell can still hold the departing repo's entry.
  const previousRestoreRef = useRef(restoreEntry);
  useEffect(() => {
    if (restoreEntry === previousRestoreRef.current) return;
    previousRestoreRef.current = restoreEntry;
    if (!restoreEntry) return;
    if (isSidebarView(restoreEntry.view)) setActiveView(restoreEntry.view);
    if (restoreEntry.commit !== undefined) setSelectedCommit(restoreEntry.commit ?? null);
  }, [restoreEntry]);

  useEffect(() => {
    const handler = (event: Event) => {
      const detail = (event as CustomEvent<{ taskId?: string; repoPath?: string }>).detail;
      if (!detail?.taskId || detail.repoPath !== repo.path) return;
      setActiveView("orchestrator");
      window.setTimeout(() => {
        window.dispatchEvent(new CustomEvent("crab:select-agent-task", { detail }));
      }, 0);
    };
    window.addEventListener("crab:open-agent-task", handler);
    return () => window.removeEventListener("crab:open-agent-task", handler);
  }, [repo.path, setActiveView]);

  // Editor tabs (existing). Keys collide-proof via `rev::path`.
  const [editorTabs, setEditorTabs] = useState<EditorTab[]>([]);
  const [activeKey, setActiveKey] = useState<string | null>(() => {
    // Restore activeFile from session as a diff tab if it was saved.
    // Editor tabs require a DirEntry (loaded async), so those are
    // restored in an effect after entries load.
    if (repo.path) {
      const session = sessionManager.activate(sessionKey);
      if (session?.activeFile) {
        // Restore as a diff key — the diff tab will be created below.
        return `diff::${session.activeFile}`;
      }
    }
    return null;
  });

  // Diff tabs. Same tab strip, distinct key prefix so we can route.
  const [diffTabs, setDiffTabs] = useState<Array<{ key: string; path: string }>>(() => {
    // If we have a saved activeFile, pre-populate a diff tab so the
    // file is visible on first render (zero-flicker restore).
    if (repo.path) {
      const session = sessionManager.activate(sessionKey);
      if (session?.activeFile) {
        const key = `diff::${session.activeFile}`;
        return [{ key, path: session.activeFile }];
      }
    }
    return [];
  });

  // Derive the active file path from the active tab key for session
  // persistence. Diff tabs use "diff::path", editor tabs use
  // "file::rev::path".
  const activeFilePath = useMemo(() => {
    if (!activeKey) return null;
    if (activeKey.startsWith("diff::")) return activeKey.slice("diff::".length);
    if (activeKey.startsWith("file::")) {
      // Format: file::rev::path — extract path after second "::"
      const afterPrefix = activeKey.slice("file::".length);
      const sepIdx = afterPrefix.indexOf("::");
      return sepIdx >= 0 ? afterPrefix.slice(sepIdx + 2) : null;
    }
    return null;
  }, [activeKey]);

  // Persist the active file path to the session on every change.
  useSessionFile(activeFilePath);

  // Persist sub-view state so switching back to a repo shows the exact
  // context — not just the view, but the item within the view.
  useSessionSubView("selectedCommit", selectedCommit);
  useSessionSubView("selectedBranch", selectedBranch);

  // Conflict resolution: selected file in ConflictsView opens MergeEditor.
  const [conflictPath, setConflictPath] = useState<string | null>(null);
  const [conflictFiles, setConflictFiles] = useState<string[]>([]);

  // Conflict count for the activity bar badge.
  const [conflictCount, setConflictCount] = useState(0);

  // Time machine: file path to show in the TimeMachineView.
  const [timeMachinePath, setTimeMachinePath] = useState<string | null>(null);

  // Worktree comparison: branch refs to compare side-by-side.
  const [worktreeCompare, setWorktreeCompare] = useState<{
    baseBranch: string;
    headBranch: string;
    baseLabel: string;
    headLabel: string;
  } | null>(null);

  // Cross-repo branch comparison: compare branches across different repos.
  const [crossRepoCompare, setCrossRepoCompare] = useState<{
    base: CrossRepoCompareTarget;
    head: CrossRepoCompareTarget;
  } | null>(null);

  // Worktree detail: selected worktree to show in the main pane.
  const [selectedWorktree, setSelectedWorktree] = useState<HydratedWorktreeEntry | null>(null);
  const [worktreeStatusMap, setWorktreeStatusMap] = useState<Map<string, WorktreeStatus>>(new Map());

  const [entries, setEntries] = useState<DirEntry[] | null>(null);
  const [quickOpen, setQuickOpen] = useState(false);

  const { map: gitStatus, refresh: refreshGitStatus } = useGitStatusMap(repo.repo_id, !!repo.path && !repo.deferred);
  const hydration = useHydrationStatus(repo.repo_id, (!!repo.path || repo.is_remote) && !repo.deferred);

  // Branch info for the StatusBar toolbar (ahead/behind/upstream).
  const [branchInfo, setBranchInfo] = useState<{
    currentBranch: string;
    ahead: number;
    behind: number;
    upstream: string | null;
  } | null>(null);

  const refreshBranchInfo = useCallback(async () => {
    if (repo.deferred || !repo.path) return;
    try {
      // Only need the current head ref for the status bar; skip the
      // full ref list which can hold thousands of remote-tracking
      // entries on heavily mirrored repos.
      const refs = await rpc.listRefs(repo.repo_id, { kinds: ["branch"] });
      const headRef = refs.refs.find((r) => r.kind === "head" || r.is_head);
      if (!headRef) return;
      const branch = headRef.name.replace(/^refs\/heads\//, "");
      try {
        const info = await rpc.gitBranchInfo(repo.repo_id, branch);
        setBranchInfo({
          currentBranch: branch,
          ahead: info.upstream_ahead,
          behind: info.upstream_behind,
          upstream: info.upstream,
        });
      } catch {
        setBranchInfo({
          currentBranch: branch,
          ahead: headRef.ahead ?? 0,
          behind: headRef.behind ?? 0,
          upstream: headRef.upstream ?? null,
        });
      }
    } catch {
      // Non-fatal — toolbar just won't render
    }
  }, [repo.repo_id, repo.path, repo.deferred]);

  useEffect(() => {
    refreshBranchInfo();
  }, [refreshBranchInfo]);

  // Auto-refresh branch info and git status when files change on disk.
  // This keeps the status bar (ahead/behind counts) and the git status
  // map (used by ExplorerView decorations) up to date.
  useFileWatcher(watchPath, useCallback(() => {
    refreshBranchInfo();
    refreshGitStatus();
    hydration.refresh();
    // Also refresh conflict count for the activity bar badge.
    rpc.conflictList(repo.repo_id).then((r) => setConflictCount(r.files.length)).catch(() => {});
  }, [refreshBranchInfo, refreshGitStatus, hydration.refresh, repo.repo_id]));

  // Initial conflict count fetch.
  useEffect(() => {
    if (repo.deferred || !repo.path) return;
    rpc.conflictList(repo.repo_id).then((r) => setConflictCount(r.files.length)).catch(() => {});
  }, [repo.repo_id, repo.path, repo.deferred]);

  // Handle post-command side effects: refresh relevant state after
  // push/pull/fetch operations complete.
  const handleCommandExecuted = useCallback((commandId: string) => {
    if (commandId === "git.push" || commandId === "git.fetch" || commandId === "git.fetch.all") {
      // Refresh ahead/behind counts after push or fetch
      refreshBranchInfo();
    }
    if (commandId === "git.pull" || commandId === "git.pull.rebase") {
      // Refresh file explorer, changes view, timeline, and branch info after pull
      refreshBranchInfo();
      refreshGitStatus();
      setTimelineRefreshKey((k) => k + 1);
      // Re-fetch file listing for explorer
      if (!repo.deferred) {
        rpc.list(repo.repo_id, {}).then((r) => setEntries(r.entries)).catch(() => {});
      }
      // Check for merge conflicts after pull/rebase and navigate to Conflicts view
      rpc.conflictList(repo.repo_id).then((r) => {
        setConflictCount(r.files.length);
        if (r.files.length > 0) {
          setTimelineConflictsTabRaw("conflicts");
          setActiveView("timeline");
        }
      }).catch(() => {});
    }
  }, [refreshBranchInfo, refreshGitStatus, repo.repo_id, repo.deferred]);

  // Fetch the flat listing for each native session+rev; both Explorer and the
  // quick-open finder reuse it. Explorer also re-fetches on refresh,
  // and that's fine — setState on `entries` here would be stale.
  useEffect(() => {
    if (repo.deferred || !repo.native_session_id) return;
    let cancelled = false;
    (async () => {
      let pinned: string | undefined;
      if (rev) {
        try {
          const r = await rpc.resolveRef(repo.repo_id, rev);
          pinned = r.sha;
        } catch {
          pinned = rev;
        }
      }
      rpc
        .list(repo.repo_id, pinned ? { rev: pinned } : {})
        .then((r) => {
          if (!cancelled) setEntries(r.entries);
        })
        .catch(() => { if (!cancelled) setEntries([]); });
    })();
    return () => {
      cancelled = true;
    };
  }, [repo.repo_id, repo.native_session_id, rev]);

  // Validate the restored activeFile after entries load. If the file
  // no longer exists in the repo, clear the diff tab and activeKey so
  // the view shows without a file selected.
  const restoredFileValidated = useRef(false);
  useEffect(() => {
    if (restoredFileValidated.current) return;
    if (!entries || entries.length === 0) return;

    restoredFileValidated.current = true;

    const session = sessionManager.activate(sessionKey);
    const savedFile = session?.activeFile;
    if (!savedFile) return;

    // Check if the saved file exists in the current entry listing.
    const fileExists = entries.some((e) => e.path === savedFile);
    if (!fileExists) {
      // File no longer exists — clear the restored diff tab and activeKey.
      setDiffTabs((prev) => prev.filter((t) => t.path !== savedFile));
      setActiveKey((prev) => {
        if (prev === `diff::${savedFile}`) return null;
        return prev;
      });
      sessionManager.update(sessionKey, { activeFile: undefined });
    }
  }, [entries, sessionKey, sessionManager]);

  // RepoDetail is keyed by repo_id in App; changing identity remounts its state.
  const entryByPath = useMemo(() => {
    const m = new Map<string, DirEntry>();
    for (const e of entries ?? []) m.set(e.path, e);
    return m;
  }, [entries]);

  const openFile = useCallback(
    (path: string, entry: DirEntry, line?: number, revision = rev) => {
      const key = `file::${revision ?? "HEAD"}::${path}`;
      setEditorTabs((prev) =>
        prev.some((t) => t.key === key)
          ? prev.map((t) => t.key === key ? { ...t, rev: revision, entry, line } : t)
          : [...prev, { key, path, rev: revision, entry, line }],
      );
      setActiveKey(key);
    },
    [rev],
  );

  // Pinned-at-rev variant used by the commit detail view. The
  // tab's rev is fixed to the supplied sha so future rev changes in
  // the live Explorer don't affect it.
  const openFileAtRev = useCallback(
    (path: string, entry: DirEntry, pinRev: string) => {
      const key = `file::${pinRev}::${path}`;
      setEditorTabs((prev) =>
        prev.some((t) => t.key === key)
          ? prev
          : [...prev, { key, path, rev: pinRev, entry }],
      );
      setActiveKey(key);
      setActiveView("explorer");
    },
    [],
  );

  const openDiff = useCallback((path: string) => {
    const key = `diff::${path}`;
    setDiffTabs((prev) => (prev.some((t) => t.key === key) ? prev : [...prev, { key, path }]));
    setActiveKey(key);
    // Changes occupies the main pane; selecting a tab alone leaves it hidden.
    setActiveView("explorer");
  }, [setActiveView]);

  // --- Tab context menu operations ---

  const [pinnedTabKeys, setPinnedTabKeys] = useState<Set<string>>(new Set());

  const closeTab = useCallback(
    (key: string) => {
      if (pinnedTabKeys.has(key)) return; // pinned tabs resist close
      if (key.startsWith("diff::")) {
        setDiffTabs((prev) => prev.filter((t) => t.key !== key));
      } else {
        setEditorTabs((prev) => prev.filter((t) => t.key !== key));
      }
      // If we closed the active tab, fall back to the most recently
      // opened remaining tab of any kind.
      if (activeKey === key) {
        const remaining = [
          ...editorTabs.filter((t) => t.key !== key).map((t) => t.key),
          ...diffTabs.filter((t) => t.key !== key).map((t) => t.key),
        ];
        setActiveKey(remaining[remaining.length - 1] ?? null);
      }
    },
    [activeKey, editorTabs, diffTabs, pinnedTabKeys],
  );

  const pinTab = useCallback((key: string, pinned: boolean) => {
    setPinnedTabKeys((prev) => {
      const next = new Set(prev);
      if (pinned) next.add(key);
      else next.delete(key);
      return next;
    });
  }, []);

  const closeAllTabs = useCallback(() => {
    setEditorTabs((prev) => prev.filter((t) => pinnedTabKeys.has(t.key)));
    setDiffTabs((prev) => prev.filter((t) => pinnedTabKeys.has(t.key)));
    const remaining = [
      ...editorTabs.filter((t) => pinnedTabKeys.has(t.key)).map((t) => t.key),
      ...diffTabs.filter((t) => pinnedTabKeys.has(t.key)).map((t) => t.key),
    ];
    setActiveKey(remaining[remaining.length - 1] ?? null);
  }, [editorTabs, diffTabs, pinnedTabKeys]);

  const closeOtherTabs = useCallback(
    (key: string) => {
      setEditorTabs((prev) => prev.filter((t) => t.key === key || pinnedTabKeys.has(t.key)));
      setDiffTabs((prev) => prev.filter((t) => t.key === key || pinnedTabKeys.has(t.key)));
      setActiveKey(key);
    },
    [pinnedTabKeys],
  );

  const closeTabsToRight = useCallback(
    (key: string) => {
      const allKeys = [
        ...editorTabs.map((t) => t.key),
        ...diffTabs.map((t) => t.key),
      ];
      const idx = allKeys.indexOf(key);
      if (idx === -1) return;
      const keysToClose = new Set(allKeys.slice(idx + 1).filter((k) => !pinnedTabKeys.has(k)));
      setEditorTabs((prev) => prev.filter((t) => !keysToClose.has(t.key)));
      setDiffTabs((prev) => prev.filter((t) => !keysToClose.has(t.key)));
      if (activeKey && keysToClose.has(activeKey)) {
        setActiveKey(key);
      }
    },
    [editorTabs, diffTabs, activeKey, pinnedTabKeys],
  );

  /** Close Saved — closes all tabs that are NOT dirty.
   *  Accepts a set of dirty keys from EditorPane so we know which to keep. */
  const closeSavedTabs = useCallback(
    (dirtyKeys: Set<string>) => {
      const keysToClose = [
        ...editorTabs.map((t) => t.key),
        ...diffTabs.map((t) => t.key),
      ].filter((k) => !dirtyKeys.has(k) && !pinnedTabKeys.has(k));
      const closeSet = new Set(keysToClose);
      setEditorTabs((prev) => prev.filter((t) => !closeSet.has(t.key)));
      setDiffTabs((prev) => prev.filter((t) => !closeSet.has(t.key)));
      if (activeKey && closeSet.has(activeKey)) {
        const remaining = [
          ...editorTabs.filter((t) => !closeSet.has(t.key)).map((t) => t.key),
          ...diffTabs.filter((t) => !closeSet.has(t.key)).map((t) => t.key),
        ];
        setActiveKey(remaining[remaining.length - 1] ?? null);
      }
    },
    [editorTabs, diffTabs, activeKey, pinnedTabKeys],
  );

  // --- Split editor support ---
  // When the user splits a tab, we store the split tab info and render
  // a second editor pane side-by-side.
  const [splitTab, setSplitTab] = useState<EditorTab | null>(null);

  const handleSplitRight = useCallback(
    (key: string) => {
      const tab = editorTabs.find((t) => t.key === key);
      if (tab) {
        setSplitTab(tab);
      }
    },
    [editorTabs],
  );

  const handleSplitDown = useCallback(
    (key: string) => {
      // For now, split down behaves the same as split right — opens in
      // a second pane. Direction can be refined later with PanelGroup direction.
      const tab = editorTabs.find((t) => t.key === key);
      if (tab) {
        setSplitTab(tab);
      }
    },
    [editorTabs],
  );

  // Cmd-P / Ctrl-P — global, works across views.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key === "p") {
        e.preventDefault();
        setQuickOpen(true);
      } else if (e.key === "Escape") {
        setQuickOpen(false);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Listen for "Git Time Machine" events dispatched from the context
  // menu or the command palette.
  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent<{ path?: string }>).detail;
      const filePath = detail?.path ?? editorTabs.find((t) => t.key === activeKey)?.path ?? null;
      if (filePath) {
        setTimeMachinePath(filePath);
      }
    };
    window.addEventListener("crab:open-time-machine", handler);
    return () => window.removeEventListener("crab:open-time-machine", handler);
  }, [editorTabs, activeKey]);

  // Listen for "Worktree Compare" events dispatched from the worktree
  // context menu when the user picks a target worktree to compare.
  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent<{
        baseBranch: string;
        headBranch: string;
        baseLabel: string;
        headLabel: string;
      }>).detail;
      if (detail) {
        setWorktreeCompare(detail);
      }
    };
    window.addEventListener("crab:open-worktree-compare", handler);
    return () => window.removeEventListener("crab:open-worktree-compare", handler);
  }, []);

  // Cross-repo branch comparison: triggered from the workspace dashboard
  // or sidebar when the user picks two repos + branches to compare.
  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent<{
        base: CrossRepoCompareTarget;
        head: CrossRepoCompareTarget;
      }>).detail;
      if (detail) {
        setCrossRepoCompare(detail);
      }
    };
    window.addEventListener("crab:open-cross-repo-compare", handler);
    return () => window.removeEventListener("crab:open-cross-repo-compare", handler);
  }, []);

  // Sidebar content uses the same route as the rail and local navigation.
  const views: Partial<Record<SidebarView, React.ReactNode>> = {
    explorer: (
      <ExplorerView
        repo={repo}
        rev={rev}
        onChangeRev={setRev}
        onOpenFile={openFile}
        activePath={(editorTabs.find((t) => t.key === activeKey)?.path) ?? null}
        gitStatus={gitStatus}
        onRefreshGitStatus={refreshGitStatus}
        hydrationByPath={hydration.byPath}
      />
    ),
    search: (
      <SearchView repo={repo} onOpenFile={openFile} entryByPath={entryByPath} />
    ),
    changes: (
      <ChangesView
        repo={repo}
        onOpenDiff={openDiff}
        activeDiffPath={
          activeKey?.startsWith("diff::") ? activeKey.slice("diff::".length) : null
        }
        onCommit={() => setTimelineRefreshKey((k) => k + 1)}
      />
    ),
    crab: <CrabFlowView repo={repo} onSelectCommit={(sha) => { setSelectedCommit(sha); setActiveView("timeline"); }} onOpenFile={openFile} />,
    branches: (
      <BranchesView repo={repo} selectedBranch={selectedBranch} onSelectBranch={setSelectedBranch} />
    ),
    worktrees: (
      <WorktreeView
        repo={repo}
        onSelectWorktree={setSelectedWorktree}
        selectedWorktree={selectedWorktree}
        statusMap={worktreeStatusMap}
        onStatusMapChange={setWorktreeStatusMap}
      />
    ),
    timeline: (
      <TimelineAndConflictsView
        repo={repo}
        selectedCommit={selectedCommit}
        onSelectCommit={(sha) => {
          setSelectedCommit(sha);
          if (sha) setConflictPath(null);
        }}
        onCommitsLoaded={setTimelineCommits}
        timelineRefreshKey={timelineRefreshKey}
        conflictPath={conflictPath}
        onSelectConflictFile={(p) => {
          setConflictPath(p);
          if (p) setSelectedCommit(null);
        }}
        onConflictCountChange={(count) => {
          setConflictCount(count);
          rpc.conflictList(repo.repo_id).then((r) => {
            setConflictFiles(r.files.map((f) => f.path));
          }).catch(() => {});
        }}
        conflictCount={conflictCount}
        requestedTab={timelineConflictsTab}
        onTabChange={handleTimelineConflictsTabChange}
      />
    ),
    "pull-requests": (
      <PullRequestsView
        repo={repo}
        onNavigateToSettings={() => setActiveView("settings")}
        onSelectPr={(num) => setSelectedPr(num)}
      />
    ),
    workflows: (
      <div className="p-3 text-xs text-gx-muted">
        Workflows open in the main area.
      </div>
    ),
    dashboard: (
      <div className="p-3 text-xs text-gx-muted">
        Dashboard opens in the main area.
      </div>
    ),
    settings: (
      <div className="p-3 text-xs text-gx-muted">
        Settings open in the main area. Press{" "}
        <span className="kbd">⌘,</span> anytime to jump back.
      </div>
    ),
  };

  const activeDiff = diffTabs.find((t) => t.key === activeKey);

  const hasOpenTabs = editorTabs.length > 0 || diffTabs.length > 0;

  // Views that render their own full-pane content (not the editor pane).
  // When the active view is NOT one of these and there are no open tabs,
  // we show the Dashboard as a landing page instead of a blank screen.
  const fullPaneActiveViews: SidebarView[] = [
    "settings", "dashboard", "terminal", "crab",
    "workflows", "orchestrator", "changes", "timeline",
    "pull-requests", "branches", "worktrees",
  ];
  const showDashboardFallback =
    !hasOpenTabs &&
    !fullPaneActiveViews.includes(activeView) &&
    !timeMachinePath &&
    !worktreeCompare &&
    !crossRepoCompare &&
    !splitTab;

  return (
    <div className="flex h-full min-w-0 flex-col">
      {repo.native_session_id === null && <div role={reopenError || repo.native_error ? "alert" : "status"} className="flex shrink-0 items-center justify-between gap-3 border-b border-gx-border bg-gx-surface px-3 py-2 text-xs">
        <span className="min-w-0 break-words text-gx-muted">{reopenError ?? repo.native_error ?? "Restoring repository services…"}</span>
        {(reopenError || repo.native_error) && <button type="button" className="btn shrink-0 text-xs" disabled={reopening} onClick={() => void reopenRepository()}>{reopening ? "Reopening…" : "Retry reopening repository"}</button>}
      </div>}
      <div className="flex min-h-0 flex-1">
        <WorkspaceShell
          views={views}
          onViewChange={setActiveView}
          activeView={activeView}
          fullPaneViews={["settings", "dashboard", "terminal", "crab", "workflows", "orchestrator", "changes", "ssh"]}
          hiddenViews={hiddenViews}
          badges={conflictCount > 0 ? { timeline: conflictCount } : undefined}
          main={
            <div className="relative flex h-full w-full min-w-0 flex-col">
              {activeView === "terminal" ? null : activeView === "settings" ? (
                <SettingsView />
              ) : activeView === "ssh" ? (
                <SshView />
              ) : activeView === "crab" ? (
                <CrabFlowView repo={repo} onSelectCommit={(sha) => { setSelectedCommit(sha); setActiveView("timeline"); }} onOpenFile={openFile} />
              ) : activeView === "workflows" ? (
                <WorkflowEditorView repo={repo} onOpenSource={async path => {
                  const stat = await rpc.stat(repo.repo_id, path);
                  setRev(null);
                  openFile(path, { path, kind: "file", size: stat.size, pointer_hash: stat.pointer_hash }, undefined, null);
                  setActiveView("explorer");
                }} />
              ) : activeView === "orchestrator" ? (
                <AgentTasksView repo={repo} onOpenAgentManager={() => {
                  setActiveView("settings");
                  // Navigate directly to the Agents section within Settings.
                  setTimeout(() => {
                    window.dispatchEvent(
                      new CustomEvent("crab:settings-navigate", { detail: { section: "agents" } }),
                    );
                  }, 0);
                }} />
              ) : activeView === "changes" ? (
                <ChangesAndStashesView
                  repo={repo}
                  onOpenDiff={openDiff}
                  activeDiffPath={null}
                  onCommit={() => setTimelineRefreshKey((k) => k + 1)}
                />
              ) : activeView === "dashboard" ? (
                <DashboardView
                  repo={repo}
                  onSelectCommit={(sha) => {
                    setSelectedCommit(sha);
                    setActiveView("timeline");
                  }}
                  onSelectBranch={(name) => {
                    setSelectedBranch(name);
                    setActiveView("branches");
                  }}
                />
            ) : activeView === "timeline" && selectedCommit ? (
              <CommitDetailView
                  repo={repo}
                  commits={timelineCommits}
                  selectedSha={selectedCommit}
                  onSelectCommit={setSelectedCommit}
                  onOpenFileAtRev={openFileAtRev}
                />
            ) : activeView === "branches" && selectedBranch ? (
              <BranchDetailView
                  repo={repo}
                  branchName={selectedBranch}
                  onSelectCommit={(sha) => {
                    setSelectedCommit(sha);
                    setActiveView("timeline");
                  }}
                />
            ) : activeView === "pull-requests" ? (
              selectedPr ? (
                <PullRequestView
                  repo={repo}
                  prNumber={selectedPr}
                  onClose={() => setSelectedPr(null)}
                />
              ) : (
                <PullRequestsEmpty />
              )
            ) : conflictPath ? (
              <MergeEditor
                repoId={repo.repo_id}
                path={conflictPath}
                onResolved={(p) => {
                  setConflictPath(null);
                  // The ConflictsView will auto-refresh on next render
                  void p;
                }}
                onCancel={() => setConflictPath(null)}
                onNextFile={
                  conflictFiles.length > 1
                    ? () => {
                        const idx = conflictFiles.indexOf(conflictPath);
                        const next = conflictFiles[(idx + 1) % conflictFiles.length];
                        if (next) setConflictPath(next);
                      }
                    : undefined
                }
                onPrevFile={
                  conflictFiles.length > 1
                    ? () => {
                        const idx = conflictFiles.indexOf(conflictPath);
                        const prev = conflictFiles[(idx - 1 + conflictFiles.length) % conflictFiles.length];
                        if (prev) setConflictPath(prev);
                      }
                    : undefined
                }
                filePositionLabel={
                  conflictFiles.length > 1
                    ? `${conflictFiles.indexOf(conflictPath) + 1} of ${conflictFiles.length}`
                    : undefined
                }
              />
            ) : timeMachinePath ? (
              <TimeMachineView repo={repo} path={timeMachinePath} />
            ) : activeView === "worktrees" && selectedWorktree ? (
              <WorktreeDetailPanel
                repo={repo}
                worktree={selectedWorktree}
                status={worktreeStatusMap.get(selectedWorktree.path)}
                onClose={() => setSelectedWorktree(null)}
              />
            ) : worktreeCompare ? (
              <WorktreeCompareView
                repo={repo}
                baseBranch={worktreeCompare.baseBranch}
                headBranch={worktreeCompare.headBranch}
                baseLabel={worktreeCompare.baseLabel}
                headLabel={worktreeCompare.headLabel}
                onClose={() => setWorktreeCompare(null)}
              />
            ) : crossRepoCompare ? (
              <CrossRepoBranchCompareView
                base={crossRepoCompare.base}
                head={crossRepoCompare.head}
                onClose={() => setCrossRepoCompare(null)}
              />
            ) : showDashboardFallback ? (
              <DashboardView
                repo={repo}
                onSelectCommit={(sha) => {
                  setSelectedCommit(sha);
                  setActiveView("timeline");
                }}
                onSelectBranch={(name) => {
                  setSelectedBranch(name);
                  setActiveView("branches");
                }}
              />
            ) : splitTab ? (
              <PanelGroup direction="horizontal" className="h-full">
                <Panel minSize={20}>
                  <EditorPane
                    repo={repo}
                    tabs={editorTabs}
                    diffTabs={diffTabs}
                    activeKey={activeKey}
                    onActivate={setActiveKey}
                    onClose={closeTab}
                    onCloseAll={closeAllTabs}
                    onCloseOthers={closeOtherTabs}
                    onCloseToRight={closeTabsToRight}
                    onCloseSaved={closeSavedTabs}
                    onPin={pinTab}
                    onSplit={handleSplitRight}
                    onSplitDown={handleSplitDown}
                    pinnedKeys={pinnedTabKeys}
                    renderDiff={activeDiff ? <DiffView repo={repo} path={activeDiff.path} /> : null}
                  />
                </Panel>
                <PanelResizeHandle className="w-1 bg-gx-border hover:bg-gx-accent transition-colors" />
                <Panel minSize={20}>
                  <EditorPane
                    repo={repo}
                    tabs={[splitTab]}
                    diffTabs={[]}
                    activeKey={splitTab.key}
                    onActivate={() => {}}
                    onClose={() => setSplitTab(null)}
                    onCloseAll={() => setSplitTab(null)}
                    pinnedKeys={pinnedTabKeys}
                    renderDiff={null}
                  />
                </Panel>
              </PanelGroup>
            ) : (
              <EditorPane
                repo={repo}
                tabs={editorTabs}
                diffTabs={diffTabs}
                activeKey={activeKey}
                onActivate={setActiveKey}
                onClose={closeTab}
                onCloseAll={closeAllTabs}
                onCloseOthers={closeOtherTabs}
                onCloseToRight={closeTabsToRight}
                onCloseSaved={closeSavedTabs}
                onPin={pinTab}
                onSplit={handleSplitRight}
                onSplitDown={handleSplitDown}
                pinnedKeys={pinnedTabKeys}
                renderDiff={activeDiff ? <DiffView repo={repo} path={activeDiff.path} /> : null}
              />
            )}
            </div>
          }
        />
      </div>

      <div className={activeView === "terminal" ? "hidden" : undefined}>
        <StatusBar
          summary={hydration.summary}
          loading={hydration.loading}
          error={hydration.error}
          onRefresh={hydration.refresh}
          syncToolbar={branchInfo ? {
            repoId: repo.repo_id,
            currentBranch: branchInfo.currentBranch,
            ahead: branchInfo.ahead,
            behind: branchInfo.behind,
            upstream: branchInfo.upstream,
          } : null}
          onCommandExecuted={handleCommandExecuted}
        />
      </div>
      <QuickOpen
        entries={entries}
        open={quickOpen}
        onClose={() => setQuickOpen(false)}
        onPick={(entry) => openFile(entry.path, entry)}
      />
    </div>
  );
}

function PullRequestsEmpty() {
  return (
    <div className="flex h-full items-center justify-center bg-gx-bg-sub">
      <div className="text-center text-sm text-gx-muted">
        <div className="mb-1 text-base text-gx-text">Select a pull request</div>
        <div>Click a pull request on the left to see its timeline, reviews, and changes.</div>
      </div>
    </div>
  );
}
