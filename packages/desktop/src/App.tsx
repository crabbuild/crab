import { useCallback, useEffect, useRef, useState, useMemo } from "react";
import { StoreProvider, useActiveRepo, useStore, WorkspaceRepoEntry, getActiveWorkspace } from "./state";
import { SettingsProvider, useSetting, useSettings } from "./settings";
import { PtyModeProvider } from "./lib/pty-mode-context";
import { SessionProvider, useSession, useWorkspaceSession, useActiveRepoPath } from "./lib/session-context";
import { rpc } from "./ipc";
import { Sidebar } from "./components/Sidebar";
import { RepoDetail } from "./components/RepoDetail";
import { EmptyState } from "./components/EmptyState";
import { ProgressDrawer } from "./components/ProgressDrawer";
import { TopBar } from "./components/TopBar";
import { UpdateBanner } from "./components/UpdateBanner";
import { TelemetryOptInModal } from "./components/TelemetryOptInModal";
import { CommandPalette } from "./components/CommandPalette";
import { ToastManager } from "./components/ToastManager";
import { WorkspaceThemeEffect } from "./components/WorkspaceThemeEffect";
import { Welcome } from "./components/Welcome";
import { WorkspaceDashboardView } from "./views/WorkspaceDashboardView";
import { TerminalKeepAlive, TerminalKeepAliveHandle } from "./components/TerminalKeepAlive";
import { useTabBarShortcuts } from "./components/workspace/TabBar";
import { FuzzySwitcher } from "./components/workspace/FuzzySwitcher";
import { useWorkspaceRepoSelect } from "./hooks/useWorkspaceRepoSelect";
import { useWorkspaceIPC } from "./hooks/useWorkspaceIPC";
import { useFuzzySwitcherShortcut } from "./hooks/useFuzzySwitcherShortcut";
import { useNavHistory, NavEntry } from "./hooks/useNavHistory";
import { useHeapWatcherEffect } from "./hooks/useHeapWatcher";
import { useCommandPaletteShortcuts } from "./hooks/useCommandPaletteShortcuts";
import { useSshHealthAlerts } from "./hooks/useSshHealthAlerts";
import { setupAllToastPublishers } from "./lib/toast-publishers";
import { localPathName } from "./lib/local-path-name";
import type { SshTerminalRequest, TerminalStartupRequest } from "./views/TerminalView";
import type { StartupCommand } from "./lib/terminal-presets";

// Side-effect imports: register commands in the command-registry at startup.
import "./lib/git-commands";
import "./lib/view-commands";

const LS_REPO_SIDEBAR = "crab.repoSidebarOpen";

function Shell() {
  const active = useActiveRepo();
  const { state, dispatch, addRepo, setActive, setMaxLogLines } = useStore();
  const [drawerOpen, setDrawerOpen] = useState(false);
  const maxLogLines = useSetting("maxLogLines");
  const showActivityBar = useSetting("showActivityBar");
  const reopenReposOnStartup = useSetting("reopenReposOnStartup");

  // Workspace tab bar state.
  const selectWorkspaceRepo = useWorkspaceRepoSelect();
  const workspaceActive = state.workspaces.length > 0;

  // Subscribe to workspace IPC events (discovery + status polling) and
  // handle startup restore of persisted workspace path.
  useWorkspaceIPC();

  // All workspace repos flattened (for tab bar and fuzzy switcher).
  const allWorkspaceRepos = useMemo(() => {
    return state.workspaces.flatMap((ws) => ws.repos);
  }, [state.workspaces]);

  // Repos belonging to the currently selected workspace (for TopBar dropdown).
  const activeWorkspaceRepos = useMemo(() => {
    const ws = getActiveWorkspace(state);
    return ws ? ws.repos : [];
  }, [state.workspaces, state.activeWorkspaceRootPath]);

  // Resolve tab bar entries from workspace state.
  const tabBarEntries: WorkspaceRepoEntry[] = useMemo(() => {
    if (state.workspaces.length === 0) return [];
    return state.tabBar
      .map((rp) => allWorkspaceRepos.find((e) => e.relativePath === rp))
      .filter((e): e is WorkspaceRepoEntry => e !== undefined);
  }, [state.workspaces, state.tabBar, allWorkspaceRepos]);

  // Determine the active workspace repo relative path for tab highlighting.
  const activeWorkspaceRelativePath = useMemo(() => {
    if (state.workspaces.length === 0 || !state.activeRepoId) return null;
    for (const ws of state.workspaces) {
      const entry = ws.repos.find((e) => e.openRepoId === state.activeRepoId);
      if (entry) return entry.relativePath;
    }
    return null;
  }, [state.workspaces, state.activeRepoId]);

  // Navigation history (VS Code-style back/forward).
  // The onRepoSwitch callback handles cross-repo navigation: when
  // back/forward restores an entry with a different repoPath, we
  // switch the active repo via the L2 promotion flow.
  const nav = useNavHistory({
    onRepoSwitch: selectWorkspaceRepo,
  });

  // Tab bar select handler: dispatches tab-bar-activate and switches repo.
  // Also pushes a navigation entry so ⌘[ / ⌘] can restore the switch.
  const handleTabSelect = useCallback(
    (relativePath: string) => {
      if (relativePath !== activeWorkspaceRelativePath) {
        nav.push({ view: "explorer", repoPath: relativePath });
      }
      dispatch({ type: "tab-bar-activate", relativePath });
      selectWorkspaceRepo(relativePath);
    },
    [dispatch, selectWorkspaceRepo, activeWorkspaceRelativePath, nav],
  );

  // ⌘1–⌘9 keyboard shortcuts for tab positions.
  useTabBarShortcuts(tabBarEntries, handleTabSelect, workspaceActive);

  // ⌘K fuzzy repo switcher.
  const fuzzySwitcher = useFuzzySwitcherShortcut(workspaceActive);

  const handleFuzzySelect = useCallback(
    (relativePath: string) => {
      fuzzySwitcher.closeSwitcher();
      if (relativePath !== activeWorkspaceRelativePath) {
        nav.push({ view: "explorer", repoPath: relativePath });
      }
      dispatch({ type: "tab-bar-activate", relativePath });
      selectWorkspaceRepo(relativePath);
    },
    [fuzzySwitcher, dispatch, selectWorkspaceRepo, activeWorkspaceRelativePath, nav],
  );

  // Start the heap watcher for memory-pressure eviction.
  useHeapWatcherEffect();

  // Wire up toast publishers for job completion, PR updates, and agent errors.
  useEffect(() => setupAllToastPublishers(), []);

  // Listen for SSH health alerts and publish toast notifications.
  useSshHealthAlerts();

  // The outer "Repositories" sidebar. Users collapse it to give the
  // workspace more horizontal room; restore via a top-bar button or
  // `Cmd+Shift+B`. We deliberately pick a different accelerator from
  // the inner WorkspaceShell's `Cmd+B` (the one that toggles the
  // Explorer/Search/Changes column) so the two can coexist without
  // fighting each other.
  const [repoSidebarOpen, setRepoSidebarOpen] = useState<boolean>(() => {
    const saved = localStorage.getItem(LS_REPO_SIDEBAR);
    return saved == null ? true : saved === "1";
  });

  const [terminalRepoId, setTerminalRepoId] = useState<string | null>(null);
  const terminalActive = active != null && terminalRepoId === active.repo_id;
  const [terminalActivated, setTerminalActivated] = useState(false);
  const [sshTerminalRequest, setSshTerminalRequest] = useState<SshTerminalRequest | null>(null);
  const [startupRequest, setStartupRequest] = useState<TerminalStartupRequest | null>(null);
  const openTerminal = useCallback(() => {
    window.dispatchEvent(new CustomEvent("crab:open-terminal"));
  }, []);

  // ── Session persistence: deactivate/activate on repo switch ──────
  const sessionManager = useSession();
  const wsSessionManager = useWorkspaceSession();
  const activeRepoPath = useActiveRepoPath();
  const prevRepoPathRef = useRef<string | null>(null);

  useEffect(() => {
    const prevPath = prevRepoPathRef.current;
    const newPath = activeRepoPath;

    // Skip the initial mount (no previous repo to deactivate).
    if (prevPath === newPath) return;

    // Deactivate the departing repo — snapshot current nav state.
    if (prevPath) {
      const navState = nav.getState();
      sessionManager.deactivate(prevPath, {
        activeView: nav.current?.view ?? "explorer",
        navHistory: navState.entries,
        navIndex: navState.index,
      });
    }

    // Activate the arriving repo (loads from cache or localStorage).
    if (newPath) {
      const session = sessionManager.activate(newPath);
      if (session?.navHistory && session.navHistory.length > 0) {
        nav.setState(session.navHistory, session.navIndex);
      }
    }

    prevRepoPathRef.current = newPath;
  }, [activeRepoPath, sessionManager, nav]);

  // Flush all dirty sessions on window close / app quit.
  // Also save the current workspace session before quitting.
  useEffect(() => {
    const handleBeforeUnload = () => {
      // Save the current workspace session so it persists across restarts.
      const activeWs = state.workspaces[0];
      if (activeWs) {
        wsSessionManager.save(activeWs.rootPath, {
          activeRepoRelativePath: activeWorkspaceRelativePath ?? null,
          tabBarOrder: state.tabBar,
          sidebarWidth: repoSidebarOpen ? 260 : 0,
        });
      }
      wsSessionManager.flush();
      // A first-open repository may never have been deactivated. Snapshot its
      // route before flushing so it restores just like a switched-away repo.
      if (activeRepoPath) {
        const snapshot = nav.getState();
        sessionManager.deactivate(activeRepoPath, {
          activeView: snapshot.entries[snapshot.index].view,
          navHistory: snapshot.entries,
          navIndex: snapshot.index,
        });
      }
      sessionManager.flush();
    };
    window.addEventListener("beforeunload", handleBeforeUnload);
    return () => window.removeEventListener("beforeunload", handleBeforeUnload);
  }, [sessionManager, wsSessionManager, state.workspaces, activeWorkspaceRelativePath, state.tabBar, repoSidebarOpen, activeRepoPath, nav.getState]);

  // ── Worktree session cleanup on app start ──────────────────────────
  // Cross-reference persisted sessions against currently known repo paths.
  // Sessions for paths no longer in the store are queued for cleanup;
  // queued entries older than 24 hours are purged.
  const cleanupRanRef = useRef(false);
  useEffect(() => {
    if (cleanupRanRef.current) return;
    if (state.repos.length === 0) return;
    cleanupRanRef.current = true;
    const knownPaths = state.repos
      .map((r) => r.path)
      .filter((p): p is string => typeof p === "string" && p.length > 0);
    sessionManager.cleanupStaleWorktrees(knownPaths);
  }, [sessionManager, state.repos]);

  // ── Workspace session persistence: save/restore on workspace change ──
  const prevWorkspaceRootRef = useRef<string | null>(null);
  const wsStartupRestoredRef = useRef(false);

  useEffect(() => {
    const currentWs = state.workspaces[0];
    const currentRoot = currentWs?.rootPath ?? null;
    const prevRoot = prevWorkspaceRootRef.current;

    if (prevRoot === currentRoot) return;

    // Save the departing workspace session. Terminal ownership follows paths.
    if (prevRoot) {
      workspaceSwitchingRef.current = true;
      wsSessionManager.save(prevRoot, {
        activeRepoRelativePath: activeWorkspaceRelativePath ?? null,
        tabBarOrder: state.tabBar,
        sidebarWidth: repoSidebarOpen ? 260 : 0,
      });
      wsSessionManager.flush();
    }

    // Restore the arriving workspace session (startup or workspace switch).
    if (currentRoot && !wsStartupRestoredRef.current) {
      wsStartupRestoredRef.current = true;
      const saved = wsSessionManager.restore(currentRoot);
      if (saved) {
        // Restore tab bar order.
        if (saved.tabBarOrder && saved.tabBarOrder.length > 0) {
          for (const relativePath of saved.tabBarOrder) {
            // Only activate tabs for repos that still exist in the workspace.
            const exists = currentWs?.repos.some((r) => r.relativePath === relativePath);
            if (exists) {
              dispatch({ type: "tab-bar-activate", relativePath });
            }
          }
        }

        // Activate the saved repo (if it still exists in the workspace).
        if (saved.activeRepoRelativePath) {
          const exists = currentWs?.repos.some(
            (r) => r.relativePath === saved.activeRepoRelativePath,
          );
          if (exists) {
            dispatch({ type: "tab-bar-activate", relativePath: saved.activeRepoRelativePath });
            selectWorkspaceRepo(saved.activeRepoRelativePath);
          }
        }

        // The active view is restored by the per-repository session.
        if (typeof saved.sidebarWidth === "number") {
          setRepoSidebarOpen(saved.sidebarWidth > 0);
        }
      }
    } else if (currentRoot && prevRoot) {
      // Keep removals from evicting renderers while workspace state settles.
      requestAnimationFrame(() => {
        workspaceSwitchingRef.current = false;
      });

      const saved = wsSessionManager.restore(currentRoot);
      if (saved) {
        if (saved.tabBarOrder && saved.tabBarOrder.length > 0) {
          for (const relativePath of saved.tabBarOrder) {
            const exists = currentWs?.repos.some((r) => r.relativePath === relativePath);
            if (exists) {
              dispatch({ type: "tab-bar-activate", relativePath });
            }
          }
        }
        if (saved.activeRepoRelativePath) {
          const exists = currentWs?.repos.some(
            (r) => r.relativePath === saved.activeRepoRelativePath,
          );
          if (exists) {
            dispatch({ type: "tab-bar-activate", relativePath: saved.activeRepoRelativePath });
            selectWorkspaceRepo(saved.activeRepoRelativePath);
          }
        }

        // The active view is restored by the per-repository session.
        if (typeof saved.sidebarWidth === "number") {
          setRepoSidebarOpen(saved.sidebarWidth > 0);
        }
      }
    }

    // Clear the workspace switching flag after a short delay to allow
    // the repo list to stabilize before re-enabling eviction.
    if (prevRoot && !currentRoot) {
      // Workspace closed entirely — allow eviction after state settles.
      requestAnimationFrame(() => {
        workspaceSwitchingRef.current = false;
      });
    }

    prevWorkspaceRootRef.current = currentRoot;
  }, [state.workspaces, wsSessionManager, dispatch, selectWorkspaceRepo, activeWorkspaceRelativePath, state.tabBar, repoSidebarOpen, state.repos]);

  // Command palette state + global keyboard shortcuts.
  const [paletteOpen, setPaletteOpen] = useState(false);

  // Fetch current branch for command palette context.
  const [paletteBranch, setPaletteBranch] = useState("");
  useEffect(() => {
    if (!active || active.deferred) { setPaletteBranch(""); return; }
    rpc.listRefs(active.repo_id, { kinds: ["branch"] }).then((r) => {
      const head = r.refs.find((ref) => ref.is_head && ref.kind === "branch");
      setPaletteBranch(head?.name ?? "");
    }).catch(() => setPaletteBranch(""));
  }, [active?.repo_id, active?.deferred]);

  // Track whether the Changes view is active (for Cmd+Enter shortcut).
  const [changesViewFocused, setChangesViewFocused] = useState(false);

  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent<{ view: string }>).detail;
      setChangesViewFocused(detail?.view === "changes");
    };
    window.addEventListener("crab:active-view-changed", handler);
    return () => window.removeEventListener("crab:active-view-changed", handler);
  }, []);

  // Register all command palette keyboard shortcuts.
  useCommandPaletteShortcuts({
    context: {
      repoId: active?.repo_id ?? "",
      repoPath: active?.path,
      currentBranch: paletteBranch,
      upstream: null,
      ahead: 0,
      behind: 0,
      dirtyCount: 0,
      hasStagedChanges: false,
      activeOperation: null,
      forgeProvider: active?.forge_provider ?? null,
    },
    onOpenPalette: () => setPaletteOpen(true),
    isChangesViewFocused: changesViewFocused,
  });

  // Listen for the custom event dispatched by the `app.command-palette` command
  // so the palette can be opened programmatically (e.g. from Settings or itself).
  useEffect(() => {
    const handler = () => setPaletteOpen(true);
    window.addEventListener("crab:open-command-palette", handler);
    return () => window.removeEventListener("crab:open-command-palette", handler);
  }, []);

  // Listen for repo-opened events dispatched by commands (e.g. "Open sample repo")
  // that clone/open repos outside of the normal React component flow.
  useEffect(() => {
    const handler = (e: Event) => {
      const detail = (e as CustomEvent).detail as { handle: { repo_id: string; url: string; is_remote: boolean; cache_dir: string }; path: string } | undefined;
      if (!detail?.handle) return;
      addRepo({
        ...detail.handle,
        label: localPathName(detail.path),
        path: detail.path,
        openedAt: Date.now(),
      });
    };
    window.addEventListener("crab:repo-opened", handler);
    return () => window.removeEventListener("crab:repo-opened", handler);
  }, [addRepo]);

  // Keep the state store's ring buffer in sync with the Settings
  // value. Dispatches a `set-max-log-lines` action that both updates
  // the cap and trims any already-buffered overflow.
  useEffect(() => {
    setMaxLogLines(maxLogLines);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [maxLogLines]);

  // Reopen persisted repositories on first render, once. We keep a
  // ref guard so React Strict Mode's double-invoke doesn't open each
  // repo twice during dev. Failures per-repo are non-fatal — a repo
  // whose path no longer exists simply drops out of the list.
  //
  // Optimisation: only the last-active repo is opened immediately via
  // `rpc.openLocal`. The remaining repos are added as deferred entries
  // (no agent round-trip) and opened on-demand when the user selects
  // them in the sidebar. This avoids scanning every repo at startup.
  const reopenedRef = useRef(false);
  useEffect(() => {
    if (reopenedRef.current) return;
    reopenedRef.current = true;
    if (!reopenReposOnStartup) return;
    try {
      const raw = localStorage.getItem("crab.recentRepos.v1");
      const activePath = localStorage.getItem("crab.activeRepoPath.v1");
      if (!raw) return;
      const persisted = JSON.parse(raw) as Array<{ path: string; label: string; openedAt: number }>;
      if (!Array.isArray(persisted)) return;

      // Add non-active repos as deferred entries immediately (no RPC).
      for (const p of persisted) {
        if (p.path === activePath) continue; // handled below
        addRepo({
          repo_id: `deferred-${p.path}`,
          url: "",
          is_remote: false,
          cache_dir: "",
          label: p.label.replace(/^(↳\s*)wt:/, "$1worktree:"),
          path: p.path,
          openedAt: p.openedAt,
          deferred: true,
        });
      }

      // Open only the last-active repo eagerly.
      if (activePath) {
        const activeEntry = persisted.find((p) => p.path === activePath);
        if (activeEntry) {
          (async () => {
            try {
              const handle = await rpc.openLocal(activeEntry.path);
              addRepo({
                ...handle,
                label: activeEntry.label.replace(/^(↳\s*)wt:/, "$1worktree:"),
                path: activeEntry.path,
                openedAt: activeEntry.openedAt,
              });
              setActive(handle.repo_id);
            } catch {
              // Agent down or path gone — skip.
            }
          })();
        }
      }
    } catch {
      // Corrupt localStorage blob — ignore.
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The outer "Repositories" sidebar persists its state to localStorage.
  useEffect(() => {
    localStorage.setItem(LS_REPO_SIDEBAR, repoSidebarOpen ? "1" : "0");
  }, [repoSidebarOpen]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && !e.altKey && e.key.toLowerCase() === "b") {
        e.preventDefault();
        setRepoSidebarOpen((o) => !o);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // Terminal keep-alive ref and settings.
  const terminalKeepAliveRef = useRef<TerminalKeepAliveHandle>(null);
  const { settings } = useSettings();
  const sessionPersistence = settings.sessionPersistence;
  const terminalKeepAliveLimit = sessionPersistence.terminalKeepAliveLimit ?? 5;
  // When terminalKeepAlive is disabled, only the active repo's terminal is rendered (maxKeptAlive=1).
  const effectiveKeepAliveLimit = sessionPersistence.terminalKeepAlive ? terminalKeepAliveLimit : 1;

  // Wire the enabled setting into SessionManager reactively — changes take effect immediately.
  useEffect(() => {
    sessionManager.setEnabled(sessionPersistence.enabled);
  }, [sessionManager, sessionPersistence.enabled]);

  useEffect(() => {
    const openSsh = (event: Event) => {
      setSshTerminalRequest((event as CustomEvent<SshTerminalRequest>).detail);
      openTerminal();
    };
    const spawnAgent = (event: Event) => {
      setStartupRequest((event as CustomEvent<TerminalStartupRequest>).detail);
      openTerminal();
    };
    window.addEventListener("crab:open-ssh-terminal-here", openSsh);
    window.addEventListener("crab:spawn-agent-terminal", spawnAgent);
    return () => {
      window.removeEventListener("crab:open-ssh-terminal-here", openSsh);
      window.removeEventListener("crab:spawn-agent-terminal", spawnAgent);
    };
  }, [openTerminal]);

  // Evict repos from terminal keep-alive when they're removed from the store.
  // Skip eviction during workspace switches — the suspend/resume flow handles that.
  const prevRepoIdsRef = useRef<string[]>(state.repos.map((r) => r.repo_id));
  const workspaceSwitchingRef = useRef(false);
  useEffect(() => {
    // Don't evict during a workspace switch — terminals are suspended, not killed.
    if (workspaceSwitchingRef.current) {
      prevRepoIdsRef.current = state.repos.map((r) => r.repo_id);
      return;
    }
    const currentIds = new Set(state.repos.map((r) => r.repo_id));
    for (const id of prevRepoIdsRef.current) {
      if (!currentIds.has(id)) {
        terminalKeepAliveRef.current?.evict(id);
      }
    }
    prevRepoIdsRef.current = state.repos.map((r) => r.repo_id);
  }, [state.repos]);

  // handleNavigate, handleBack, handleForward use the nav instance declared above.
  const handleNavigate = useCallback((entry: NavEntry) => {
    setTerminalRepoId(entry.view === "terminal" ? active?.repo_id ?? null : null);
    if (entry.view === "terminal") setTerminalActivated(true);
    // Restored entries already own this position in history. Compare routes
    // instead of suppressing user navigation until the next animation frame.
    if (entry.view === nav.current?.view && (entry.commit ?? null) === (nav.current.commit ?? null)) return;
    // Attach the current workspace repo path to the entry for cross-repo nav.
    nav.push({ ...entry, repoPath: entry.repoPath ?? activeWorkspaceRelativePath });
  }, [nav, activeWorkspaceRelativePath, active?.repo_id]);

  const handleBack = useCallback(() => {
    nav.back();
  }, [nav]);

  const handleForward = useCallback(() => {
    nav.forward();
  }, [nav]);

  // Keyboard shortcuts: Cmd+[ / Cmd+] for back/forward.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey) {
        if (e.key === "[") { e.preventDefault(); handleBack(); }
        else if (e.key === "]") { e.preventDefault(); handleForward(); }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [handleBack, handleForward]);

  const handleSpawnAgentTerminal = useCallback((command: StartupCommand) => {
    window.dispatchEvent(new CustomEvent("crab:spawn-agent-terminal", { detail: command }));
  }, []);

  return (
    <div className={`flex h-full w-full flex-col bg-gx-bg text-gx-text ${showActivityBar ? "[--activity-bar-width:3rem]" : "[--activity-bar-width:0px]"}`}>
      <TopBar
        repoSidebarOpen={repoSidebarOpen}
        onExpandRepoSidebar={() => setRepoSidebarOpen(true)}
        activeRepo={active}
        canBack={nav.canBack}
        canForward={nav.canForward}
        onBack={handleBack}
        onForward={handleForward}
        onShowOverview={() => window.dispatchEvent(new CustomEvent("crab:show-repository-overview"))}
        terminalActive={terminalActive}
        onOpenTerminal={openTerminal}
        onSpawnAgentTerminal={handleSpawnAgentTerminal}
        workspaceName={workspaceActive ? (getActiveWorkspace(state)?.name ?? null) : null}
        workspaceRepos={workspaceActive ? activeWorkspaceRepos : undefined}
        activeWorkspaceRelativePath={activeWorkspaceRelativePath}
        onWorkspaceRepoSelect={workspaceActive ? handleTabSelect : undefined}
      />
      <UpdateBanner />
      {/* Tab bar removed — workspace repos are now accessible via the
          TopBar dropdown. The tab-bar-activate action still tracks
          recently accessed repos for ⌘1–⌘9 shortcuts. */}
      <div className="flex min-h-0 flex-1">
        {repoSidebarOpen && (
          <Sidebar onCollapse={() => setRepoSidebarOpen(false)} />
        )}
        <main className="relative min-h-0 min-w-0 flex-1 overflow-hidden">
          {active ? (
            <RepoDetail
              key={active.repo_id}
              repo={active}
              restoreEntry={nav.restoreEntry}
              onNavigate={handleNavigate}
              activityDrawerOpen={drawerOpen}
              onToggleActivityDrawer={() => setDrawerOpen((value) => !value)}
            />
          ) : state.workspaces.length > 0 ? (
            <WorkspaceDashboardView workspace={getActiveWorkspace(state)!} />
          ) : (
            <EmptyState />
          )}
          {/* Share the rail width with WorkspaceShell; keep one host mounted
              across page and repository changes so PTYs are never recreated. */}
          <section aria-label="Terminal page" className={terminalActive ? "absolute inset-y-0 right-0 left-[var(--activity-bar-width)] flex min-h-0 bg-gx-bg-sub" : "hidden"}>
            {terminalActivated && (
              <TerminalKeepAlive
                ref={terminalKeepAliveRef}
                repos={state.repos}
                activeRepoId={terminalActive ? active?.repo_id ?? null : null}
                maxKeptAlive={effectiveKeepAliveLimit}
                sshRequest={sshTerminalRequest}
                onSshRequestHandled={() => setSshTerminalRequest(null)}
                startupRequest={startupRequest}
                onStartupRequestHandled={() => setStartupRequest(null)}
              />
            )}
          </section>
        </main>
      </div>
      <ProgressDrawer open={drawerOpen} onClose={() => setDrawerOpen(false)} />
      <TelemetryOptInModal />
      <WorkspaceThemeEffect />
      <CommandPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        repoId={active?.repo_id ?? ""}
        context={{
          repoId: active?.repo_id ?? "",
          repoPath: active?.path,
          currentBranch: paletteBranch,
          upstream: null,
          ahead: 0,
          behind: 0,
          dirtyCount: 0,
          hasStagedChanges: false,
          activeOperation: null,
          forgeProvider: active?.forge_provider ?? null,
        }}
      />
      <ToastManager />
      <Welcome />
      {fuzzySwitcher.open && state.workspaces.length > 0 && (
        <FuzzySwitcher
          repos={allWorkspaceRepos}
          onSelect={handleFuzzySelect}
          onClose={fuzzySwitcher.closeSwitcher}
        />
      )}
    </div>
  );
}

export default function App() {
  return (
    <StoreProvider>
      <SettingsProvider>
        <PtyModeProvider>
          <SessionProvider>
            <Shell />
          </SessionProvider>
        </PtyModeProvider>
      </SettingsProvider>
    </StoreProvider>
  );
}
