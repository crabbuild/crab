/**
 * Remote file browser component — Finder-like experience.
 *
 * Features: breadcrumb nav, list/grid views, tree sidebar,
 * back/forward history, context menu actions, loading/error/empty states.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowLeft,
  ArrowRight,
  ChevronRight,
  Download,
  File,
  Folder,
  FolderGit2,
  FolderOpen,
  Grid,
  Home,
  List,
  Loader2,
  Lock,
  MoreVertical,
  RefreshCw,
  Terminal,
  Trash2,
  PanelLeft,
} from "lucide-react";
import clsx from "clsx";
import { FileSyncStatus, SyncStatusIcon } from "./SyncStatusIcon";
import { SyncPairStatusBadge } from "./SyncPairStatusBadge";

// ── Types ────────────────────────────────────────────────────────

interface RemoteDirEntry {
  name: string;
  path: string;
  kind: "file" | "dir";
  size: number | null;
}

interface BrowseResult {
  entries: RemoteDirEntry[];
  path: string;
  hasGitDir: boolean;
}

interface RemoteFileBrowserProps {
  profileId: string;
  profileLabel: string;
  profileColour?: string;
  initialPath?: string;
  onOpenFile?: (profileId: string, path: string) => void;
  onOpenRepo?: (profileId: string, path: string) => void;
  onOpenTerminal?: (profileId: string, path: string) => void;
  onSetupSync?: (profileId: string, remotePath: string) => void;
}

type ViewMode = "list" | "grid";

interface TreeNode {
  name: string;
  path: string;
  children: TreeNode[] | null; // null = not loaded
  loading: boolean;
  expanded: boolean;
}

// ── Helpers ──────────────────────────────────────────────────────

function basename(path: string): string {
  const parts = path.split("/").filter(Boolean);
  return parts[parts.length - 1] ?? "/";
}

function parentPath(path: string): string {
  const parts = path.split("/").filter(Boolean);
  if (parts.length <= 1) return "/";
  return "/" + parts.slice(0, -1).join("/");
}

function pathSegments(path: string): { label: string; path: string }[] {
  const parts = path.split("/").filter(Boolean);
  const segments: { label: string; path: string }[] = [{ label: "/", path: "/" }];
  let current = "";
  for (const part of parts) {
    current += "/" + part;
    segments.push({ label: part, path: current });
  }
  return segments;
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

// ── Component ────────────────────────────────────────────────────

export function RemoteFileBrowser({
  profileId,
  profileLabel,
  profileColour,
  initialPath,
  onOpenFile,
  onOpenRepo,
  onOpenTerminal,
  onSetupSync,
}: RemoteFileBrowserProps) {
  const [currentPath, setCurrentPath] = useState(initialPath ?? "/");
  const [entries, setEntries] = useState<RemoteDirEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [hasGitDir, setHasGitDir] = useState(false);
  const [viewMode, setViewMode] = useState<ViewMode>("list");
  const [showTree, setShowTree] = useState(false);
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number; entry: RemoteDirEntry } | null>(null);

  // Navigation history
  const [history, setHistory] = useState<string[]>([]);
  const [historyIndex, setHistoryIndex] = useState(-1);
  const isNavigatingRef = useRef(false);

  // Tree state
  const [treeNodes, setTreeNodes] = useState<TreeNode[]>([]);

  // Sync status map: relative file path → FileSyncStatus
  const [fileSyncStatuses, setFileSyncStatuses] = useState<Map<string, FileSyncStatus>>(new Map());

  // Listen for sync file status updates from main process
  useEffect(() => {
    const cleanup = (window as any).crab.ssh.onSyncFileStatus(
      (payload: { syncPairId: string; statuses: Record<string, string> }) => {
        setFileSyncStatuses(new Map(Object.entries(payload.statuses)) as Map<string, FileSyncStatus>);
      },
    );
    return () => { cleanup(); };
  }, []);

  // Dir cache
  const dirCacheRef = useRef<Map<string, { entries: RemoteDirEntry[]; hasGitDir: boolean; fetchedAt: number }>>(new Map());
  const CACHE_TTL = 5_000;

  const browsePath = useCallback(async (path: string, skipHistory = false) => {
    const cached = dirCacheRef.current.get(path);
    if (cached && Date.now() - cached.fetchedAt < CACHE_TTL) {
      setEntries(cached.entries);
      setHasGitDir(cached.hasGitDir);
      setCurrentPath(path);
      setLoading(false);
      if (!skipHistory && !isNavigatingRef.current) {
        setHistory((prev) => [...prev.slice(0, historyIndex + 1), path]);
        setHistoryIndex((prev) => prev + 1);
      }
      isNavigatingRef.current = false;
      return;
    }

    setLoading(true);
    setError(null);
    try {
      const result: BrowseResult = await (window as any).crab.ssh.browsePath(profileId, path);
      setEntries(result.entries);
      setHasGitDir(result.hasGitDir);
      setCurrentPath(result.path);
      dirCacheRef.current.set(path, {
        entries: result.entries,
        hasGitDir: result.hasGitDir,
        fetchedAt: Date.now(),
      });
      if (!skipHistory && !isNavigatingRef.current) {
        setHistory((prev) => [...prev.slice(0, historyIndex + 1), result.path]);
        setHistoryIndex((prev) => prev + 1);
      }
      isNavigatingRef.current = false;
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [profileId, historyIndex]);

  // Load initial path or home directory
  useEffect(() => {
    if (initialPath) {
      browsePath(initialPath);
    } else {
      (async () => {
        try {
          const result = await (window as any).crab.ssh.homeDir(profileId);
          const homePath = (result as { path: string }).path ?? "/home";
          browsePath(homePath);
        } catch {
          browsePath("/");
        }
      })();
    }
  }, [profileId, initialPath]);

  // Initialize tree with root
  useEffect(() => {
    if (showTree && treeNodes.length === 0) {
      setTreeNodes([{ name: "/", path: "/", children: null, loading: false, expanded: false }]);
    }
  }, [showTree, treeNodes.length]);

  // Navigation handlers
  const handleNavigate = useCallback((path: string) => {
    setContextMenu(null);
    browsePath(path);
  }, [browsePath]);

  const handleBack = useCallback(() => {
    if (historyIndex > 0) {
      isNavigatingRef.current = true;
      const newIndex = historyIndex - 1;
      setHistoryIndex(newIndex);
      browsePath(history[newIndex], true);
    }
  }, [historyIndex, history, browsePath]);

  const handleForward = useCallback(() => {
    if (historyIndex < history.length - 1) {
      isNavigatingRef.current = true;
      const newIndex = historyIndex + 1;
      setHistoryIndex(newIndex);
      browsePath(history[newIndex], true);
    }
  }, [historyIndex, history, browsePath]);

  const handleRefresh = useCallback(() => {
    dirCacheRef.current.delete(currentPath);
    browsePath(currentPath, true);
  }, [currentPath, browsePath]);

  const handleGoHome = useCallback(async () => {
    try {
      const result = await (window as any).crab.ssh.homeDir(profileId);
      const homePath = (result as { path: string }).path ?? "/home";
      handleNavigate(homePath);
    } catch {
      handleNavigate("/");
    }
  }, [profileId, handleNavigate]);

  const handleEntryClick = useCallback((entry: RemoteDirEntry) => {
    if (entry.kind === "dir") {
      handleNavigate(entry.path);
    } else if (onOpenFile) {
      onOpenFile(profileId, entry.path);
    }
  }, [profileId, handleNavigate, onOpenFile]);

  const handleOpenRepo = useCallback(() => {
    if (onOpenRepo) onOpenRepo(profileId, currentPath);
  }, [profileId, currentPath, onOpenRepo]);

  const handleOpenTerminal = useCallback((path: string) => {
    if (onOpenTerminal) onOpenTerminal(profileId, path);
  }, [profileId, onOpenTerminal]);

  const handleContextMenu = useCallback((e: React.MouseEvent, entry: RemoteDirEntry) => {
    e.preventDefault();
    e.stopPropagation();
    setContextMenu({ x: e.clientX, y: e.clientY, entry });
  }, []);

  // Close context menu on click outside
  useEffect(() => {
    if (!contextMenu) return;
    const handler = () => setContextMenu(null);
    document.addEventListener("click", handler);
    return () => document.removeEventListener("click", handler);
  }, [contextMenu]);

  // Tree expand/collapse
  const handleTreeToggle = useCallback(async (nodePath: string) => {
    const updateNode = (nodes: TreeNode[], targetPath: string, updater: (n: TreeNode) => TreeNode): TreeNode[] => {
      return nodes.map((node) => {
        if (node.path === targetPath) return updater(node);
        if (node.children) return { ...node, children: updateNode(node.children, targetPath, updater) };
        return node;
      });
    };

    // Find the node
    const findNode = (nodes: TreeNode[], targetPath: string): TreeNode | null => {
      for (const node of nodes) {
        if (node.path === targetPath) return node;
        if (node.children) {
          const found = findNode(node.children, targetPath);
          if (found) return found;
        }
      }
      return null;
    };

    const node = findNode(treeNodes, nodePath);
    if (!node) return;

    if (node.expanded) {
      setTreeNodes((prev) => updateNode(prev, nodePath, (n) => ({ ...n, expanded: false })));
      return;
    }

    if (node.children !== null) {
      setTreeNodes((prev) => updateNode(prev, nodePath, (n) => ({ ...n, expanded: true })));
      return;
    }

    // Load children
    setTreeNodes((prev) => updateNode(prev, nodePath, (n) => ({ ...n, loading: true })));
    try {
      const result: BrowseResult = await (window as any).crab.ssh.browsePath(profileId, nodePath);
      const children: TreeNode[] = result.entries
        .filter((e) => e.kind === "dir")
        .sort((a, b) => a.name.localeCompare(b.name))
        .map((e) => ({ name: e.name, path: e.path, children: null, loading: false, expanded: false }));
      setTreeNodes((prev) => updateNode(prev, nodePath, (n) => ({ ...n, children, expanded: true, loading: false })));
    } catch {
      setTreeNodes((prev) => updateNode(prev, nodePath, (n) => ({ ...n, loading: false })));
    }
  }, [profileId, treeNodes]);

  const segments = useMemo(() => pathSegments(currentPath), [currentPath]);

  const sortedEntries = useMemo(() => {
    return [...entries].sort((a, b) => {
      if (a.kind === "dir" && b.kind !== "dir") return -1;
      if (a.kind !== "dir" && b.kind === "dir") return 1;
      return (a.name ?? basename(a.path)).localeCompare(b.name ?? basename(b.path));
    });
  }, [entries]);

  const canGoBack = historyIndex > 0;
  const canGoForward = historyIndex < history.length - 1;

  return (
    <div className="flex flex-col h-full bg-gx-bg text-gx-text">
      {/* Toolbar */}
      <div className="flex items-center gap-1 px-2 py-1.5 border-b border-gx-border bg-gx-surface">
        {/* Back / Forward */}
        <button
          onClick={handleBack}
          disabled={!canGoBack}
          className="btn-icon"
          title="Back"
          aria-label="Navigate back"
        >
          <ArrowLeft className="w-4 h-4" />
        </button>
        <button
          onClick={handleForward}
          disabled={!canGoForward}
          className="btn-icon"
          title="Forward"
          aria-label="Navigate forward"
        >
          <ArrowRight className="w-4 h-4" />
        </button>

        {/* Up / Home */}
        <button
          onClick={() => handleNavigate(parentPath(currentPath))}
          disabled={currentPath === "/"}
          className="btn-icon"
          title="Go to parent directory"
          aria-label="Go to parent directory"
        >
          <ChevronRight className="w-4 h-4 rotate-[-90deg]" />
        </button>
        <button
          onClick={handleGoHome}
          className="btn-icon"
          title="Go to home directory"
          aria-label="Go to home directory"
        >
          <Home className="w-4 h-4" />
        </button>

        {/* Profile label */}
        <div className="flex items-center gap-1.5 ml-2 mr-auto">
          {profileColour && (
            <span className="w-2 h-2 rounded-full shrink-0" style={{ backgroundColor: profileColour }} />
          )}
          <span className="text-xs font-medium text-gx-muted truncate max-w-[120px]">
            {profileLabel}
          </span>
          {/* Sync pair status badge */}
          <SyncPairStatusBadge profileId={profileId} currentPath={currentPath} />
        </div>

        {/* View toggles */}
        <button
          onClick={() => setShowTree((v) => !v)}
          className={clsx("btn-icon", showTree && "bg-gx-surface-hover text-gx-text")}
          title="Toggle tree sidebar"
          aria-label="Toggle tree sidebar"
        >
          <PanelLeft className="w-4 h-4" />
        </button>
        <button
          onClick={() => setViewMode("list")}
          className={clsx("btn-icon", viewMode === "list" && "bg-gx-surface-hover text-gx-text")}
          title="List view"
          aria-label="Switch to list view"
        >
          <List className="w-4 h-4" />
        </button>
        <button
          onClick={() => setViewMode("grid")}
          className={clsx("btn-icon", viewMode === "grid" && "bg-gx-surface-hover text-gx-text")}
          title="Grid view"
          aria-label="Switch to grid view"
        >
          <Grid className="w-4 h-4" />
        </button>

        {/* Refresh */}
        <button
          onClick={handleRefresh}
          className="btn-icon"
          title="Refresh"
          aria-label="Refresh file list"
        >
          <RefreshCw className="w-3.5 h-3.5" />
        </button>
      </div>

      {/* Breadcrumb */}
      <div className="flex items-center gap-0.5 px-3 py-1.5 text-xs text-gx-muted overflow-x-auto border-b border-gx-border bg-gx-bg-sub scrollbar-none">
        {segments.map((seg, i) => (
          <span key={seg.path} className="flex items-center gap-0.5 shrink-0">
            {i > 0 && <ChevronRight className="w-3 h-3 text-gx-muted/50" />}
            <button
              onClick={() => handleNavigate(seg.path)}
              className={clsx(
                "px-1 py-0.5 rounded hover:bg-gx-surface-hover hover:text-gx-text transition-colors truncate max-w-[140px]",
                i === segments.length - 1 ? "font-medium text-gx-text" : "text-gx-muted",
              )}
            >
              {seg.label === "/" ? <Home className="w-3 h-3 inline" /> : seg.label}
            </button>
          </span>
        ))}
      </div>

      {/* Git repo banner */}
      {hasGitDir && onOpenRepo && (
        <div className="flex items-center gap-2 px-3 py-1.5 bg-gx-accent/10 border-b border-gx-accent/20">
          <FolderGit2 className="w-4 h-4 text-gx-accent" />
          <span className="text-xs text-gx-accent">Git repository detected</span>
          <button
            onClick={handleOpenRepo}
            className="ml-auto text-xs px-2 py-0.5 rounded bg-gx-accent text-white hover:bg-gx-accent-hover transition-colors"
          >
            Open Repo
          </button>
        </div>
      )}

      {/* Main content area */}
      <div className="flex flex-1 overflow-hidden">
        {/* Tree sidebar */}
        {showTree && (
          <div className="w-48 border-r border-gx-border bg-gx-bg-sub overflow-y-auto shrink-0">
            <div className="py-1">
              {treeNodes.map((node) => (
                <TreeNodeRow
                  key={node.path}
                  node={node}
                  depth={0}
                  currentPath={currentPath}
                  onToggle={handleTreeToggle}
                  onNavigate={handleNavigate}
                />
              ))}
            </div>
          </div>
        )}

        {/* File listing */}
        <div className="flex-1 overflow-y-auto" role="grid" aria-label="Remote file browser">
          {/* Loading state */}
          {loading && (
            <div className="flex flex-col items-center justify-center py-12 gap-2">
              <Loader2 className="w-6 h-6 animate-spin text-gx-accent" />
              <span className="text-sm text-gx-muted">Loading directory...</span>
            </div>
          )}

          {/* Error state */}
          {error && !loading && (
            <div className="flex flex-col items-center justify-center py-12 gap-3">
              <Lock className="w-8 h-8 text-gx-error" />
              <p className="text-sm text-gx-error max-w-xs text-center">{error}</p>
              <button
                onClick={handleRefresh}
                className="btn btn-primary text-xs"
              >
                <RefreshCw className="w-3 h-3" />
                Retry
              </button>
            </div>
          )}

          {/* Empty state */}
          {!loading && !error && sortedEntries.length === 0 && (
            <div className="flex flex-col items-center justify-center py-12 gap-2">
              <FolderOpen className="w-8 h-8 text-gx-muted/50" />
              <span className="text-sm text-gx-muted">This directory is empty</span>
            </div>
          )}

          {/* List view */}
          {!loading && !error && sortedEntries.length > 0 && viewMode === "list" && (
            <div className="divide-y divide-gx-border/30">
              {sortedEntries.map((entry) => (
                <FileEntryRow
                  key={entry.path}
                  entry={entry}
                  syncStatus={fileSyncStatuses.get(entry.path) ?? fileSyncStatuses.get(entry.name)}
                  onClick={() => handleEntryClick(entry)}
                  onContextMenu={(e) => handleContextMenu(e, entry)}
                  onAction={(e) => handleContextMenu(e, entry)}
                />
              ))}
            </div>
          )}

          {/* Grid view */}
          {!loading && !error && sortedEntries.length > 0 && viewMode === "grid" && (
            <div className="grid grid-cols-[repeat(auto-fill,minmax(100px,1fr))] gap-2 p-3">
              {sortedEntries.map((entry) => (
                <FileEntryCard
                  key={entry.path}
                  entry={entry}
                  onClick={() => handleEntryClick(entry)}
                  onContextMenu={(e) => handleContextMenu(e, entry)}
                />
              ))}
            </div>
          )}
        </div>
      </div>

      {/* Context menu */}
      {contextMenu && (
        <ContextMenu
          x={contextMenu.x}
          y={contextMenu.y}
          entry={contextMenu.entry}
          onOpenTerminal={onOpenTerminal ? () => handleOpenTerminal(contextMenu.entry.kind === "dir" ? contextMenu.entry.path : parentPath(contextMenu.entry.path)) : undefined}
          onSetupSync={onSetupSync ? () => { onSetupSync(profileId, contextMenu.entry.path); } : undefined}
          onDownload={() => { /* placeholder */ }}
          onDelete={() => { /* placeholder */ }}
          onClose={() => setContextMenu(null)}
        />
      )}
    </div>
  );
}

// ── Tree Node Row ────────────────────────────────────────────────

interface TreeNodeRowProps {
  node: TreeNode;
  depth: number;
  currentPath: string;
  onToggle: (path: string) => void;
  onNavigate: (path: string) => void;
}

function TreeNodeRow({ node, depth, currentPath, onToggle, onNavigate }: TreeNodeRowProps) {
  const isActive = currentPath === node.path;

  return (
    <>
      <div
        className={clsx(
          "flex items-center gap-1 px-2 py-0.5 text-xs cursor-pointer select-none",
          "hover:bg-gx-surface-hover transition-colors",
          isActive && "bg-gx-accent/10 text-gx-accent",
        )}
        style={{ paddingLeft: `${depth * 12 + 8}px` }}
      >
        <button
          onClick={() => onToggle(node.path)}
          className="shrink-0 w-4 h-4 flex items-center justify-center"
          aria-label={node.expanded ? "Collapse" : "Expand"}
        >
          {node.loading ? (
            <Loader2 className="w-3 h-3 animate-spin text-gx-muted" />
          ) : (
            <ChevronRight
              className={clsx(
                "w-3 h-3 text-gx-muted transition-transform",
                node.expanded && "rotate-90",
              )}
            />
          )}
        </button>
        <button
          onClick={() => onNavigate(node.path)}
          className="flex items-center gap-1 flex-1 truncate"
        >
          {node.expanded ? (
            <FolderOpen className="w-3.5 h-3.5 text-gx-accent shrink-0" />
          ) : (
            <Folder className="w-3.5 h-3.5 text-gx-accent shrink-0" />
          )}
          <span className="truncate">{node.name}</span>
        </button>
      </div>
      {node.expanded && node.children && node.children.map((child) => (
        <TreeNodeRow
          key={child.path}
          node={child}
          depth={depth + 1}
          currentPath={currentPath}
          onToggle={onToggle}
          onNavigate={onNavigate}
        />
      ))}
    </>
  );
}

// ── File Entry Row (List View) ───────────────────────────────────

interface FileEntryRowProps {
  entry: RemoteDirEntry;
  syncStatus?: FileSyncStatus;
  onClick: () => void;
  onContextMenu: (e: React.MouseEvent) => void;
  onAction: (e: React.MouseEvent) => void;
}

function FileEntryRow({ entry, syncStatus, onClick, onContextMenu, onAction }: FileEntryRowProps) {
  const name = entry.name ?? basename(entry.path);
  const isDir = entry.kind === "dir";

  return (
    <button
      className={clsx(
        "flex items-center gap-2.5 w-full px-3 py-1.5 text-left text-sm group",
        "hover:bg-gx-surface-hover transition-colors",
        "focus:outline-none focus-visible:bg-gx-surface-hover",
      )}
      onClick={onClick}
      onContextMenu={onContextMenu}
      role="row"
      aria-label={`${isDir ? "Directory" : "File"}: ${name}`}
    >
      {isDir ? (
        <Folder className="w-4 h-4 shrink-0 text-gx-accent" />
      ) : (
        <File className="w-4 h-4 shrink-0 text-gx-muted" />
      )}
      {syncStatus && <SyncStatusIcon status={syncStatus} size="sm" />}
      <span className="truncate flex-1 text-gx-text">{name}</span>
      {entry.size != null && !isDir && (
        <span className="text-xs text-gx-muted shrink-0 tabular-nums">
          {formatSize(entry.size)}
        </span>
      )}
      <button
        onClick={(e) => { e.stopPropagation(); onAction(e); }}
        className="opacity-0 group-hover:opacity-100 btn-icon shrink-0 transition-opacity"
        aria-label="More actions"
      >
        <MoreVertical className="w-3.5 h-3.5" />
      </button>
    </button>
  );
}

// ── File Entry Card (Grid View) ──────────────────────────────────

interface FileEntryCardProps {
  entry: RemoteDirEntry;
  onClick: () => void;
  onContextMenu: (e: React.MouseEvent) => void;
}

function FileEntryCard({ entry, onClick, onContextMenu }: FileEntryCardProps) {
  const name = entry.name ?? basename(entry.path);
  const isDir = entry.kind === "dir";

  return (
    <button
      className={clsx(
        "flex flex-col items-center gap-1.5 p-3 rounded-lg text-center",
        "hover:bg-gx-surface-hover transition-colors cursor-pointer",
        "focus:outline-none focus-visible:ring-2 focus-visible:ring-gx-accent",
      )}
      onClick={onClick}
      onContextMenu={onContextMenu}
      aria-label={`${isDir ? "Directory" : "File"}: ${name}`}
    >
      {isDir ? (
        <Folder className="w-10 h-10 text-gx-accent" />
      ) : (
        <File className="w-10 h-10 text-gx-muted" />
      )}
      <span className="text-xs text-gx-text truncate w-full leading-tight">{name}</span>
      {entry.size != null && !isDir && (
        <span className="text-[10px] text-gx-muted tabular-nums">{formatSize(entry.size)}</span>
      )}
    </button>
  );
}

// ── Context Menu ─────────────────────────────────────────────────

interface ContextMenuProps {
  x: number;
  y: number;
  entry: RemoteDirEntry;
  onOpenTerminal?: () => void;
  onSetupSync?: () => void;
  onDownload: () => void;
  onDelete: () => void;
  onClose: () => void;
}

function ContextMenu({ x, y, entry, onOpenTerminal, onSetupSync, onDownload, onDelete, onClose }: ContextMenuProps) {
  const menuRef = useRef<HTMLDivElement>(null);
  const isDir = entry.kind === "dir";

  // Adjust position to stay within viewport
  const [pos, setPos] = useState({ x, y });
  useEffect(() => {
    if (menuRef.current) {
      const rect = menuRef.current.getBoundingClientRect();
      const newX = x + rect.width > window.innerWidth ? x - rect.width : x;
      const newY = y + rect.height > window.innerHeight ? y - rect.height : y;
      setPos({ x: Math.max(0, newX), y: Math.max(0, newY) });
    }
  }, [x, y]);

  return (
    <div
      ref={menuRef}
      className="fixed z-50 min-w-[160px] rounded-lg border border-gx-border bg-gx-surface shadow-lg py-1"
      style={{ left: pos.x, top: pos.y }}
      role="menu"
    >
      {isDir && onOpenTerminal && (
        <ContextMenuItem
          icon={<Terminal className="w-3.5 h-3.5" />}
          label="Open in Terminal"
          onClick={() => { onOpenTerminal(); onClose(); }}
        />
      )}
      {isDir && onSetupSync && (
        <>
          <div className="my-1 border-t border-gx-border/50" />
          <ContextMenuItem
            icon={<RefreshCw className="w-3.5 h-3.5" />}
            label="Sync this folder locally"
            onClick={() => { onSetupSync(); onClose(); }}
          />
        </>
      )}
      <ContextMenuItem
        icon={<Download className="w-3.5 h-3.5" />}
        label={isDir ? "Download Folder" : "Download File"}
        onClick={() => { onDownload(); onClose(); }}
      />
      <div className="my-1 border-t border-gx-border/50" />
      <ContextMenuItem
        icon={<Trash2 className="w-3.5 h-3.5" />}
        label="Delete"
        danger
        onClick={() => { onDelete(); onClose(); }}
      />
    </div>
  );
}

function ContextMenuItem({
  icon,
  label,
  danger,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  danger?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      className={clsx(
        "flex items-center gap-2 w-full px-3 py-1.5 text-xs text-left transition-colors",
        danger
          ? "text-gx-error hover:bg-gx-error/10"
          : "text-gx-text hover:bg-gx-surface-hover",
      )}
      onClick={onClick}
      role="menuitem"
    >
      {icon}
      {label}
    </button>
  );
}
