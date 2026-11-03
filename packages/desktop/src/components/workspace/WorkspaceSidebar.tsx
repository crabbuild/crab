import { useMemo, useState } from "react";
import clsx from "clsx";
import {
  AlertCircle,
  ChevronDown,
  ChevronRight,
  Cloud,
  Download,
  FolderTree,
  GitBranch,
  LayoutDashboard,
  RefreshCw,
  Settings,
  X,
} from "lucide-react";
import type { Workspace, WorkspaceRepoEntry } from "../../state";
import { ContextMenu, type MenuItemDef } from "../ContextMenu";

export interface WorkspaceSidebarProps {
  workspace: Workspace;
  activeRelativePath: string | null;
  onSelect: (relativePath: string) => void;
  onRemoveRepo?: (relativePath: string) => void;
  onRefresh?: () => void;
  onClose?: () => void;
  onSettings?: () => void;
  onShowDashboard?: () => void;
  onCloneRepository?: () => void;
}

/**
 * Sidebar workspace view: flat list of all workspace repos with
 * inline status (branch, dirty count) and visual level indicators.
 *
 * Simplified model: a workspace is either entirely local or entirely
 * remote. All repos are discovered under the workspace root.
 */
export function WorkspaceSidebar({
  workspace,
  activeRelativePath,
  onSelect,
  onRemoveRepo,
  onRefresh,
  onClose,
  onSettings,
  onShowDashboard,
  onCloneRepository,
}: WorkspaceSidebarProps) {
  // Group repos: worktrees nested under parent repos.
  // A worktree is identified by having a relativePath that starts with
  // another repo's relativePath + "/". We use a simple heuristic:
  // repos whose path is a sub-path of another repo are treated as worktrees.
  const { topLevel, worktreeMap } = useMemo(() => {
    const sorted = [...workspace.repos].sort(
      (a, b) => a.relativePath.localeCompare(b.relativePath),
    );
    const topLevel: WorkspaceRepoEntry[] = [];
    const worktreeMap = new Map<string, WorkspaceRepoEntry[]>();

    for (const entry of sorted) {
      // Check if this entry is a worktree nested under a parent repo
      const parent = topLevel.find(
        (p) =>
          entry.relativePath.startsWith(p.relativePath + "/") &&
          entry.relativePath !== p.relativePath,
      );
      if (parent) {
        const existing = worktreeMap.get(parent.relativePath) ?? [];
        existing.push(entry);
        worktreeMap.set(parent.relativePath, existing);
      } else {
        topLevel.push(entry);
      }
    }
    return { topLevel, worktreeMap };
  }, [workspace.repos]);

  // Context menu state for workspace header right-click
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null);
  // Collapsible state for the workspace's repo list
  const [reposCollapsed, setReposCollapsed] = useState(false);

  const contextMenuItems: MenuItemDef[] = [
    { id: "show-dashboard", label: "Show Workspace Dashboard", icon: LayoutDashboard },
    ...(onCloneRepository ? [{ id: "clone-repo", label: "Clone Repository to Workspace", icon: Download }] : []),
    ...(onRefresh ? [{ id: "refresh", label: "Refresh Workspace", icon: RefreshCw }] : []),
    ...(onSettings ? [{ id: "settings", label: "Workspace Settings", icon: Settings }] : []),
    ...(onClose ? [{ id: "close", label: "Close Workspace", icon: X, separator: true, danger: true }] : []),
  ];

  function handleContextMenuSelect(itemId: string) {
    setContextMenu(null);
    switch (itemId) {
      case "show-dashboard":
        onShowDashboard?.();
        break;
      case "clone-repo":
        onCloneRepository?.();
        break;
      case "refresh":
        onRefresh?.();
        break;
      case "settings":
        onSettings?.();
        break;
      case "close":
        onClose?.();
        break;
    }
  }

  return (
    <div className="flex flex-col">
      {/* Workspace header */}
      <div
        className="flex items-center gap-1.5 px-2 py-1.5 rounded hover:bg-gx-surface-hover/50 transition"
        onContextMenu={(e) => {
          e.preventDefault();
          setContextMenu({ x: e.clientX, y: e.clientY });
        }}
        title="Click to expand/collapse workspace repos"
      >
        <button
          type="button"
          className="shrink-0 p-0 text-gx-muted hover:text-gx-text"
          onClick={(e) => { e.stopPropagation(); setReposCollapsed((v) => !v); }}
          aria-label={reposCollapsed ? "Expand workspace" : "Collapse workspace"}
        >
          {reposCollapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
        </button>
        <div className="min-w-0 flex-1 cursor-pointer" onClick={onShowDashboard}>
          <div className="truncate text-xs font-semibold text-gx-text flex items-center gap-1">
            {workspace.name}
            {workspace.kind === "remote" && (
              <span title={`Remote: ${workspace.profileLabel ?? workspace.profileId}`}>
                <Cloud size={10} className="text-gx-accent" />
              </span>
            )}
          </div>
          <div className="truncate text-[10px] text-gx-muted">
            {workspace.kind === "remote" && workspace.profileLabel
              ? `${workspace.profileLabel}:${workspace.rootPath}`
              : workspace.rootPath}
          </div>
        </div>
        {onCloneRepository && (
          <button
            type="button"
            className="shrink-0 rounded p-1 text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover transition"
            onClick={(e) => { e.stopPropagation(); onCloneRepository(); }}
            title="Clone Repository to Workspace"
            aria-label="Clone Repository to Workspace"
          >
            <Download size={12} />
          </button>
        )}
        {onSettings && (
          <button
            type="button"
            className="shrink-0 rounded p-1 text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover transition"
            onClick={(e) => { e.stopPropagation(); onSettings(); }}
            title="Workspace Settings"
            aria-label="Workspace Settings"
          >
            <Settings size={12} />
          </button>
        )}
        {onRefresh && (
          <button
            type="button"
            className="shrink-0 rounded p-1 text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover transition"
            onClick={(e) => { e.stopPropagation(); onRefresh(); }}
            title="Refresh workspace"
            aria-label="Refresh workspace"
          >
            <RefreshCw size={12} />
          </button>
        )}
        {onClose && (
          <button
            type="button"
            className="shrink-0 rounded p-1 text-gx-muted hover:text-gx-error hover:bg-gx-surface-hover transition"
            onClick={(e) => { e.stopPropagation(); onClose(); }}
            title="Close workspace"
            aria-label="Close workspace"
          >
            <X size={12} />
          </button>
        )}
      </div>

      {/* Repo list (collapsible) */}
      {!reposCollapsed && (
        topLevel.length === 0 ? (
          <div className="px-3 py-6 text-center">
            <p className="text-xs text-gx-muted">No repositories found</p>
            <p className="mt-1 text-[10px] text-gx-muted/70">
              Try adjusting the scan depth or exclude patterns in workspace settings.
            </p>
            {/* Action buttons for empty state */}
            <div className="mt-3 flex flex-col gap-1.5">
              {onCloneRepository && (
                <button
                  type="button"
                  className="flex w-full items-center justify-center gap-1.5 rounded border border-gx-border px-2 py-1.5 text-[11px] text-gx-muted hover:text-gx-text hover:border-gx-accent/50 hover:bg-gx-accent/5 transition"
                  onClick={onCloneRepository}
                >
                  <Download size={11} />
                  Clone Repository
                </button>
              )}
            </div>
          </div>
        ) : (
          <div>
            <ul className="flex-1 overflow-auto pl-3 pr-2 py-1">
              {topLevel.map((entry) => (
                <li key={entry.relativePath}>
                  <WorkspaceRepoRow
                    entry={entry}
                    isActive={entry.relativePath === activeRelativePath}
                    onSelect={onSelect}
                    onRemove={onRemoveRepo}
                  />
                  {/* Nested worktrees */}
                  {worktreeMap.has(entry.relativePath) && (
                    <ul className="ml-3 border-l border-gx-border/40 pl-1.5">
                      {worktreeMap.get(entry.relativePath)!.map((wt) => (
                        <li key={wt.relativePath}>
                          <WorkspaceRepoRow
                            entry={wt}
                            isActive={wt.relativePath === activeRelativePath}
                            onSelect={onSelect}
                            isWorktree
                          />
                        </li>
                      ))}
                    </ul>
                  )}
                </li>
              ))}
            </ul>
          </div>
        )
      )}

      {/* Context menu */}
      {contextMenu && (
        <ContextMenu
          items={contextMenuItems}
          position={contextMenu}
          onSelect={handleContextMenuSelect}
          onClose={() => setContextMenu(null)}
        />
      )}
    </div>
  );
}

// ── Individual repo row ─────────────────────────────────────────────

interface WorkspaceRepoRowProps {
  entry: WorkspaceRepoEntry;
  isActive: boolean;
  onSelect: (relativePath: string) => void;
  onRemove?: (relativePath: string) => void;
  isWorktree?: boolean;
}

function WorkspaceRepoRow({ entry, isActive, onSelect, onRemove, isWorktree }: WorkspaceRepoRowProps) {
  const isLevel0 = entry.level === 0;
  const isLevel2 = entry.level === 2;
  const hasError = entry.error !== null;
  const [ctxMenu, setCtxMenu] = useState<{ x: number; y: number } | null>(null);

  const ctxMenuItems: MenuItemDef[] = [
    ...(onRemove && !isWorktree ? [{ id: "remove", label: "Remove from Workspace", icon: X, danger: true }] : []),
  ];

  return (
    <>
    <button
      type="button"
      className={clsx(
        "group flex w-full items-center gap-1.5 rounded px-2 py-1.5 text-left text-xs transition mb-0.5",
        isActive && "bg-gx-accent/15 ring-1 ring-gx-accent/30",
        !isActive && !isLevel0 && "hover:bg-gx-surface-hover",
        isLevel0 && !isActive && "opacity-50 hover:opacity-70 hover:bg-gx-surface-hover",
      )}
      onClick={() => onSelect(entry.relativePath)}
      onContextMenu={(e) => {
        if (ctxMenuItems.length > 0) {
          e.preventDefault();
          setCtxMenu({ x: e.clientX, y: e.clientY });
        }
      }}
      title={hasError ? `Error: ${entry.error}` : entry.absolutePath}
    >
      {/* Error indicator */}
      {hasError && (
        <span className="shrink-0" title={entry.error ?? undefined}>
          <AlertCircle size={12} className="text-gx-error" />
        </span>
      )}

      {/* Worktree icon */}
      {isWorktree && !hasError && (
        <FolderTree size={11} className="shrink-0 text-gx-muted" />
      )}

      {/* Repo name */}
      <span
        className={clsx(
          "min-w-0 flex-1 truncate font-medium",
          isActive && "text-gx-text",
          !isActive && isLevel0 && "text-gx-muted",
          !isActive && !isLevel0 && "text-gx-text",
        )}
      >
        {entry.name}
      </span>

      {/* Level 2 open indicator */}
      {isLevel2 && !isActive && (
        <span className="shrink-0 w-1.5 h-1.5 rounded-full bg-gx-accent/50" title="Open" />
      )}

      {/* Branch + dirty count (Level 1+) */}
      {!isLevel0 && entry.branch && (
        <span className="shrink-0 flex items-center gap-0.5 text-[10px] text-gx-muted">
          <GitBranch size={9} />
          <span className="max-w-[80px] truncate">{entry.branch}</span>
        </span>
      )}

      {!isLevel0 && entry.dirtyCount !== null && entry.dirtyCount > 0 && (
        <span className="shrink-0 rounded bg-gx-warn/20 px-1 py-0.5 text-[9px] font-medium text-gx-warn">
          {entry.dirtyCount}
        </span>
      )}
    </button>
    {ctxMenu && (
      <ContextMenu
        items={ctxMenuItems}
        position={ctxMenu}
        onSelect={(id) => {
          setCtxMenu(null);
          if (id === "remove") onRemove?.(entry.relativePath);
        }}
        onClose={() => setCtxMenu(null)}
      />
    )}
    </>
  );
}
