import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import { matchesRepository, workspaceIdentity } from "../lib/workspace-sidebar";
import { localPathName } from "../lib/local-path-name";
import {
  ChevronDown,
  ChevronRight,
  Cloud,
  Download,
  EllipsisVertical,
  FolderGit2,
  FolderOpen,
  FolderPlus,
  FolderTree,
  GripVertical,
  Lock,
  LockOpen,
  Loader2,
  PanelLeftClose,
  Pin,
  PinOff,
  Plus,
  Scissors,
  Search,
  Trash2,
  X,
} from "lucide-react";
import {
  DndContext,
  closestCenter,
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
  DragEndEvent,
} from "@dnd-kit/core";
import {
  arrayMove,
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { useStore, OpenRepo, sortRepos, getActiveWorkspace } from "../state";
import { dialogs, rpc, WorktreeEntry } from "../ipc";
import { CloneDialog } from "./CloneDialog";
import { WorkspaceSidebar } from "./workspace/WorkspaceSidebar";
import { WorkspaceSettings } from "./workspace/WorkspaceSettings";
import { RemoteWorkspaceDialog } from "./workspace/RemoteWorkspaceDialog";
import { useWorkspaceRepoSelect } from "../hooks/useWorkspaceRepoSelect";

interface Props {
  /** Invoked when the user hits the icon-only collapse button. */
  onCollapse: () => void;
}

const LS_SIDEBAR_WIDTH = "crab.repoSidebarWidth";
const LS_SECTION_WORKSPACES = "crab.section.workspaces.collapsed";
const LS_SECTION_REPOS = "crab.section.repos.collapsed";
const MIN_WIDTH = 220;
const MAX_WIDTH = 480;
const DEFAULT_WIDTH = 260;

export function Sidebar({ onCollapse }: Props) {
  const { state, dispatch, addRepo, removeRepo, setActive, pinRepo, unpinRepo, reorderRepos } = useStore();
  const [busy, setBusy] = useState(false);
  const [query, setQuery] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);
  const scrollPosition = useRef(0);
  const restoringScroll = useRef(false);
  const activeWorkspace = getActiveWorkspace(state);
  const scrollKey = `crab.repoSidebarScroll.${activeWorkspace ? workspaceIdentity(activeWorkspace) : "standalone"}`;
  useLayoutEffect(() => {
    scrollPosition.current = Number(localStorage.getItem(scrollKey)) || 0;
    return () => localStorage.setItem(scrollKey, String(scrollPosition.current));
  }, [scrollKey]);
  useLayoutEffect(() => {
    restoringScroll.current = true;
    if (scrollRef.current) scrollRef.current.scrollTop = query.trim() ? 0 : scrollPosition.current;
    const frame = requestAnimationFrame(() => { restoringScroll.current = false; });
    return () => cancelAnimationFrame(frame);
  }, [scrollKey, query, state.workspaces, state.repos]);
  useLayoutEffect(() => {
    const saveScroll = () => {
      // Quit can precede queued scroll events. The mounted, unfiltered list
      // owns the visible offset; filtering keeps its prior unfiltered position.
      if (!query.trim() && scrollRef.current) scrollPosition.current = scrollRef.current.scrollTop;
      localStorage.setItem(scrollKey, String(scrollPosition.current));
    };
    window.addEventListener("beforeunload", saveScroll);
    return () => window.removeEventListener("beforeunload", saveScroll);
  }, [scrollKey, query]);
  const [cloneOpen, setCloneOpen] = useState(false);
  const [cloneTargetWorkspace, setCloneTargetWorkspace] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [addMenuOpen, setAddMenuOpen] = useState(false);
  const [workspacePickerAction, setWorkspacePickerAction] = useState<"clone" | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [remoteDialogOpen, setRemoteDialogOpen] = useState(false);
  const addMenuRef = useRef<HTMLDivElement>(null);

  // Collapsible section state (persisted)
  const [workspacesSectionCollapsed, setWorkspacesSectionCollapsed] = useState(() => {
    try { return localStorage.getItem(LS_SECTION_WORKSPACES) === "1"; } catch { return false; }
  });
  const [reposSectionCollapsed, setReposSectionCollapsed] = useState(() => {
    try { return localStorage.getItem(LS_SECTION_REPOS) === "1"; } catch { return false; }
  });

  useEffect(() => {
    try { localStorage.setItem(LS_SECTION_WORKSPACES, workspacesSectionCollapsed ? "1" : "0"); } catch { /* best-effort */ }
  }, [workspacesSectionCollapsed]);

  useEffect(() => {
    try { localStorage.setItem(LS_SECTION_REPOS, reposSectionCollapsed ? "1" : "0"); } catch { /* best-effort */ }
  }, [reposSectionCollapsed]);

  const selectWorkspaceRepo = useWorkspaceRepoSelect();

  // Listen for the "crab:add-remote-workspace" custom event to open the dialog
  useEffect(() => {
    function handleAddRemote(_e: Event) {
      setRemoteDialogOpen(true);
    }
    window.addEventListener("crab:add-remote-workspace", handleAddRemote);
    return () => window.removeEventListener("crab:add-remote-workspace", handleAddRemote);
  }, []);

  // Close the add menu when clicking outside
  useEffect(() => {
    if (!addMenuOpen) return;
    const handleClick = (e: MouseEvent) => {
      if (addMenuRef.current && !addMenuRef.current.contains(e.target as Node)) {
        setAddMenuOpen(false);
      }
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setAddMenuOpen(false);
    };
    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKey);
    return () => {
      document.removeEventListener("mousedown", handleClick);
      document.removeEventListener("keydown", handleKey);
    };
  }, [addMenuOpen]);

  // Determine the active workspace repo relative path for highlighting
  const activeWorkspaceRelativePath = useMemo(() => {
    if (state.workspaces.length === 0 || !state.activeRepoId) return null;
    for (const ws of state.workspaces) {
      const entry = ws.repos.find((e) => e.openRepoId === state.activeRepoId);
      if (entry) return entry.relativePath;
    }
    return null;
  }, [state.workspaces, state.activeRepoId]);

  // Standalone repos: repos not belonging to any workspace
  const standaloneRepos = useMemo(() => {
    if (state.workspaces.length === 0) return [];
    const workspacePaths = new Set(
      state.workspaces.flatMap((ws) => ws.repos.map((e) => e.absolutePath)),
    );
    return state.repos.filter(
      (r) => !r.label.startsWith("↳ ") && (!r.path || !workspacePaths.has(r.path)),
    );
  }, [state.workspaces, state.repos]);

  async function openWorkspace() {
    setAddMenuOpen(false);
    setError(null);
    const picked = await window.crab.workspace.pickDirectory();
    if (!picked) return;
    try {
      await window.crab.workspace.open(picked);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function createWorkspace() {
    setError(null);
    try {
      const result = await window.crab.workspace.create();
      if (!result.ok && result.error !== "cancelled") {
        setError(result.error ?? "Failed to create workspace");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  function handleCloseWorkspace(rootPath: string) {
    dispatch({ type: "workspace-close", rootPath });
    window.crab.workspace.close().catch(() => undefined);

    // Remove from persisted workspace list so it doesn't reopen on restart.
    try {
      const raw = localStorage.getItem("crab.workspace.v1");
      if (raw) {
        const paths: string[] = JSON.parse(raw);
        const updated = paths.filter((p) => p !== rootPath);
        localStorage.setItem("crab.workspace.v1", JSON.stringify(updated));
      }
    } catch {
      // best-effort
    }
  }

  function handleCloneToWorkspace() {
    if (state.workspaces.length === 0) {
      setCloneOpen(true);
      return;
    }
    // If multiple workspaces, show picker; otherwise proceed directly
    if (state.workspaces.length > 1) {
      setWorkspacePickerAction("clone");
      return;
    }
    setCloneTargetWorkspace(state.workspaces[0].rootPath);
    setCloneOpen(true);
  }

  function handleCloneToSpecificWorkspace(rootPath: string) {
    setCloneTargetWorkspace(rootPath);
    setCloneOpen(true);
  }

  function handleWorkspacePickerSelect(rootPath: string) {
    if (workspacePickerAction === "clone") {
      setWorkspacePickerAction(null);
      handleCloneToSpecificWorkspace(rootPath);
    }
  }

  async function handleRemoveRepoFromWorkspace(relativePath: string) {
    setError(null);
    try {
      const result = await window.crab.workspace.removeRepo(relativePath);
      if (!result.ok) {
        setError(result.error ?? "Failed to remove repo");
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  function handleRefreshWorkspace() {
    window.crab.workspace.refresh().catch(() => undefined);
  }

  // Handle selecting a repo — if it's deferred (not yet opened via the
  // agent), open it on demand before activating.
  const handleSelectRepo = useCallback(async (repo: OpenRepo) => {
    if (!repo.deferred) {
      setActive(repo.repo_id);
      return;
    }
    // Deferred entries carry the synthetic id `deferred-${path}`; the
    // agent only understands real filesystem paths, so route through
    // `repo.path`. Without this, the open call fails canonicalize and
    // surfaces as a generic "internal crab error".
    if (!repo.path) {
      setError("Cannot open repository: missing path");
      removeRepo(repo.repo_id);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const handle = await rpc.openLocal(repo.path);
      // Replace the deferred placeholder with the real handle.
      removeRepo(repo.repo_id);
      addRepo({
        ...handle,
        label: repo.label,
        path: repo.path,
        openedAt: Date.now(),
        initialView: "dashboard",
      });
      setActive(handle.repo_id);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      // Remove the broken deferred entry so it doesn't linger.
      removeRepo(repo.repo_id);
    } finally {
      setBusy(false);
    }
  }, [addRepo, removeRepo, setActive]);

  // Resizable width
  const [width, setWidth] = useState<number>(() => {
    const saved = localStorage.getItem(LS_SIDEBAR_WIDTH);
    const parsed = saved ? parseInt(saved, 10) : NaN;
    return Number.isFinite(parsed) ? Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, parsed)) : DEFAULT_WIDTH;
  });
  const resizing = useRef(false);

  useEffect(() => {
    localStorage.setItem(LS_SIDEBAR_WIDTH, String(width));
  }, [width]);

  const onResizeStart = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    resizing.current = true;
    const startX = e.clientX;
    const startWidth = width;
    const onMove = (ev: MouseEvent) => {
      if (!resizing.current) return;
      setWidth(Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, startWidth + (ev.clientX - startX))));
    };
    const onUp = () => {
      resizing.current = false;
      document.removeEventListener("mousemove", onMove);
      document.removeEventListener("mouseup", onUp);
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    };
    document.addEventListener("mousemove", onMove);
    document.addEventListener("mouseup", onUp);
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  }, [width]);

  async function openLocal() {
    setError(null);
    const picked = await dialogs.pickRepoDirectory();
    if (!picked) return;
    setBusy(true);
    try {
      const handle = await rpc.openLocal(picked);
      addRepo({ ...handle, label: localPathName(picked), path: picked, openedAt: Date.now() });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  // Group repos: hide repos whose path is a worktree of another open repo.
  // We show them as sub-items instead. For simplicity, we show all repos
  // as top-level entries — the worktree sub-items handle switching.
  // We filter out repos that were opened as worktrees (label starts with "↳").
  const visibleRepos = useMemo(
    () => sortRepos(
      state.repos.filter((r) => !r.label.startsWith("↳ ")),
      state.pinnedPaths,
      state.repoOrder,
    ),
    [state.repos, state.pinnedPaths, state.repoOrder],
  );

  // Drag-and-drop sensors
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 5 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  function handleDragEnd(event: DragEndEvent) {
    const { active, over } = event;
    if (!over || active.id === over.id) return;

    // Only allow reordering among unpinned repos
    const unpinnedRepos = visibleRepos.filter((r) => !r.path || !state.pinnedPaths.has(r.path));
    const oldIndex = unpinnedRepos.findIndex((r) => r.repo_id === active.id);
    const newIndex = unpinnedRepos.findIndex((r) => r.repo_id === over.id);
    if (oldIndex === -1 || newIndex === -1) return;

    const reordered = arrayMove(unpinnedRepos, oldIndex, newIndex);
    const newOrder = reordered
      .map((r) => r.path)
      .filter((p): p is string => typeof p === "string" && p.length > 0);
    reorderRepos(newOrder);
  }

  return (
    <aside
      className="relative flex shrink-0 flex-col border-r border-gx-border bg-gx-surface"
      style={{ width }}
    >
      <div className="flex items-center gap-1.5 p-3">
        <button type="button" className="btn h-[30px] w-[30px] shrink-0 justify-center px-0" onClick={openLocal} disabled={busy} title="Open Repository">
          <FolderOpen size={14} />
        </button>
        <button type="button" className="btn h-[30px] w-[30px] shrink-0 justify-center px-0" onClick={openWorkspace} disabled={busy} title="Open Workspace">
          <FolderGit2 size={14} />
        </button>
        <button type="button" className="btn h-[30px] w-[30px] shrink-0 justify-center px-0" onClick={() => setCloneOpen(true)} disabled={busy} title="Clone Repository">
          <Download size={14} />
        </button>
        {/* More actions dropdown */}
        <div className="relative" ref={addMenuRef}>
          <button
            type="button"
            className="btn h-[30px] w-[30px] shrink-0 justify-center px-0"
            onClick={() => setAddMenuOpen((v) => !v)}
            title="More actions"
            aria-label="More actions"
          >
            <EllipsisVertical size={14} />
          </button>
          {addMenuOpen && (
            <div className="absolute left-0 top-full z-50 mt-1 w-56 rounded border border-gx-border bg-gx-surface shadow-xl py-1">
              <button
                type="button"
                className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-gx-text hover:bg-gx-accent/10 transition"
                onClick={async () => { setAddMenuOpen(false); await createWorkspace(); }}
              >
                <FolderPlus size={12} /> Create Workspace
              </button>
              <div className="my-1 border-t border-gx-border" />
              <button
                type="button"
                className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-gx-text hover:bg-gx-accent/10 transition"
                onClick={() => { setAddMenuOpen(false); handleCloneToWorkspace(); }}
              >
                <Download size={12} /> Clone Repository to Workspace
              </button>
              <div className="my-1 border-t border-gx-border" />
              <button
                type="button"
                className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-gx-text hover:bg-gx-accent/10 transition"
                onClick={() => {
                  setAddMenuOpen(false);
                  setRemoteDialogOpen(true);
                }}
              >
                <Cloud size={12} /> Connect to Remote Workspace
              </button>
            </div>
          )}
        </div>
        <div className="flex-1" />
        <button
          type="button"
          className="btn h-[30px] w-[30px] shrink-0 justify-center px-0"
          onClick={onCollapse}
          title="Collapse Repositories (⇧⌘B)"
          aria-label="Collapse Repositories"
        >
          <PanelLeftClose size={14} />
        </button>
      </div>

      {error && (
        <div className="mx-3 mb-2 rounded border border-gx-error/40 bg-gx-error/10 p-2 text-xs text-gx-error">
          {error}
        </div>
      )}

      <div className="relative mx-3 mb-1 mt-1 flex shrink-0 items-center">
        <Search size={12} className="pointer-events-none absolute left-2 text-gx-muted" />
        <input
          aria-label="Filter repositories"
          placeholder="Filter repositories…"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Escape") { event.stopPropagation(); setQuery(""); }
          }}
          className="w-full rounded border border-gx-border bg-gx-bg-sub py-1.5 pl-7 pr-7 text-xs text-gx-text placeholder:text-gx-muted focus:border-gx-accent focus:outline-none"
        />
        {query && (
          <button type="button" aria-label="Clear repository filter" className="absolute right-1 rounded p-1 text-gx-muted hover:text-gx-text" onClick={() => setQuery("")}>
            <X size={12} />
          </button>
        )}
      </div>
      <div
        ref={scrollRef}
        aria-label="Repository list"
        className="min-h-0 flex-1 overflow-auto"
        onScroll={(event) => {
          if (!query.trim() && !restoringScroll.current) scrollPosition.current = event.currentTarget.scrollTop;
        }}
      >
        {/* ── Workspaces section (when any workspace is active) ── */}
        {state.workspaces.length > 0 && (
          <div>
            <button
              type="button"
              className="flex w-full items-center gap-1 px-3 pt-2 pb-1 text-[11px] uppercase tracking-wider text-gx-muted hover:text-gx-text transition"
              onClick={() => setWorkspacesSectionCollapsed((v) => !v)}
              aria-expanded={!workspacesSectionCollapsed || Boolean(query.trim())}
              aria-label="Toggle Workspaces section"
            >
              {workspacesSectionCollapsed ? <ChevronRight size={11} /> : <ChevronDown size={11} />}
              <span>Workspaces</span>
            </button>
            {(!workspacesSectionCollapsed || query.trim()) && (
              <div className="pl-2">
                {state.workspaces.map((ws) => (
                  <WorkspaceSidebar
                    key={workspaceIdentity(ws)}
                    workspace={ws}
                    query={query}
                    pinnedPaths={state.pinnedPaths}
                    onTogglePin={(path) => state.pinnedPaths.has(path) ? unpinRepo(path) : pinRepo(path)}
                    activeRelativePath={ws.repos.some((entry) => entry.openRepoId === state.activeRepoId) ? activeWorkspaceRelativePath : null}
                    onSelect={(path) => selectWorkspaceRepo(path, ws)}
                    onRemoveRepo={handleRemoveRepoFromWorkspace}
                    onRefresh={handleRefreshWorkspace}
                    onClose={() => handleCloseWorkspace(ws.rootPath)}
                    onSettings={() => setSettingsOpen(true)}
                    onShowDashboard={() => {
                      dispatch({ type: "set-active-workspace", rootPath: ws.rootPath });
                      setActive(null);
                    }}
                    onCloneRepository={() => handleCloneToSpecificWorkspace(ws.rootPath)}
                  />
                ))}
              </div>
            )}
          </div>
        )}

        {/* ── Repositories section ── */}
        {(() => {
          // When workspaces are active, show standalone repos; otherwise show all repos
          const reposToShow = (state.workspaces.length > 0 ? standaloneRepos : visibleRepos).filter((repo) => matchesRepository(query, repo.label, repo.path ?? repo.url));
          if (state.workspaces.length > 0 && reposToShow.length === 0) return null;
          return (
            <div>
              <button
                type="button"
                className="flex w-full items-center gap-1 px-3 pt-2 pb-1 text-[11px] uppercase tracking-wider text-gx-muted hover:text-gx-text transition"
                onClick={() => setReposSectionCollapsed((v) => !v)}
                aria-expanded={!reposSectionCollapsed || Boolean(query.trim())}
                aria-label="Toggle Repositories section"
              >
                {reposSectionCollapsed ? <ChevronRight size={11} /> : <ChevronDown size={11} />}
                <span>Repositories</span>
              </button>
              {(!reposSectionCollapsed || query.trim()) && (
                <ul className="mt-1 pl-3 pr-2">
                  {reposToShow.length === 0 && (
                    <li className="px-2 py-6 text-center text-xs text-gx-muted">{query.trim() ? "No matching repositories" : "No repositories open yet."}</li>
                  )}
                  {/* Pinned repos (not draggable, always on top, alphabetic) */}
                  {reposToShow.filter((r) => r.path && state.pinnedPaths.has(r.path)).map((repo) => (
                    <RepoSidebarEntry
                      key={repo.repo_id}
                      repo={repo}
                      activeRepoId={state.activeRepoId}
                      allRepos={state.repos}
                      isPinned={true}
                      onPin={() => repo.path && pinRepo(repo.path)}
                      onUnpin={() => repo.path && unpinRepo(repo.path)}
                      onSelect={() => handleSelectRepo(repo)}
                      onClose={() => {
                        void rpc.close(repo.repo_id).catch(() => undefined);
                        removeRepo(repo.repo_id);
                      }}
                      onActivateWorktree={(repoId) => setActive(repoId)}
                      onOpenWorktreePath={async (wtPath) => {
                        const existing = state.repos.find((r) => r.path === wtPath);
                        if (existing) {
                          setActive(existing.repo_id);
                          return;
                        }
                        setBusy(true);
                        setError(null);
                        try {
                          const handle = await rpc.openLocal(wtPath);
                          addRepo({
                            ...handle,
                            label: `↳ worktree:${localPathName(wtPath)}`,
                            path: wtPath,
                            openedAt: Date.now(),
                          });
                          setActive(handle.repo_id);
                        } catch (e) {
                          setError(e instanceof Error ? e.message : String(e));
                        } finally {
                          setBusy(false);
                        }
                      }}
                    />
                  ))}
                  {/* Unpinned repos (draggable, user-ordered or alphabetic) */}
                  <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={handleDragEnd}>
                    <SortableContext
                      items={reposToShow.filter((r) => !r.path || !state.pinnedPaths.has(r.path)).map((r) => r.repo_id)}
                      strategy={verticalListSortingStrategy}
                    >
                      {reposToShow.filter((r) => !r.path || !state.pinnedPaths.has(r.path)).map((repo) => (
                        <SortableRepoEntry
                          key={repo.repo_id}
                          repo={repo}
                          activeRepoId={state.activeRepoId}
                          allRepos={state.repos}
                          isPinned={false}
                          onPin={() => repo.path && pinRepo(repo.path)}
                          onUnpin={() => repo.path && unpinRepo(repo.path)}
                          onSelect={() => handleSelectRepo(repo)}
                          onClose={() => {
                            void rpc.close(repo.repo_id).catch(() => undefined);
                            removeRepo(repo.repo_id);
                          }}
                          onActivateWorktree={(repoId) => setActive(repoId)}
                          onOpenWorktreePath={async (wtPath) => {
                            const existing = state.repos.find((r) => r.path === wtPath);
                            if (existing) {
                              setActive(existing.repo_id);
                              return;
                            }
                            setBusy(true);
                            setError(null);
                            try {
                              const handle = await rpc.openLocal(wtPath);
                              addRepo({
                                ...handle,
                                label: `↳ worktree:${localPathName(wtPath)}`,
                                path: wtPath,
                                openedAt: Date.now(),
                              });
                              setActive(handle.repo_id);
                            } catch (e) {
                              setError(e instanceof Error ? e.message : String(e));
                            } finally {
                              setBusy(false);
                            }
                          }}
                        />
                      ))}
                    </SortableContext>
                  </DndContext>
                </ul>
              )}
            </div>
          );
        })()}
      </div>

      <CloneDialog
        open={cloneOpen}
        onClose={() => { setCloneOpen(false); setCloneTargetWorkspace(null); }}
        targetWorkspaceRootPath={cloneTargetWorkspace}
      />

      {/* Workspace picker modal — shown when multiple workspaces exist and user
          needs to choose which one to add/clone into */}
      {workspacePickerAction && state.workspaces.length > 1 && (
        <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/50">
          <div className="w-72 rounded-lg border border-gx-border bg-gx-surface shadow-2xl p-4">
            <h3 className="text-sm font-semibold text-gx-text mb-1">
              {workspacePickerAction === "clone" ? "Clone Repository to Workspace" : "Select Workspace"}
            </h3>
            <p className="text-[11px] text-gx-muted mb-3">
              Select the workspace to target:
            </p>
            <ul className="flex flex-col gap-1">
              {state.workspaces.map((ws) => (
                <li key={ws.rootPath}>
                  <button
                    type="button"
                    className="flex w-full items-center gap-2 rounded px-3 py-2 text-left text-xs text-gx-text hover:bg-gx-accent/10 border border-gx-border hover:border-gx-accent/50 transition"
                    onClick={() => handleWorkspacePickerSelect(ws.rootPath)}
                  >
                    <FolderGit2 size={13} className="shrink-0 text-gx-accent" />
                    <div className="min-w-0 flex-1">
                      <div className="truncate font-medium">{ws.name}</div>
                      <div className="truncate text-[10px] text-gx-muted">{ws.rootPath}</div>
                    </div>
                  </button>
                </li>
              ))}
            </ul>
            <button
              type="button"
              className="mt-3 w-full rounded px-3 py-1.5 text-xs text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover border border-gx-border transition"
              onClick={() => setWorkspacePickerAction(null)}
            >
              Cancel
            </button>
          </div>
        </div>
      )}

      {/* Workspace settings modal */}
      {settingsOpen && state.workspaces.length > 0 && (
        <WorkspaceSettings
          workspace={getActiveWorkspace(state)!}
          onClose={() => setSettingsOpen(false)}
        />
      )}

      {/* Remote workspace dialog */}
      <RemoteWorkspaceDialog
        open={remoteDialogOpen}
        onClose={() => setRemoteDialogOpen(false)}
        sshConnections={state.sshConnections}
      />

      {/* Resize handle */}
      <div
        className="absolute right-0 top-0 h-full w-[4px] cursor-col-resize hover:bg-gx-accent/40 active:bg-gx-accent/60 transition-colors z-10"
        onMouseDown={onResizeStart}
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize sidebar"
      />
    </aside>
  );
}

// ── Per-repo entry with inline worktree management ──────────────────

interface RepoEntryProps {
  repo: OpenRepo;
  activeRepoId: string | null;
  allRepos: OpenRepo[];
  isPinned: boolean;
  onPin: () => void;
  onUnpin: () => void;
  onSelect: () => void;
  onClose: () => void;
  onActivateWorktree: (repoId: string) => void;
  onOpenWorktreePath: (path: string) => Promise<void>;
  /** Drag handle props — only present for sortable (unpinned) entries */
  dragHandleProps?: {
    attributes: React.HTMLAttributes<HTMLElement>;
    listeners: React.HTMLAttributes<HTMLElement> | undefined;
  };
}

/** Wrapper that makes an unpinned repo entry sortable via dnd-kit */
function SortableRepoEntry(props: Omit<RepoEntryProps, "dragHandleProps">) {
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } = useSortable({
    id: props.repo.repo_id,
  });

  const style: React.CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
    opacity: isDragging ? 0.5 : undefined,
    position: "relative",
    zIndex: isDragging ? 50 : undefined,
  };

  return (
    <div ref={setNodeRef} style={style}>
      <RepoSidebarEntry
        {...props}
        dragHandleProps={{ attributes, listeners }}
      />
    </div>
  );
}

// Track which repo paths are expanded — survives component remounts
// during the deferred→real repo transition.
const expandedPaths = new Set<string>();

function RepoSidebarEntry({
  repo,
  activeRepoId,
  allRepos,
  isPinned,
  onPin,
  onUnpin,
  onSelect,
  onClose,
  onActivateWorktree,
  onOpenWorktreePath,
  dragHandleProps,
}: RepoEntryProps) {
  const [expanded, setExpanded] = useState(() => !!(repo.path && expandedPaths.has(repo.path)));
  const [worktrees, setWorktrees] = useState<WorktreeEntry[] | null>(null);
  const [wtLoading, setWtLoading] = useState(false);
  const [wtError, setWtError] = useState<string | null>(null);
  const [wtErrorPath, setWtErrorPath] = useState<string | null>(null);
  const [showAddForm, setShowAddForm] = useState(false);
  const [busy, setBusy] = useState(false);

  const fetchWorktrees = useCallback(() => {
    if (!repo.path || repo.deferred) return;
    setWtLoading(true);
    setWtError(null);
    rpc
      .gitWorktreeList(repo.repo_id)
      .then((r) => setWorktrees(r.worktrees))
      .catch((e: Error) => setWtError(e.message))
      .finally(() => setWtLoading(false));
  }, [repo.repo_id, repo.path, repo.deferred]);

  useEffect(() => {
    if (expanded && worktrees === null && !wtLoading) {
      fetchWorktrees();
    }
  }, [expanded, worktrees, wtLoading, fetchWorktrees]);

  const allWorktrees = worktrees ?? [];
  const linkedWorktrees = allWorktrees.filter((wt) => !wt.is_main);

  // Determine if this repo or any of its worktrees is the active one
  const isThisRepoActive = activeRepoId === repo.repo_id;
  const activeWorktreeRepo = allRepos.find(
    (r) => r.repo_id === activeRepoId && r.path && allWorktrees.some((wt) => wt.path === r.path),
  );
  const isAnyWorktreeActive = isThisRepoActive || !!activeWorktreeRepo;

  // Find which worktree path is currently active
  const activeWtPath = activeWorktreeRepo?.path ?? (isThisRepoActive ? repo.path : null);

  function shortBranch(branch: string): string {
    if (branch.startsWith("refs/heads/")) return branch.slice("refs/heads/".length);
    if (branch.startsWith("refs/")) return branch.slice("refs/".length);
    return branch;
  }

  function dirName(p: string): string {
    const parts = p.split("/");
    return parts[parts.length - 1] || p;
  }

  async function handleSelectWorktree(wt: WorktreeEntry) {
    if (wt.is_main) {
      // Main worktree = the repo itself
      onSelect();
    } else {
      // Check if this worktree path is already opened as a repo
      const existing = allRepos.find((r) => r.path === wt.path);
      if (existing) {
        onActivateWorktree(existing.repo_id);
      } else {
        await onOpenWorktreePath(wt.path);
      }
    }
  }

  async function handleRemove(wtPath: string, isLocked: boolean) {
    setBusy(true);
    setWtError(null);
    setWtErrorPath(null);
    try {
      await rpc.gitWorktreeRemove(repo.repo_id, wtPath, { force: isLocked });
      fetchWorktrees();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setWtError(msg);
      setWtErrorPath(wtPath);
    } finally {
      setBusy(false);
    }
  }

  async function handleForceRemove() {
    if (!wtErrorPath) return;
    setBusy(true);
    setWtError(null);
    setWtErrorPath(null);
    try {
      await rpc.gitWorktreeRemove(repo.repo_id, wtErrorPath, { force: true });
      fetchWorktrees();
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      setWtError(msg);
      setWtErrorPath(wtErrorPath);
    } finally {
      setBusy(false);
    }
  }

  async function handleLock(wtPath: string) {
    setBusy(true);
    setWtError(null);
    try {
      await rpc.gitWorktreeLock(repo.repo_id, wtPath);
      fetchWorktrees();
    } catch (e) {
      setWtError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleUnlock(wtPath: string) {
    setBusy(true);
    setWtError(null);
    try {
      await rpc.gitWorktreeUnlock(repo.repo_id, wtPath);
      fetchWorktrees();
    } catch (e) {
      setWtError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handlePrune() {
    setBusy(true);
    setWtError(null);
    try {
      await rpc.gitWorktreePrune(repo.repo_id);
      fetchWorktrees();
    } catch (e) {
      setWtError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <li className="mb-1">
      {/* ── Repo header ── */}
      <div className="group flex w-full items-center gap-1 rounded text-sm">
        {/* Drag handle (only for unpinned/sortable entries) */}
        {dragHandleProps && (
          <span
            className="shrink-0 cursor-grab p-0.5 text-gx-muted/50 hover:text-gx-muted active:cursor-grabbing"
            {...dragHandleProps.attributes}
            {...dragHandleProps.listeners}
            aria-label="Drag to reorder"
          >
            <GripVertical size={12} />
          </span>
        )}
        <button
          type="button"
          className="shrink-0 p-0.5 text-gx-muted hover:text-gx-text"
          onClick={() => {
            const willExpand = !expanded;
            setExpanded(willExpand);
            if (repo.path) {
              if (willExpand) expandedPaths.add(repo.path);
              else expandedPaths.delete(repo.path);
            }
            if (willExpand && repo.deferred) onSelect();
          }}
          aria-label={expanded ? "Collapse" : "Expand"}
        >
          {expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        </button>
        <button
          type="button"
          onClick={onSelect}
          className={clsx(
            "flex min-w-0 flex-1 items-center gap-1.5 rounded px-1.5 py-1.5 text-left transition",
            isAnyWorktreeActive ? "bg-gx-accent/15 text-gx-text" : "text-gx-muted hover:bg-gx-surface-hover",
          )}
        >
          {isPinned && <Pin size={10} className="shrink-0 text-gx-accent" />}
          <span className={clsx("min-w-0 flex-1 truncate font-medium", repo.deferred && "opacity-60")}>{repo.label}</span>
          {linkedWorktrees.length > 0 && (
            <span className="shrink-0 rounded bg-gx-accent/15 px-1.5 py-0.5 text-[9px] font-medium text-gx-accent">
              {linkedWorktrees.length + 1}
            </span>
          )}
        </button>
        {/* Pin/Unpin button */}
        <span
          className={clsx(
            "shrink-0 p-0.5 transition cursor-pointer",
            isPinned
              ? "text-gx-accent hover:text-gx-muted"
              : "opacity-0 group-hover:opacity-100 text-gx-muted hover:text-gx-accent",
          )}
          onClick={isPinned ? onUnpin : onPin}
          role="button"
          aria-label={isPinned ? "Unpin repository" : "Pin repository"}
          title={isPinned ? "Unpin" : "Pin to top"}
        >
          {isPinned ? <PinOff size={12} /> : <Pin size={12} />}
        </span>
        <span
          className="shrink-0 p-0.5 opacity-0 transition group-hover:opacity-100 text-gx-muted hover:text-gx-text cursor-pointer"
          onClick={onClose}
          role="button"
          aria-label="Close repository"
        >
          <X size={13} />
        </span>
      </div>

      {/* Collapsed info */}
      {!expanded && (
        <div className="truncate pl-7 pr-3 pb-0.5 font-mono text-[10px] text-gx-muted">
          {repo.url}
        </div>
      )}

      {/* ── Expanded: worktree list ── */}
      {expanded && (
        <div className="mt-0.5 ml-3 border-l-2 border-gx-border/40 pl-2 pb-0">
          {/* Header */}
          <div className="flex items-center gap-1.5 py-1 text-[10px] text-gx-muted">
            <FolderTree size={10} />
            <span className="font-medium uppercase tracking-wider">Worktrees</span>
            <div className="ml-auto flex gap-1">
              <button
                type="button"
                className="rounded p-0.5 hover:bg-gx-surface-hover hover:text-gx-text"
                onClick={() => setShowAddForm((v) => !v)}
                title="Add worktree"
              >
                <Plus size={11} />
              </button>
              <button
                type="button"
                className="rounded p-0.5 hover:bg-gx-surface-hover hover:text-gx-text"
                onClick={handlePrune}
                title="Prune stale"
                disabled={busy}
              >
                <Scissors size={11} />
              </button>
            </div>
          </div>

          {/* Add form */}
          {showAddForm && (
            <AddWorktreeForm
              repoId={repo.repo_id}
              onCreated={() => { setShowAddForm(false); fetchWorktrees(); }}
              onCancel={() => setShowAddForm(false)}
              checkedOutBranches={allWorktrees.map((wt) => wt.branch)}
            />
          )}

          {/* Loading */}
          {wtLoading && (
            <div className="flex items-center gap-1.5 py-2 text-[10px] text-gx-muted">
              <Loader2 size={10} className="animate-spin" /> Loading…
            </div>
          )}

          {/* Error */}
          {wtError && (
            <div className="rounded border border-gx-error/40 bg-gx-error/10 px-2 py-1.5 text-[10px] text-gx-error mb-1">
              <p className="break-words whitespace-pre-wrap">{wtError}</p>
              {wtErrorPath && /modified or untracked|--force/.test(wtError) && (
                <div className="mt-1.5 flex justify-end">
                  <button
                    type="button"
                    className="rounded bg-gx-error/20 px-2 py-0.5 font-medium hover:bg-gx-error/30 transition"
                    onClick={handleForceRemove}
                    disabled={busy}
                  >
                    Force Remove
                  </button>
                </div>
              )}
            </div>
          )}

          {/* Worktree items */}
          {!wtLoading && allWorktrees.map((wt) => {
            const isActive = activeWtPath === wt.path;

            return (
              <div
                key={wt.path}
                className={clsx(
                  "group/wt rounded px-1.5 py-1.5 cursor-pointer transition mb-0.5",
                  isActive
                    ? "bg-gx-accent/12 ring-1 ring-gx-accent/40"
                    : "hover:bg-gx-surface-hover",
                )}
                onClick={() => handleSelectWorktree(wt)}
                role="button"
                tabIndex={0}
                aria-label={`Switch to worktree ${dirName(wt.path)}`}
              >
                <div className="flex items-center gap-1.5">
                  <FolderTree size={11} className={clsx("shrink-0", isActive ? "text-gx-accent" : "text-gx-muted")} />
                  <span className={clsx("min-w-0 flex-1 truncate text-[12px] font-medium", isActive ? "text-gx-text" : "text-gx-muted")}>
                    {dirName(wt.path)}
                  </span>
                  {/* Hover action icons for non-main worktrees */}
                  {!wt.is_main && (
                    <span className="shrink-0 flex items-center gap-0.5 opacity-0 group-hover/wt:opacity-100 transition">
                      {wt.is_locked ? (
                        <button
                          type="button"
                          className="rounded p-0.5 text-gx-muted hover:text-gx-warn hover:bg-gx-surface-hover"
                          onClick={(e) => { e.stopPropagation(); handleUnlock(wt.path); }}
                          disabled={busy}
                          title="Unlock worktree"
                          aria-label="Unlock worktree"
                        >
                          <LockOpen size={11} />
                        </button>
                      ) : (
                        <button
                          type="button"
                          className="rounded p-0.5 text-gx-muted hover:text-gx-warn hover:bg-gx-surface-hover"
                          onClick={(e) => { e.stopPropagation(); handleLock(wt.path); }}
                          disabled={busy}
                          title="Lock worktree"
                          aria-label="Lock worktree"
                        >
                          <Lock size={11} />
                        </button>
                      )}
                      <button
                        type="button"
                        className="rounded p-0.5 text-gx-muted hover:text-gx-error hover:bg-gx-surface-hover"
                        onClick={(e) => { e.stopPropagation(); handleRemove(wt.path, wt.is_locked); }}
                        disabled={busy}
                        title="Remove worktree"
                        aria-label="Remove worktree"
                      >
                        <Trash2 size={11} />
                      </button>
                    </span>
                  )}
                  {wt.is_main && (
                    <span className="shrink-0 rounded bg-gx-info/20 px-1 py-0.5 text-[8px] font-bold text-gx-info">
                      MAIN
                    </span>
                  )}
                  {isActive && (
                    <span className="shrink-0 w-1.5 h-1.5 rounded-full bg-gx-accent" title="Active" />
                  )}
                  {wt.is_locked && (
                    <span title={wt.lock_reason ?? "Locked"}>
                      <Lock size={9} className="text-gx-warn" />
                    </span>
                  )}
                  {wt.is_prunable && (
                    <span className="shrink-0 rounded bg-gx-error/20 px-1 py-0.5 text-[8px] font-bold text-gx-error">
                      STALE
                    </span>
                  )}
                </div>

                {/* Branch */}
                <div className="mt-0.5 pl-[18px] text-[10px] text-gx-muted">
                  {wt.is_detached ? (
                    <span className="italic">detached @ {wt.head.slice(0, 7)}</span>
                  ) : wt.branch ? (
                    <span className="font-mono">{shortBranch(wt.branch)}</span>
                  ) : null}
                </div>

                {/* Path */}
                <div className="mt-0.5 pl-[18px] truncate font-mono text-[9px] text-gx-muted/50" title={wt.path}>
                  {wt.path}
                </div>
              </div>
            );
          })}

          {!wtLoading && worktrees && allWorktrees.length === 0 && (
            <div className="py-2 text-center text-[10px] text-gx-muted italic">No worktrees found.</div>
          )}
        </div>
      )}
    </li>
  );
}

// ── Add worktree form ───────────────────────────────────────────────

interface AddFormProps {
  repoId: string;
  onCreated: () => void;
  onCancel: () => void;
  checkedOutBranches: string[];
}

type AddMode = "existing" | "new-branch" | "detached";

function AddWorktreeForm({ repoId, onCreated, onCancel, checkedOutBranches }: AddFormProps) {
  const [mode, setMode] = useState<AddMode>("new-branch");
  const [path, setPath] = useState("");
  const [branch, setBranch] = useState("");
  const [startPoint, setStartPoint] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Refs and commits for searchable pickers
  const [refs, setRefs] = useState<{ name: string; target: string; kind: string }[] | null>(null);
  const [commits, setCommits] = useState<{ sha: string; subject: string; author: string; date: string }[] | null>(null);
  const [refsLoading, setRefsLoading] = useState(false);
  const [commitsLoading, setCommitsLoading] = useState(false);
  const [showBranchPicker, setShowBranchPicker] = useState(false);
  const [showStartPointPicker, setShowStartPointPicker] = useState(false);
  const [showDetachedPicker, setShowDetachedPicker] = useState(false);

  // Fetch refs on mount
  useEffect(() => {
    setRefsLoading(true);
    rpc
      .listRefs(repoId)
      .then((r) => setRefs(r.refs))
      .catch(() => setRefs([]))
      .finally(() => setRefsLoading(false));
  }, [repoId]);

  // Fetch recent commits on mount
  useEffect(() => {
    setCommitsLoading(true);
    rpc
      .gitLog(repoId, { max: 80, all: true })
      .then((r) => setCommits(r.commits))
      .catch(() => setCommits([]))
      .finally(() => setCommitsLoading(false));
  }, [repoId]);

  // Normalize checked-out branch names for comparison
  const checkedOutSet = useMemo(() => {
    const set = new Set<string>();
    for (const b of checkedOutBranches) {
      set.add(b.replace(/^refs\/heads\//, ""));
    }
    return set;
  }, [checkedOutBranches]);

  // Build branch options — mark branches already checked out in another worktree
  const branchOptions = useMemo(() => {
    if (!refs) return [];
    return refs
      .filter((r) => r.kind === "branch")
      .map((r) => {
        const short = r.name.replace(/^refs\/heads\//, "");
        const inUse = checkedOutSet.has(short);
        return {
          value: short,
          label: inUse ? `${short} (in use)` : short,
          detail: inUse ? "Already checked out in another worktree" : r.target?.slice(0, 7),
          disabled: inUse,
        };
      });
  }, [refs, checkedOutSet]);

  // Build commit/tag options for start point and detached pickers
  const commitTagOptions = useMemo(() => {
    const opts: { value: string; label: string; detail?: string }[] = [];
    if (refs) {
      for (const r of refs) {
        if (r.kind === "tag") {
          const short = r.name.replace(/^refs\/tags\//, "");
          opts.push({ value: short, label: `🏷 ${short}`, detail: r.target?.slice(0, 7) });
        }
      }
      for (const r of refs) {
        if (r.kind === "branch") {
          const short = r.name.replace(/^refs\/heads\//, "");
          opts.push({ value: short, label: `⎇ ${short}`, detail: r.target?.slice(0, 7) });
        }
      }
    }
    if (commits) {
      for (const c of commits) {
        opts.push({
          value: c.sha.slice(0, 7),
          label: c.subject.length > 45 ? c.subject.slice(0, 45) + "…" : c.subject,
          detail: `${c.sha.slice(0, 7)} · ${c.author}`,
        });
      }
    }
    return opts;
  }, [refs, commits]);

  async function handlePickPath() {
    const selected = await window.crab.pickWorktreePath();
    if (selected) setPath(selected);
  }

  async function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (!path.trim()) return;
    setBusy(true);
    setError(null);
    try {
      if (mode === "new-branch") {
        await rpc.gitWorktreeAdd(repoId, {
          path: path.trim(),
          branch: branch.trim() || undefined,
          create_branch: true,
          start_point: startPoint.trim() || undefined,
        });
      } else if (mode === "detached") {
        await rpc.gitWorktreeAdd(repoId, {
          path: path.trim(),
          branch: branch.trim() || undefined,
          detach: true,
        });
      } else {
        await rpc.gitWorktreeAdd(repoId, {
          path: path.trim(),
          branch: branch.trim() || undefined,
        });
      }
      onCreated();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={handleSubmit} className="mb-2 rounded border border-gx-border bg-gx-bg-sub p-2">
      <div className="mb-1.5 text-[10px] font-medium text-gx-text">New Worktree</div>
      <div className="mb-2 flex rounded-md border border-gx-border overflow-hidden text-[10px]">
        {(["existing", "new-branch", "detached"] as const).map((m) => (
          <button
            key={m}
            type="button"
            className={clsx(
              "flex-1 px-2 py-1 transition text-center",
              mode === m ? "bg-gx-accent/20 text-gx-text font-medium" : "text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover",
            )}
            onClick={() => setMode(m)}
          >
            {m === "existing" ? "Existing" : m === "new-branch" ? "New Branch" : "Detached"}
          </button>
        ))}
      </div>

      {/* Path — folder picker button */}
      <div className="mb-1.5 flex items-center gap-1">
        <button
          type="button"
          className="flex flex-1 items-center justify-between rounded border border-gx-border bg-gx-bg px-2 py-1 text-left text-[11px] transition hover:border-gx-muted/60 focus-visible:outline-none focus:border-gx-accent"
          onClick={handlePickPath}
          title="Select worktree folder"
        >
          <span className={path ? "truncate text-gx-text" : "text-gx-muted/60"}>
            {path || "Select location…"}
          </span>
          <FolderOpen size={11} className="shrink-0 text-gx-muted" />
        </button>
        {path && (
          <button
            type="button"
            className="shrink-0 rounded p-0.5 text-gx-muted hover:text-gx-text"
            onClick={() => setPath("")}
            title="Clear"
            aria-label="Clear path"
          >
            <X size={10} />
          </button>
        )}
      </div>

      {/* Branch / commit selector — varies by mode */}
      {mode === "existing" && (
        <SidebarSearchableField
          value={branch}
          placeholder="Select branch…"
          options={branchOptions}
          loading={refsLoading}
          open={showBranchPicker}
          onToggle={() => setShowBranchPicker(!showBranchPicker)}
          onSelect={(v) => { setBranch(v); setShowBranchPicker(false); }}
          onClose={() => setShowBranchPicker(false)}
          searchPlaceholder="Search branches…"
          emptyMessage="No branches found"
        />
      )}

      {mode === "new-branch" && (
        <>
          <input
            className="mb-1.5 w-full rounded border border-gx-border bg-gx-bg px-2 py-1 text-[11px] focus-visible:outline-none focus:border-gx-accent"
            placeholder="New branch name"
            value={branch}
            onChange={(e) => setBranch(e.target.value)}
          />
          <SidebarSearchableField
            value={startPoint}
            placeholder="Start point (HEAD)…"
            options={commitTagOptions}
            loading={commitsLoading || refsLoading}
            open={showStartPointPicker}
            onToggle={() => setShowStartPointPicker(!showStartPointPicker)}
            onSelect={(v) => { setStartPoint(v); setShowStartPointPicker(false); }}
            onClose={() => setShowStartPointPicker(false)}
            searchPlaceholder="Search commits, tags…"
            emptyMessage="No commits found"
          />
        </>
      )}

      {mode === "detached" && (
        <SidebarSearchableField
          value={branch}
          placeholder="Select commit / tag…"
          options={commitTagOptions}
          loading={commitsLoading || refsLoading}
          open={showDetachedPicker}
          onToggle={() => setShowDetachedPicker(!showDetachedPicker)}
          onSelect={(v) => { setBranch(v); setShowDetachedPicker(false); }}
          onClose={() => setShowDetachedPicker(false)}
          searchPlaceholder="Search commits, tags…"
          emptyMessage="No commits or tags found"
        />
      )}

      {error && (
        <div className="mb-1.5 rounded bg-gx-error/10 px-2 py-1 text-[10px] text-gx-error">{error}</div>
      )}
      <div className="flex gap-2">
        <button type="submit" className="btn btn-primary flex-1 justify-center text-[11px]" disabled={busy || !path.trim()}>
          {busy ? <Loader2 size={10} className="animate-spin" /> : <Plus size={10} />} Create
        </button>
        <button type="button" className="btn flex-1 justify-center text-[11px]" onClick={onCancel}>Cancel</button>
      </div>
    </form>
  );
}

/**
 * Compact searchable select field for the sidebar worktree form.
 * Shows a trigger button that opens a dropdown with search + scrollable list.
 */
function SidebarSearchableField({
  value,
  placeholder,
  options,
  loading,
  open,
  onToggle,
  onSelect,
  onClose,
  searchPlaceholder,
  emptyMessage,
}: {
  value: string;
  placeholder: string;
  options: { value: string; label: string; detail?: string; disabled?: boolean }[];
  loading: boolean;
  open: boolean;
  onToggle: () => void;
  onSelect: (value: string) => void;
  onClose: () => void;
  searchPlaceholder: string;
  emptyMessage: string;
}) {
  const [search, setSearch] = useState("");
  const panelRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    const handleClick = (e: MouseEvent) => {
      if (panelRef.current && !panelRef.current.contains(e.target as Node)) {
        onClose();
      }
    };
    window.addEventListener("keydown", handleKey);
    window.addEventListener("mousedown", handleClick);
    return () => {
      window.removeEventListener("keydown", handleKey);
      window.removeEventListener("mousedown", handleClick);
    };
  }, [open, onClose]);

  const filtered = useMemo(() => {
    if (!search.trim()) return options;
    const q = search.toLowerCase();
    return options.filter(
      (o) =>
        o.label.toLowerCase().includes(q) ||
        o.value.toLowerCase().includes(q) ||
        (o.detail && o.detail.toLowerCase().includes(q)),
    );
  }, [options, search]);

  return (
    <div className="relative mb-1.5">
      {/* Trigger button */}
      <button
        type="button"
        className="flex w-full items-center justify-between rounded border border-gx-border bg-gx-bg px-2 py-1 text-left text-[11px] transition hover:border-gx-muted/60 focus-visible:outline-none focus:border-gx-accent"
        onClick={onToggle}
      >
        <span className={value ? "truncate text-gx-text" : "text-gx-muted/60"}>
          {value || placeholder}
        </span>
        <ChevronDown size={10} className="shrink-0 text-gx-muted" />
      </button>

      {/* Dropdown */}
      {open && (
        <div
          ref={panelRef}
          className="absolute left-0 right-0 top-full z-50 mt-1 rounded border border-gx-border bg-gx-surface shadow-xl"
        >
          <div className="flex items-center gap-1 border-b border-gx-border px-2 py-1">
            <Search size={10} className="shrink-0 text-gx-muted" />
            <input
              ref={inputRef}
              className="flex-1 bg-transparent text-[11px] text-gx-text placeholder:text-gx-muted/60 focus-visible:outline-none"
              placeholder={searchPlaceholder}
              value={search}
              onChange={(e) => setSearch(e.target.value)}
            />
          </div>
          <div className="max-h-[180px] overflow-auto py-0.5">
            {loading && (
              <div className="flex items-center gap-1.5 px-2 py-2 text-[10px] text-gx-muted">
                <Loader2 size={10} className="animate-spin" /> Loading…
              </div>
            )}
            {!loading && filtered.length === 0 && (
              <div className="px-2 py-2 text-[10px] text-gx-muted">{emptyMessage}</div>
            )}
            {!loading &&
              filtered.map((opt) => (
                <button
                  key={opt.value}
                  type="button"
                  className={clsx(
                    "flex w-full items-center gap-1.5 px-2 py-1 text-left transition",
                    opt.disabled
                      ? "cursor-not-allowed opacity-50"
                      : "hover:bg-gx-accent/10",
                    opt.value === value && !opt.disabled && "bg-gx-accent/5",
                  )}
                  onClick={() => { if (!opt.disabled) onSelect(opt.value); }}
                  disabled={opt.disabled}
                >
                  <div className="min-w-0 flex-1">
                    <div className={clsx("truncate text-[11px]", opt.disabled ? "text-gx-muted" : "text-gx-text")}>{opt.label}</div>
                    {opt.detail && (
                      <div className={clsx("truncate font-mono text-[9px]", opt.disabled ? "text-gx-muted/60" : "text-gx-muted")}>{opt.detail}</div>
                    )}
                  </div>
                  {opt.value === value && !opt.disabled && (
                    <span className="shrink-0 text-[9px] text-gx-accent">✓</span>
                  )}
                </button>
              ))}
          </div>
        </div>
      )}
    </div>
  );
}
