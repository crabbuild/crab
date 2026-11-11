import { useEffect, useMemo, useState } from "react";
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
  MoreHorizontal,
  Pin,
  PinOff,
  RefreshCw,
  Settings,
  X,
} from "lucide-react";
import type { Workspace, WorkspaceRepoEntry } from "../../state";
import {
  groupWorkspaceRepositories,
  readWorkspaceSidebarState,
  workspaceIdentity,
  workspaceRepoIdentity,
} from "../../lib/workspace-sidebar";
import { ContextMenu, type MenuItemDef } from "../ContextMenu";

export interface WorkspaceSidebarProps {
  workspace: Workspace;
  activeRelativePath: string | null;
  query: string;
  pinnedPaths: Set<string>;
  onTogglePin: (path: string) => void;
  onSelect: (relativePath: string) => void;
  onRemoveRepo?: (relativePath: string) => void;
  onRefresh?: () => void;
  onClose?: () => void;
  onSettings?: () => void;
  onShowDashboard?: () => void;
  onCloneRepository?: () => void;
}

export function WorkspaceSidebar({
  workspace,
  activeRelativePath,
  query,
  pinnedPaths,
  onTogglePin,
  onSelect,
  onRemoveRepo,
  onRefresh,
  onClose,
  onSettings,
  onShowDashboard,
  onCloneRepository,
}: WorkspaceSidebarProps) {
  const storageKey = `crab.workspaceSidebar.${workspaceIdentity(workspace)}`;
  const [saved, setSaved] = useState(() =>
    readWorkspaceSidebarState(storageKey),
  );
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const collapsed = saved.collapsed && !query.trim();
  // Discovery can replace status entries. Keep bounded access history separately
  // so refreshing or restarting cannot move every recent repository back to All.
  useEffect(() => {
    setSaved((previous) => {
      const recent = { ...previous.recent };
      let changed = false;
      for (const entry of workspace.repos) {
        if (entry.lastAccessedAt > (recent[entry.relativePath] ?? 0)) {
          recent[entry.relativePath] = entry.lastAccessedAt;
          changed = true;
        }
      }
      if (!changed) return previous;
      return {
        ...previous,
        recent: Object.fromEntries(
          Object.entries(recent)
            .sort((a, b) => b[1] - a[1])
            .slice(0, 20),
        ),
      };
    });
  }, [workspace.repos]);
  useEffect(() => {
    localStorage.setItem(storageKey, JSON.stringify(saved));
  }, [storageKey, saved]);
  const { groups, children } = useMemo(
    () =>
      groupWorkspaceRepositories(workspace, pinnedPaths, saved.recent, query),
    [workspace, pinnedPaths, saved.recent, query],
  );
  const actions: MenuItemDef[] = [
    {
      id: "overview",
      label: "Show Workspace Dashboard",
      icon: LayoutDashboard,
    },
    ...(onCloneRepository
      ? [
          {
            id: "clone",
            label: "Clone Repository to Workspace",
            icon: Download,
          },
        ]
      : []),
    ...(onRefresh
      ? [{ id: "refresh", label: "Refresh Workspace", icon: RefreshCw }]
      : []),
    ...(onSettings
      ? [{ id: "settings", label: "Workspace Settings", icon: Settings }]
      : []),
    ...(onClose
      ? [
          {
            id: "close",
            label: "Close Workspace",
            icon: X,
            separator: true,
            danger: true,
          },
        ]
      : []),
  ];
  const handlers: Record<string, (() => void) | undefined> = {
    overview: onShowDashboard,
    clone: onCloneRepository,
    refresh: onRefresh,
    settings: onSettings,
    close: onClose,
  };
  return (
    <section aria-label={`${workspace.name} workspace`}>
      <div
        className="flex items-center gap-1 rounded px-2 py-1.5 hover:bg-accent/50"
        onContextMenu={(event) => {
          event.preventDefault();
          setMenu({ x: event.clientX, y: event.clientY });
        }}
      >
        <button
          type="button"
          className="shrink-0 rounded p-1 text-muted-foreground hover:text-foreground"
          aria-label={collapsed ? "Expand workspace" : "Collapse workspace"}
          aria-expanded={!collapsed}
          onClick={() =>
            setSaved((previous) => ({
              ...previous,
              collapsed: !previous.collapsed,
            }))
          }
        >
          {collapsed ? <ChevronRight size={12} /> : <ChevronDown size={12} />}
        </button>
        <button
          type="button"
          className="min-w-0 flex-1 text-left"
          onClick={onShowDashboard}
          title={
            workspace.kind === "remote"
              ? `${workspace.profileLabel ?? workspace.profileId}:${workspace.rootPath}`
              : workspace.rootPath
          }
        >
          <span className="flex items-center gap-1 text-xs font-semibold text-foreground">
            <span className="truncate">{workspace.name}</span>
            {workspace.kind === "remote" && (
              <Cloud size={11} className="shrink-0 text-primary" />
            )}
          </span>
          <span className="block truncate text-[10px] text-muted-foreground">
            {workspace.kind === "remote"
              ? `${workspace.profileLabel ?? workspace.profileId}:`
              : ""}
            {workspace.rootPath}
          </span>
        </button>
        <button
          type="button"
          aria-label={`${workspace.name} workspace actions`}
          aria-haspopup="menu"
          aria-expanded={menu != null}
          title="Workspace actions"
          className="shrink-0 rounded p-1.5 text-muted-foreground hover:bg-accent hover:text-foreground"
          onClick={(event) => {
            const rect = event.currentTarget.getBoundingClientRect();
            setMenu(menu ? null : { x: rect.left, y: rect.bottom });
          }}
        >
          <MoreHorizontal size={14} />
        </button>
      </div>
      {!collapsed &&
        (groups.length ? (
          groups.map((group) => (
            <div key={group.label}>
              <div className="px-4 pb-1 pt-2 text-[10px] font-medium text-muted-foreground">
                {group.label}
              </div>
              <ul
                aria-label={`${workspace.name} ${group.label}`}
                className="px-2 pl-3"
              >
                {group.entries.map((entry) => (
                  <li key={entry.relativePath}>
                    <WorkspaceRepoRow
                      entry={entry}
                      isActive={entry.relativePath === activeRelativePath}
                      onSelect={onSelect}
                      onRemove={onRemoveRepo}
                      pinned={pinnedPaths.has(
                        workspaceRepoIdentity(workspace, entry),
                      )}
                      onTogglePin={() =>
                        onTogglePin(workspaceRepoIdentity(workspace, entry))
                      }
                    />
                    {(children.get(entry.relativePath)?.length ?? 0) > 0 && (
                      <ul className="ml-3 border-l border-border/40 pl-1.5">
                        {children.get(entry.relativePath)!.map((child) => (
                          <li key={child.relativePath}>
                            <WorkspaceRepoRow
                              entry={child}
                              isActive={
                                child.relativePath === activeRelativePath
                              }
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
          ))
        ) : (
          <div className="px-4 py-5 text-xs text-muted-foreground">
            {query.trim()
              ? "No matching repositories"
              : "No repositories found"}
            {!query.trim() && onCloneRepository && (
              <button
                type="button"
                className="mt-3 flex items-center gap-1 rounded border border-border px-2 py-1.5 hover:text-foreground"
                onClick={onCloneRepository}
              >
                <Download size={12} />
                Clone Repository
              </button>
            )}
          </div>
        ))}
      {menu && (
        <ContextMenu
          items={actions}
          position={menu}
          onSelect={(id) => {
            setMenu(null);
            handlers[id]?.();
          }}
          onClose={() => setMenu(null)}
        />
      )}
    </section>
  );
}

interface RowProps {
  entry: WorkspaceRepoEntry;
  isActive: boolean;
  onSelect: (relativePath: string) => void;
  onRemove?: (relativePath: string) => void;
  isWorktree?: boolean;
  pinned?: boolean;
  onTogglePin?: () => void;
}

function WorkspaceRepoRow({
  entry,
  isActive,
  onSelect,
  onRemove,
  isWorktree,
  pinned,
  onTogglePin,
}: RowProps) {
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const actions: MenuItemDef[] = [
    ...(onTogglePin
      ? [
          {
            id: "pin",
            label: pinned ? "Unpin repository" : "Pin repository",
            icon: pinned ? PinOff : Pin,
          },
        ]
      : []),
    ...(onRemove
      ? [
          {
            id: "remove",
            label: "Remove from Workspace",
            icon: X,
            danger: true,
          },
        ]
      : []),
  ];
  const changed = `${entry.dirtyCount} changed ${entry.dirtyCount === 1 ? "file" : "files"}`;
  return (
    <>
      <div
        className={clsx(
          "group mb-0.5 flex items-center rounded",
          isActive ? "bg-primary/15" : "hover:bg-accent",
        )}
        onContextMenu={(event) => {
          if (!actions.length) return;
          event.preventDefault();
          setMenu({ x: event.clientX, y: event.clientY });
        }}
      >
        <button
          type="button"
          aria-current={isActive ? "true" : undefined}
          className="flex min-w-0 flex-1 items-center gap-1.5 rounded px-2 py-1.5 text-left text-xs focus-visible:outline focus-visible:outline-2 focus-visible:outline-primary"
          title={
            entry.error
              ? `${entry.absolutePath}\nError: ${entry.error}`
              : `${entry.absolutePath}${entry.branch ? `\n${entry.branch}` : ""}`
          }
          onClick={() => onSelect(entry.relativePath)}
        >
          {entry.error ? (
            <AlertCircle
              size={12}
              className="shrink-0 text-destructive"
              aria-label={entry.error}
            />
          ) : isWorktree ? (
            <FolderTree size={11} className="shrink-0 text-muted-foreground" />
          ) : null}
          <span className="min-w-0 flex-1">
            <span className="block truncate font-medium text-foreground">
              {entry.name}
            </span>
            {isActive && entry.branch && (
              <span className="mt-0.5 flex items-center gap-1 text-[10px] text-muted-foreground">
                <GitBranch size={10} />
                <span className="truncate">{entry.branch}</span>
              </span>
            )}
          </span>
          {entry.level === 2 && !isActive && (
            <span
              className="h-1 w-1 shrink-0 rounded-full bg-primary"
              title="Open"
            />
          )}
          {(entry.dirtyCount ?? 0) > 0 && (
            <span
              aria-label={changed}
              title={changed}
              className="shrink-0 rounded bg-accent px-1.5 py-0.5 text-[10px] tabular-nums text-muted-foreground"
            >
              {entry.dirtyCount}
            </span>
          )}
        </button>
        {onTogglePin && (
          <button
            type="button"
            aria-label={`${pinned ? "Unpin" : "Pin"} ${entry.name}`}
            title={pinned ? "Unpin repository" : "Pin repository"}
            onClick={onTogglePin}
            className={clsx(
              "mr-1 shrink-0 rounded p-1 text-muted-foreground hover:text-foreground focus-visible:opacity-100",
              !pinned &&
                "opacity-0 group-hover:opacity-100 group-focus-within:opacity-100",
            )}
          >
            <Pin size={11} className={pinned ? "fill-current" : undefined} />
          </button>
        )}
      </div>
      {menu && (
        <ContextMenu
          items={actions}
          position={menu}
          onSelect={(id) => {
            setMenu(null);
            if (id === "pin") onTogglePin?.();
            else if (id === "remove") onRemove?.(entry.relativePath);
          }}
          onClose={() => setMenu(null)}
        />
      )}
    </>
  );
}
