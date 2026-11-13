import { Button } from "@/components/ui/button";
import { tabVariants } from "@/components/ui/tabs";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { requestPrompt } from "@/lib/application-dialogs";
import clsx from "clsx";
import {
  ArrowUp,
  Binary,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  CloudDownload,
  CloudOff,
  Database,
  Download,
  File,
  FileArchive,
  FileAudio,
  FileCog,
  FileImage,
  FileVideo,
  FileWarning,
  Folder,
  FolderOpen,
  FolderTree,
  Grid3X3,
  History,
  List,
  Loader2,
  Minus,
  Package,
  Plus,
  RefreshCw,
  Trash2,
  Upload,
  XCircle,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { ContextMenu, MenuItemDef } from "../components/ContextMenu";
import { CrabSetupWizard } from "../components/CrabSetupWizard";
import { LargeFileWizard } from "../components/LargeFileWizard";
import { StagedSection } from "../components/track/StagedSection";
import { ToolbarAction, ViewToolbar } from "../components/ViewToolbar";
import { useCrabStatus } from "../hooks/useCrabStatus";
import { useFileTransfers } from "../hooks/useFileTransfers";
import { useHydrationStatus } from "../hooks/useHydrationStatus";
import { useStagingStats } from "../hooks/useStagingStats";
import { DirEntry, HydrationEntry, rpc, StagingStats } from "../ipc";
import { CommandContext, executeCommand } from "../lib/command-registry";
import { formatBytes } from "../lib/format";
import { isEligible, type HydrateVerb } from "../lib/hydrateScope";
import { HydratePage } from "../pages/Hydrate";
import { OpenRepo } from "../state";
import { DatasetsView } from "./DatasetsView";
import { PushFlowPanel } from "./PushFlowPanel";

interface Props {
  repo: OpenRepo;
  onSelectCommit?: (sha: string) => void;
  onOpenFile?: (path: string, entry: DirEntry) => void;
}

interface LargeFileEntry {
  path: string;
  size: number;
  kind: "untracked" | "committed" | "pointer";
  tracked: boolean;
}

/** Tree node for the tree view. */
interface TreeNode {
  name: string;
  path: string;
  isDir: boolean;
  children: TreeNode[];
  file?: LargeFileEntry;
}

type FlowPhase = "browse" | "tracking" | "tracked" | "error";
type ViewMode = "list" | "tree" | "card";
type Tab = "files" | "storage-map" | "hydrate" | "datasets";

/** Get a file type icon component based on file extension. */
function getFileIcon(path: string) {
  const ext = path.split(".").pop()?.toLowerCase() ?? "";
  const ARCHIVE = new Set(["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "dmg", "iso", "img"]);
  const IMAGE = new Set(["png", "jpg", "jpeg", "gif", "bmp", "svg", "webp", "tiff", "ico"]);
  const VIDEO = new Set(["mp4", "avi", "mkv", "mov", "wmv", "flv", "webm"]);
  const AUDIO = new Set(["mp3", "wav", "flac", "aac", "ogg", "wma", "m4a"]);
  const DATA = new Set(["safetensors", "bin", "pt", "pth", "onnx", "h5", "hdf5", "parquet", "arrow", "feather", "pkl", "pickle", "npy", "npz", "db", "sqlite", "sqlite3"]);

  if (ARCHIVE.has(ext)) return { Icon: FileArchive, color: "text-warning" };
  if (IMAGE.has(ext)) return { Icon: FileImage, color: "text-thinking" };
  if (VIDEO.has(ext)) return { Icon: FileVideo, color: "text-thinking" };
  if (AUDIO.has(ext)) return { Icon: FileAudio, color: "text-info" };
  if (DATA.has(ext)) return { Icon: FileCog, color: "text-warning" };
  if (ext === "pdf") return { Icon: File, color: "text-destructive" };
  return { Icon: File, color: "text-primary" };
}

export function CrabFlowView({ repo, onSelectCommit, onOpenFile }: Props) {
  const crabStatus = useCrabStatus(repo.repo_id);
  const fileTransfers = useFileTransfers(repo);

  const [tab, setTab] = useState<Tab>("files");
  const [phase, setPhase] = useState<FlowPhase>("browse");
  const [files, setFiles] = useState<LargeFileEntry[]>([]);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [trackingProgress, setTrackingProgress] = useState<
    Map<string, "pending" | "tracking" | "done" | "error">
  >(new Map());
  const [trackResult, setTrackResult] = useState<{
    tracked: number;
    errors: number;
    errorDetails?: Array<{ path: string; stderr: string }>;
  } | null>(null);
  const [showPush, setShowPush] = useState(false);
  const [showWizard, setShowWizard] = useState(false);
  const [wizardFiles, setWizardFiles] = useState<Array<{ path: string; size: number }>>([]);
  const [expandUntracked, setExpandUntracked] = useState(true);
  const [sizeThreshold, setSizeThreshold] = useState(1);
  const [viewMode, setViewMode] = useState<ViewMode>("list");
  const [contextMenu, setContextMenu] = useState<{
    position: { x: number; y: number };
    filePath: string;
  } | null>(null);

  const hydration = useHydrationStatus(repo);

  // Build CommandContext for executeCommand calls.
  const commandContext = useMemo<CommandContext>(() => ({
    repoId: repo.repo_id,
    repoPath: repo.path,
    fileTransfers,
    currentBranch: "main",
    upstream: null,
    ahead: 0,
    behind: 0,
    dirtyCount: 0,
    hasStagedChanges: false,
    activeOperation: null,
    forgeProvider: null,
  }), [repo.repo_id, repo.path, fileTransfers]);

  // Toolbar actions for the Large Files view.
  const toolbarActions = useMemo<ToolbarAction[]>(() => [
    { id: "crab.track", icon: Plus, label: "Add/Track" },
    { id: "crab.push", icon: ArrowUp, label: "Push" },
  ], []);

  const handleToolbarAction = useCallback(
    async (actionId: string) => {
      if (actionId === "crab.push") {
      setShowPush(true);
    } else if (actionId === "crab.track") {
        const pattern = await requestPrompt(
          "Enter file pattern to track (e.g. *.bin, models/):",
        );
        if (!pattern) return;
        await executeCommand("crab.track", commandContext, pattern);
      } else {
      await executeCommand(actionId, commandContext);
    }
    },
    [commandContext],
  );

  // Context menu items for right-clicking files.
  const contextMenuItems = useMemo<MenuItemDef[]>(() => {
    const item = contextMenu && hydration.byPath.get(contextMenu.filePath);
    const actions: MenuItemDef[] = [];
    if (item && repo.native_session_id) {
      if (repo.capabilities?.canHydrateSelection && isEligible(item, "hydrate")) actions.push({ id: "crab.hydrate.file", label: "Hydrate", icon: Download });
      if (repo.capabilities?.canDehydrateSelection && isEligible(item, "dehydrate")) actions.push({ id: "crab.dehydrate.file", label: "Dehydrate", icon: Upload });
    }
    actions.push({ id: "crab.track.file", label: "Track", icon: Plus, separator: actions.length > 0 }, { id: "crab.untrack.file", label: "Untrack", icon: Minus });
    return actions;
  }, [contextMenu, hydration.byPath, repo.native_session_id, repo.capabilities]);

  const handleContextMenuSelect = useCallback(async (itemId: string) => {
    if (!contextMenu) return;
    const path = contextMenu.filePath;
    setContextMenu(null);
    await executeCommand(itemId, commandContext, path);
  }, [contextMenu, commandContext]);

  const handleFileContextMenu = useCallback((e: React.MouseEvent, filePath: string) => {
    e.preventDefault();
    setContextMenu({ position: { x: e.clientX, y: e.clientY }, filePath });
  }, []);

  // Per-path git porcelain status code, used for staged-section
  // commit-state derivation. Refreshed alongside `files` so the
  // rendered status matches the rest of the panel.
  const [gitStatusByPath, setGitStatusByPath] = useState<Map<string, string>>(new Map());

  // Load files.
  const loadFiles = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const { entries } = await rpc.list(repo.repo_id, {});
      const { entries: statusEntries } = await rpc.gitStatus(repo.repo_id);

      const statusMap = new Map<string, string>();
      for (const e of statusEntries) statusMap.set(e.path, e.code);
      setGitStatusByPath(statusMap);

      const hydrationMap = new Map<string, HydrationEntry>();
      if (hydration.byPath) {
        for (const [path, entry] of hydration.byPath) {
          hydrationMap.set(path, entry);
        }
      }

      const thresholdBytes = sizeThreshold * 1024 * 1024;
      const largeFiles: LargeFileEntry[] = [];
      const seenPaths = new Set<string>();

      for (const entry of entries) {
        if (entry.kind === "dir" || entry.kind === "submodule") continue;
        seenPaths.add(entry.path);
        const isPointer = entry.kind === "pointer" || entry.kind === "lfs";
        const hydEntry = hydrationMap.get(entry.path);
        const size = entry.size ?? 0;

        if (isPointer) {
          largeFiles.push({ path: entry.path, size: hydEntry?.logical ?? size, kind: "pointer", tracked: true });
        } else if (size >= thresholdBytes) {
          largeFiles.push({ path: entry.path, size, kind: "committed", tracked: false });
        }
      }

      for (const statusEntry of statusEntries) {
        const code = statusEntry.code.trim();
        if (code !== "??") continue;
        if (seenPaths.has(statusEntry.path)) continue;
        if (statusEntry.path.startsWith(".") && statusEntry.path !== ".gitattributes") continue;

        let size = 0;
        try {
          const stat = await rpc.stat(repo.repo_id, statusEntry.path);
          size = stat.size;
        } catch {
          size = 0;
        }

        if (size > 0 && size < thresholdBytes) continue;
        if (size === 0) {
          const ext = statusEntry.path.split(".").pop()?.toLowerCase() ?? "";
          const LARGE_EXT = new Set(["safetensors","bin","pt","pth","onnx","h5","hdf5","parquet","arrow","feather","pkl","pickle","npy","npz","tar","gz","tgz","zip","7z","rar","bz2","xz","zst","dmg","iso","img","exe","msi","deb","rpm","pkg","mp4","avi","mkv","mov","wav","mp3","flac","db","sqlite","sqlite3"]);
          if (thresholdBytes > 0 && !LARGE_EXT.has(ext)) continue;
        }

        largeFiles.push({ path: statusEntry.path, size, kind: "untracked", tracked: false });
      }

      largeFiles.sort((a, b) => {
        if (a.tracked !== b.tracked) return a.tracked ? 1 : -1;
        return b.size - a.size;
      });
      setFiles(largeFiles);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [repo.repo_id, hydration.byPath, sizeThreshold]);

  useEffect(() => { loadFiles(); }, [loadFiles]);

  const untrackedFiles = useMemo(() => files.filter((f) => !f.tracked), [files]);

  // Real staging area stats with auto-refresh on file watcher events
  // and a slow polling fallback. This makes `crab add` from the CLI
  // visible in the desktop without manual refresh.
  const stagingStatsHook = useStagingStats(repo.repo_id, repo.path);
  const stagingStats = stagingStatsHook.stats;

  const toggleSelect = (path: string) => {
    setSelected((prev) => { const n = new Set(prev); if (n.has(path)) n.delete(path); else n.add(path); return n; });
  };
  const selectAll = () => setSelected(new Set(untrackedFiles.map((f) => f.path)));
  const selectNone = () => setSelected(new Set());

  const handleTrack = async () => {
    if (selected.size === 0) return;
    // Launch the guided wizard instead of inline tracking.
    const selectedFiles = untrackedFiles
      .filter((f) => selected.has(f.path))
      .map((f) => ({ path: f.path, size: f.size }));
    setWizardFiles(selectedFiles);
    setShowWizard(true);
  };

  const handleReset = () => {
    setPhase("browse"); setSelected(new Set()); setTrackingProgress(new Map());
    setTrackResult(null); setError(null); setShowPush(false);
    setShowWizard(false); setWizardFiles([]);
  };

  const needsSetup = crabStatus.status === "uninitialized" || crabStatus.status === "no-storage";
  const actions = needsSetup || crabStatus.status === "loading" ? [] : toolbarActions;

  if (showWizard) {
    return (
      <div className="flex h-full flex-col">
        <LargeFileWizard
          repo={repo}
          files={wizardFiles}
          onComplete={() => {
            setShowWizard(false);
            setWizardFiles([]);
            setSelected(new Set());
            setPhase("browse");
            loadFiles();
            hydration.refresh();
            stagingStatsHook.refresh();
          }}
          onBack={() => {
            setShowWizard(false);
            setWizardFiles([]);
          }}
        />
      </div>
    );
  }

  if (showPush) {
    return (
      <div className="flex h-full flex-col">
        <div className="border-b border-border px-4 py-2">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="text-xs text-primary hover:underline"
            onClick={() => setShowPush(false)}
          >
            ← Back to files
          </Button>
        </div>
        <PushFlowPanel repo={repo} onComplete={() => { setShowPush(false); loadFiles(); }} />
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {/* View toolbar */}
      <ViewToolbar
        title="Large Files"
        icon={Package}
        actions={actions}
        onAction={handleToolbarAction}
      />

      {/* Tab bar */}
      <div className="flex shrink-0 items-center gap-0 border-b border-border bg-card">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={tabVariants({ active: tab === "files" })}
          onClick={() => setTab("files")}
        >
          <span className="flex items-center gap-1.5">
            <Package size={12} /> Track &amp; Stage
          </span>
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={tabVariants({ active: tab === "storage-map" })}
          onClick={() => setTab("storage-map")}
        >
          <span className="flex items-center gap-1.5">
            <Binary size={12} /> Storage Map
          </span>
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={tabVariants({ active: tab === "hydrate" })}
          onClick={() => setTab("hydrate")}
        >
          <span className="flex items-center gap-1.5">
            <Download size={12} /> Hydrate
          </span>
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={tabVariants({ active: tab === "datasets" })}
          onClick={() => setTab("datasets")}
        >
          <span className="flex items-center gap-1.5">
            <Database size={12} /> Datasets
          </span>
        </Button>
      </div>

      {/* Dataset inspection uses captured reads and does not require Crab storage. */}
      {tab === "datasets" ? (
        <DatasetsView repo={repo} onOpenFile={onOpenFile ?? (() => {})} />
      ) : tab === "hydrate" ? (
        <HydratePage repo={repo} />
      ) : crabStatus.status === "loading" ? (
        <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
          <Loader2 size={14} className="animate-spin" /> Checking repository
          status…
        </div>
      ) : needsSetup ? (
        <CrabSetupWizard repoId={repo.repo_id} onComplete={crabStatus.refresh} />
      ) : tab === "storage-map" ? (
        <PointerMapPanel repo={repo} onSelectCommit={onSelectCommit} />
      ) : (
        <FilesPanel
          loading={loading}
          error={error ?? stagingStatsHook.error}
          phase={phase}
          untrackedFiles={untrackedFiles}
          stagingStats={stagingStats}
          stagingStatsLoading={stagingStatsHook.loading}
          gitStatusByPath={gitStatusByPath}
          selected={selected}
          trackingProgress={trackingProgress}
          trackResult={trackResult}
          expandUntracked={expandUntracked}
          sizeThreshold={sizeThreshold}
          viewMode={viewMode}
          repo={repo}
          onSetExpandUntracked={setExpandUntracked}
          onSetSizeThreshold={setSizeThreshold}
          onSetViewMode={setViewMode}
          onToggleSelect={toggleSelect}
          onSelectAll={selectAll}
          onSelectNone={selectNone}
          onTrack={handleTrack}
          onReset={handleReset}
          onShowPush={() => setShowPush(true)}
          onRefresh={loadFiles}
          onRefreshStaging={stagingStatsHook.refresh}
          onFileContextMenu={handleFileContextMenu}
        />
      )}

      {/* Right-click context menu */}
      {contextMenu && (
        <ContextMenu
          items={contextMenuItems}
          position={contextMenu.position}
          onSelect={handleContextMenuSelect}
          onClose={() => setContextMenu(null)}
        />
      )}
    </div>
  );
}

// ── Pointer Map (Storage Map) panel ─────────────────────────────────

interface PointerEntry {
  path: string;
  size: number;
  kind: string;
  pointer_hash: string | null;
}

interface PointerTreeNode {
  name: string;
  path: string;
  isDir: boolean;
  children: PointerTreeNode[];
  entry?: PointerEntry;
  /** Aggregated size of all descendant files. */
  totalSize: number;
}

type StorageViewMode = "list" | "tree";

function relativeTime(iso: string): string {
  try {
    const d = new Date(iso);
    const diffMs = Date.now() - d.getTime();
    if (diffMs < 60_000) return "just now";
    if (diffMs < 3_600_000) return `${Math.floor(diffMs / 60_000)}m ago`;
    if (diffMs < 86_400_000) return `${Math.floor(diffMs / 3_600_000)}h ago`;
    if (diffMs < 604_800_000) return `${Math.floor(diffMs / 86_400_000)}d ago`;
    return d.toLocaleDateString();
  } catch {
    return iso;
  }
}

function buildPointerTree(entries: PointerEntry[]): PointerTreeNode[] {
  const root: PointerTreeNode = { name: "", path: "", isDir: true, children: [], totalSize: 0 };
  for (const entry of entries) {
    const parts = entry.path.split("/");
    let current = root;
    for (let i = 0; i < parts.length; i++) {
      const part = parts[i];
      const isLast = i === parts.length - 1;
      if (isLast) {
        current.children.push({ name: part, path: entry.path, isDir: false, children: [], entry, totalSize: entry.size });
      } else {
        let child = current.children.find((c) => c.isDir && c.name === part);
        if (!child) {
          child = { name: part, path: parts.slice(0, i + 1).join("/"), isDir: true, children: [], totalSize: 0 };
          current.children.push(child);
        }
        current = child;
      }
    }
  }
  // Compute aggregated sizes and collapse single-child dirs.
  function computeSize(node: PointerTreeNode): number {
    if (!node.isDir) return node.totalSize;
    node.totalSize = node.children.reduce((sum, c) => sum + computeSize(c), 0);
    return node.totalSize;
  }
  function collapse(node: PointerTreeNode): PointerTreeNode {
    if (node.isDir && node.children.length === 1 && node.children[0].isDir) {
      const child = node.children[0];
      return collapse({ ...child, name: node.name ? `${node.name}/${child.name}` : child.name });
    }
    return { ...node, children: node.children.map(collapse) };
  }
  computeSize(root);
  // Sort dirs first, then by size descending.
  function sortTree(nodes: PointerTreeNode[]): PointerTreeNode[] {
    return nodes
      .map((n) => (n.isDir ? { ...n, children: sortTree(n.children) } : n))
      .sort((a, b) => {
        if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
        return b.totalSize - a.totalSize;
      });
  }
  return sortTree(root.children.map(collapse));
}

function PointerMapPanel({ repo, onSelectCommit }: { repo: OpenRepo; onSelectCommit?: (sha: string) => void }) {
  const fileTransfers = useFileTransfers(repo);
  const [data, setData] = useState<PointerEntry[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [limit, setLimit] = useState<50 | 100 | 250 | 500>(50);
  const [mode, setMode] = useState<StorageViewMode>("list");
  const [historyFile, setHistoryFile] = useState<PointerEntry | null>(null);
  const hydration = useHydrationStatus(repo);
  const [actionInProgress, setActionInProgress] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setData(null);
    setErr(null);
    rpc
      .pointerTop(repo.repo_id, { top_n: limit })
      .then((r) => {
        if (!cancelled) setData(r.entries);
      })
      .catch((e: Error) => {
        if (!cancelled) setErr(e.message);
      });
    return () => { cancelled = true; };
  }, [repo.repo_id, limit]);

  const totalBytes = useMemo(
    () => (data ?? []).reduce((sum, e) => sum + e.size, 0),
    [data],
  );

  const tree = useMemo(() => (data ? buildPointerTree(data) : []), [data]);

  const transfer = async (path: string, verb: HydrateVerb) => {
    if (!fileTransfers[verb] || actionInProgress) return;
    setActionInProgress(path);
    setErr(null);
    try {
      await fileTransfers[verb]?.({ kind: "custom", paths: [path] });
    } catch (error) {
      setErr(error instanceof Error ? error.message : String(error));
    } finally {
      setActionInProgress(null);
    }
  };
  const handleHydrate = repo.native_session_id && repo.capabilities?.canHydrateSelection ? (path: string) => transfer(path, "hydrate") : undefined;
  const handleDehydrate = repo.native_session_id && repo.capabilities?.canDehydrateSelection ? (path: string) => transfer(path, "dehydrate") : undefined;

  return (
    <div className="relative flex min-h-0 flex-1">
      {/* Main list/tree area */}
      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <div className="flex shrink-0 items-center gap-3 border-b border-border bg-card px-3 py-2 text-[11px] text-muted-foreground">
          {/* Main list/tree area */}
          <div className="flex overflow-hidden rounded border border-border">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className={clsx("px-1.5 py-0.5", mode === "list" ? "bg-primary/15 text-foreground" : "text-muted-foreground hover:text-foreground")}
              onClick={() => setMode("list")}
              title="List view"
              aria-label="List view"
            >
              <List size={11} />
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className={clsx("px-1.5 py-0.5", mode === "tree" ? "bg-primary/15 text-foreground" : "text-muted-foreground hover:text-foreground")}
              onClick={() => setMode("tree")}
              title="Tree view"
              aria-label="Tree view"
            >
              <FolderTree size={11} />
            </Button>
          </div>
          <Binary size={13} className="text-primary" />
          <span>
            Top <strong className="text-foreground">{data?.length ?? 0}</strong>{" "}
            pointers · {formatBytes(totalBytes)} total
          </span>
          {/* List/tree toggle — left side */}
          {hydration.summary && (
            <span className="text-[10px]">
              ·{" "}
              <span className="text-success">{hydration.summary.hydrated}</span>{" "}
              hydrated ·{" "}
              <span className="text-primary">{hydration.summary.pointer}</span>{" "}
              pointer
            </span>
          )}
          <div className="ml-auto flex items-center gap-2">
            <Select
              value={":" + String(limit)}
              onValueChange={(selectedValue) => {
                const value = selectedValue.slice(1);
                return setLimit(Number(value) as 50 | 100 | 250 | 500);
              }}
            >
              <SelectTrigger className="rounded border border-border bg-background px-2 py-0.5 text-[11px] focus-visible:outline-none">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectGroup>
                  <SelectItem value={":" + String("50")}>Top 50</SelectItem>
                  <SelectItem value={":" + String("100")}>Top 100</SelectItem>
                  <SelectItem value={":" + String("250")}>Top 250</SelectItem>
                  <SelectItem value={":" + String("500")}>Top 500</SelectItem>
                </SelectGroup>
              </SelectContent>
            </Select>
          </div>
        </div>

        <div className="min-h-0 flex-1 overflow-auto">
          {err && (
            <div className="m-4 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
              {err}
            </div>
          )}
          {!err && data == null && (
            <div className="flex h-full items-center justify-center gap-2 text-sm text-muted-foreground">
              <Loader2 size={14} className="animate-spin" /> Scanning pointers…
            </div>
          )}
          {data && data.length === 0 && (
            <div className="p-6 text-center text-sm text-muted-foreground">
              No pointer-tracked files yet.
            </div>
          )}
          {data && data.length > 0 && mode === "list" && (
            <div>
              {/* Table header */}
              <div className="sticky top-0 z-10 flex items-center gap-2 border-b border-border bg-card px-3 py-1.5 text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
                <span className="shrink-0 w-6 text-right">#</span>
                <span className="shrink-0 w-3" /> {/* icon spacer */}
                <span className="min-w-0 flex-1">File</span>
                <span className="shrink-0 w-16 text-center">Status</span>
                <span className="shrink-0 w-16 text-right">Size</span>
                <span className="shrink-0 w-24 text-right">Share</span>
                <span className="shrink-0 w-14 text-center">Actions</span>
              </div>
              {data.map((e, i) => (
                <PointerListRow
                  key={e.path}
                  entry={e}
                  index={i}
                  totalBytes={totalBytes}
                  onShowHistory={() => setHistoryFile(e)}
                  isActive={historyFile?.path === e.path}
                  hydrationState={hydration.byPath.get(e.path)?.state ?? null}
                  onHydrate={handleHydrate && (() => handleHydrate(e.path))}
                  onDehydrate={handleDehydrate && (() => handleDehydrate(e.path))}
                  actionInProgress={actionInProgress === e.path}
                />
              ))}
            </div>
          )}
          {data && data.length > 0 && mode === "tree" && (
            <div className="py-1">
              {tree.map((node) => (
                <PointerTreeRow
                  key={node.path}
                  node={node}
                  depth={0}
                  totalBytes={totalBytes}
                  onShowHistory={(entry) => setHistoryFile(entry)}
                  activeHistoryPath={historyFile?.path ?? null}
                  hydrationByPath={hydration.byPath}
                  onHydrate={handleHydrate}
                  onDehydrate={handleDehydrate}
                  actionInProgress={actionInProgress}
                />
              ))}
            </div>
          )}
        </div>
      </div>

      {/* History side drawer */}
      {historyFile && (
        <FileHistoryDrawer
          entry={historyFile}
          repoId={repo.repo_id}
          onClose={() => setHistoryFile(null)}
          onSelectCommit={onSelectCommit}
        />
      )}
    </div>
  );
}

// ── Storage Map: List row ────────────────────────────────────────────

function PointerListRow({ entry, index, totalBytes, onShowHistory, isActive, hydrationState, onHydrate, onDehydrate, actionInProgress }: {
  entry: PointerEntry; index: number; totalBytes: number;
  onShowHistory: () => void; isActive: boolean;
  hydrationState: string | null;
  onHydrate?: () => void;
  onDehydrate?: () => void;
  actionInProgress: boolean;
}) {
  const share = totalBytes ? (entry.size / totalBytes) * 100 : 0;
  const fileName = entry.path.split("/").pop() ?? entry.path;
  const dirPath = entry.path.includes("/") ? entry.path.slice(0, entry.path.lastIndexOf("/") + 1) : "";
  const isHydrated = hydrationState === "hydrated";
  const isPointer = hydrationState === "pointer" || hydrationState === "missing";
  const { Icon: FileIcon, color: iconColor } = getFileIcon(entry.path);

  return (
    <div className={clsx("group flex items-center gap-2 border-b border-border/40 px-3 py-1.5 text-xs transition hover:bg-accent", isActive && "bg-primary/5")}>
      <span className="shrink-0 w-6 text-right tabular-nums text-muted-foreground">
        {index + 1}
      </span>
      <FileIcon size={13} className={clsx("shrink-0", iconColor)} />
      <div className="flex min-w-0 flex-1 items-baseline gap-1">
        <span className="truncate font-mono text-foreground">{fileName}</span>
        {dirPath && (
          <span className="truncate text-[10px] text-muted-foreground">
            {dirPath}
          </span>
        )}
      </div>
      {/* History side drawer */}
      <span className={clsx(
        "shrink-0 w-16 text-center rounded px-1.5 py-0.5 text-[10px] font-medium",
        isHydrated ? "bg-success/10 text-success" : "bg-primary/10 text-primary",
      )}>
        {hydrationState ?? "Unknown"}
      </span>
      <span className="shrink-0 w-16 text-right tabular-nums text-muted-foreground">
        {formatBytes(entry.size)}
      </span>
      <div className="flex shrink-0 items-center gap-1.5 w-24 justify-end">
        <span className="relative h-1.5 w-14 overflow-hidden rounded-full bg-background">
          <span className="absolute inset-y-0 left-0 bg-primary" style={{ width: `${share}%` }} />
        </span>
        <span className="tabular-nums text-[10px] text-muted-foreground w-10 text-right">
          {share.toFixed(1)}%
        </span>
      </div>
      {/* Hydration status badge */}
      <div className="flex shrink-0 w-14 items-center justify-center gap-1">
        {actionInProgress ? (
          <Loader2 size={12} className="animate-spin text-muted-foreground" />
        ) : isHydrated && onDehydrate ? (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground transition hover:bg-primary/10 hover:text-primary"
            onClick={onDehydrate}
            title="Dehydrate — replace with pointer to free disk space"
            aria-label={`Dehydrate ${fileName}`}
          >
            <CloudOff size={12} />
          </Button>
        ) : isPointer && onHydrate ? (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground transition hover:bg-success/10 hover:text-success"
            onClick={onHydrate}
            title="Hydrate — download full content"
            aria-label={`Hydrate ${fileName}`}
          >
            <CloudDownload size={12} />
          </Button>
        ) : null}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={clsx(
            "rounded p-1 text-muted-foreground transition hover:bg-primary/10 hover:text-primary",
            isActive && "text-primary",
          )}
          onClick={onShowHistory}
          title="File history"
          aria-label={`Show history for ${fileName}`}
        >
          <History size={12} />
        </Button>
      </div>
    </div>
  );
}

// ── Storage Map: Tree row ───────────────────────────────────────────

function PointerTreeRow({ node, depth, totalBytes, onShowHistory, activeHistoryPath, hydrationByPath, onHydrate, onDehydrate, actionInProgress }: {
  node: PointerTreeNode; depth: number; totalBytes: number;
  onShowHistory: (entry: PointerEntry) => void; activeHistoryPath: string | null;
  hydrationByPath: Map<string, HydrationEntry>;
  onHydrate?: (path: string) => void;
  onDehydrate?: (path: string) => void;
  actionInProgress: string | null;
}) {
  const [dirExpanded, setDirExpanded] = useState(depth < 2);

  if (node.isDir) {
    const share = totalBytes ? (node.totalSize / totalBytes) * 100 : 0;
    return (
      <div>
        <div
          className="flex cursor-pointer items-center gap-1.5 px-2 py-1 text-xs text-muted-foreground hover:bg-accent"
          style={{ paddingLeft: `${depth * 14 + 8}px` }}
          onClick={() => setDirExpanded(!dirExpanded)}
        >
          {dirExpanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
          <Folder size={11} className="text-primary/70" />
          <span className="font-mono">{node.name}/</span>
          <span className="ml-auto flex items-center gap-1.5 text-[10px]">
            <span className="tabular-nums">{formatBytes(node.totalSize)}</span>
            <span className="relative h-1 w-10 overflow-hidden rounded-full bg-background">
              <span className="absolute inset-y-0 left-0 bg-primary/60" style={{ width: `${share}%` }} />
            </span>
            <span className="tabular-nums w-9 text-right">
              {share.toFixed(1)}%
            </span>
          </span>
        </div>
        {dirExpanded && node.children.map((child) => (
          <PointerTreeRow
            key={child.path}
            node={child}
            depth={depth + 1}
            totalBytes={totalBytes}
            onShowHistory={onShowHistory}
            activeHistoryPath={activeHistoryPath}
            hydrationByPath={hydrationByPath}
            onHydrate={onHydrate}
            onDehydrate={onDehydrate}
            actionInProgress={actionInProgress}
          />
        ))}
      </div>
    );
  }

  const entry = node.entry!;
  const share = totalBytes ? (entry.size / totalBytes) * 100 : 0;
  const isActive = activeHistoryPath === entry.path;
  const hydrationState = hydrationByPath.get(entry.path)?.state ?? null;
  const isHydrated = hydrationState === "hydrated";
  const isPointer = hydrationState === "pointer" || hydrationState === "missing";
  const isActionInProgress = actionInProgress === entry.path;
  const { Icon: FileIcon, color: iconColor } = getFileIcon(entry.path);

  return (
    <div
      className={clsx("group flex items-center gap-1.5 px-2 py-1.5 text-xs transition hover:bg-accent", isActive && "bg-primary/5")}
      style={{ paddingLeft: `${depth * 14 + 8}px` }}
    >
      <FileIcon size={13} className={clsx("shrink-0", iconColor)} />
      <span className="min-w-0 flex-1 truncate font-mono text-foreground">
        {node.name}
      </span>
      {/* Hydration status badge */}
      <span className={clsx(
        "shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium",
        isHydrated ? "bg-success/10 text-success" : "bg-primary/10 text-primary",
      )}>
        {hydrationState ?? "Unknown"}
      </span>
      <span className="ml-2 flex items-center gap-1.5 text-[10px] text-muted-foreground">
        <span className="tabular-nums">{formatBytes(entry.size)}</span>
        <span className="relative h-1 w-10 overflow-hidden rounded-full bg-background">
          <span className="absolute inset-y-0 left-0 bg-primary" style={{ width: `${share}%` }} />
        </span>
        <span className="tabular-nums w-9 text-right">{share.toFixed(1)}%</span>
      </span>
      {/* Hydration status badge */}
      <div className="flex shrink-0 items-center gap-0.5">
        {isActionInProgress ? (
          <Loader2 size={11} className="animate-spin text-muted-foreground" />
        ) : isHydrated && onDehydrate ? (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-0.5 text-muted-foreground transition hover:bg-primary/10 hover:text-primary"
            onClick={() => onDehydrate(entry.path)}
            title="Dehydrate — replace with pointer to free disk space"
            aria-label={`Dehydrate ${node.name}`}
          >
            <CloudOff size={11} />
          </Button>
        ) : isPointer && onHydrate ? (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-0.5 text-muted-foreground transition hover:bg-success/10 hover:text-success"
            onClick={() => onHydrate(entry.path)}
            title="Hydrate — download full content"
            aria-label={`Hydrate ${node.name}`}
          >
            <CloudDownload size={11} />
          </Button>
        ) : null}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={clsx(
            "rounded p-0.5 text-muted-foreground transition hover:bg-primary/10 hover:text-primary",
            isActive && "text-primary",
          )}
          onClick={() => onShowHistory(entry)}
          title="File history"
          aria-label={`Show history for ${node.name}`}
        >
          <History size={11} />
        </Button>
      </div>
    </div>
  );
}

// ── File History side drawer ─────────────────────────────────────────

interface CommitHistoryRow {
  sha: string;
  author: string;
  date: string;
  subject: string;
}

function FileHistoryDrawer({ entry, repoId, onClose, onSelectCommit }: {
  entry: PointerEntry; repoId: string; onClose: () => void; onSelectCommit?: (sha: string) => void;
}) {
  const [history, setHistory] = useState<CommitHistoryRow[] | null>(null);
  const [historyErr, setHistoryErr] = useState<string | null>(null);
  const [loadingHistory, setLoadingHistory] = useState(true);

  const fileName = entry.path.split("/").pop() ?? entry.path;

  useEffect(() => {
    let cancelled = false;
    setLoadingHistory(true);
    setHistoryErr(null);
    setHistory(null);
    rpc
      .gitLog(repoId, { path: entry.path, max: 50 })
      .then((r) => {
        if (!cancelled) setHistory(r.commits);
      })
      .catch((e: Error) => {
        if (!cancelled) setHistoryErr(e.message);
      })
      .finally(() => {
        if (!cancelled) setLoadingHistory(false);
      });
    return () => { cancelled = true; };
  }, [repoId, entry.path]);

  return (
    <div className="flex h-full w-72 shrink-0 flex-col border-l border-border bg-card">
      {/* Drawer header */}
      <div className="flex items-center gap-2 border-b border-border px-3 py-2.5">
        <History size={13} className="shrink-0 text-primary" />
        <div className="min-w-0 flex-1">
          <div className="truncate text-xs font-medium text-foreground" title={entry.path}>
            {fileName}
          </div>
          <div className="truncate text-[10px] text-muted-foreground">
            {entry.path}
          </div>
        </div>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="shrink-0 rounded p-1 text-muted-foreground transition hover:bg-accent hover:text-foreground"
          onClick={onClose}
          aria-label="Close history"
        >
          <XCircle size={14} />
        </Button>
      </div>

      {/* Drawer header */}
      <div className="border-b border-border/50 px-3 py-2 text-[10px]">
        <div className="flex items-center justify-between">
          <span className="text-muted-foreground">Size</span>
          <span className="text-foreground">{formatBytes(entry.size)}</span>
        </div>
        <div className="mt-1 flex items-center justify-between">
          <span className="text-muted-foreground">Kind</span>
          <span className="text-primary">{entry.kind}</span>
        </div>
        {entry.pointer_hash && (
          <div className="mt-1 flex items-center justify-between">
            <span className="text-muted-foreground">Pointer</span>
            <span className="font-mono text-foreground">
              {entry.pointer_hash.slice(0, 12)}…
            </span>
          </div>
        )}
      </div>

      {/* Drawer header */}
      <div className="min-h-0 flex-1 overflow-auto">
        <div className="px-3 py-2 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
          Commits ({loadingHistory ? "…" : (history?.length ?? 0)})
        </div>
        {loadingHistory && (
          <div className="flex items-center gap-1.5 px-3 py-4 text-xs text-muted-foreground">
            <Loader2 size={11} className="animate-spin" /> Loading…
          </div>
        )}
        {historyErr && (
          <div className="mx-3 rounded border border-destructive/30 bg-destructive/5 px-2 py-1.5 text-[11px] text-destructive">
            {historyErr}
          </div>
        )}
        {history && history.length === 0 && (
          <div className="px-3 py-4 text-xs text-muted-foreground">
            No commits found for this file.
          </div>
        )}
        {history && history.length > 0 && (
          <div className="space-y-px px-1">
            {history.map((commit) => (
              <div
                key={commit.sha}
                className={clsx("rounded px-2 py-1.5 hover:bg-accent", onSelectCommit && "cursor-pointer")}
                onClick={() => onSelectCommit?.(commit.sha)}
              >
                <div className="flex items-center gap-2">
                  <span className="shrink-0 font-mono text-[10px] text-primary">
                    {commit.sha.slice(0, 7)}
                  </span>
                  <span className="shrink-0 text-[10px] text-muted-foreground">
                    {relativeTime(commit.date)}
                  </span>
                </div>
                <div className="mt-0.5 truncate text-[11px] text-foreground" title={commit.subject}>
                  {commit.subject}
                </div>
                <div className="mt-0.5 text-[10px] text-muted-foreground">
                  {commit.author}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// ── Files (Track & Stage) panel ─────────────────────────────────────

interface FilesPanelProps {
  loading: boolean;
  error: string | null;
  phase: FlowPhase;
  untrackedFiles: LargeFileEntry[];
  stagingStats: StagingStats | null;
  stagingStatsLoading: boolean;
  /** Per-path git porcelain status code, used by the Staged section. */
  gitStatusByPath: Map<string, string>;
  selected: Set<string>;
  trackingProgress: Map<string, "pending" | "tracking" | "done" | "error">;
  trackResult: { tracked: number; errors: number; errorDetails?: Array<{ path: string; stderr: string }> } | null;
  expandUntracked: boolean;
  sizeThreshold: number;
  viewMode: ViewMode;
  repo: OpenRepo;
  onSetExpandUntracked: (v: boolean) => void;
  onSetSizeThreshold: (v: number) => void;
  onSetViewMode: (v: ViewMode) => void;
  onToggleSelect: (path: string) => void;
  onSelectAll: () => void;
  onSelectNone: () => void;
  onTrack: () => void;
  onReset: () => void;
  onShowPush: () => void;
  onRefresh: () => void;
  onRefreshStaging: () => void;
  onFileContextMenu: (e: React.MouseEvent, filePath: string) => void;
}

function FilesPanel({
  loading, error, phase, untrackedFiles, stagingStats, stagingStatsLoading,
  gitStatusByPath,
  selected, trackingProgress, trackResult, expandUntracked,
  sizeThreshold, viewMode, repo,
  onSetExpandUntracked, onSetSizeThreshold, onSetViewMode,
  onToggleSelect, onSelectAll, onSelectNone, onTrack,
  onReset, onShowPush, onRefresh, onRefreshStaging, onFileContextMenu,
}: FilesPanelProps) {
  const [removingFiles, setRemovingFiles] = useState<Set<string>>(new Set());

  // Note: remote selection lives in the push panel now; keeping a
  // duplicate selector here would split state and confuse the user.

  const handleRemoveFile = useCallback(async (fileHash: string) => {
    setRemovingFiles((prev) => new Set(prev).add(fileHash));
    try {
      await rpc.crabResetFiles(repo.repo_id, [`hash:${fileHash}`]);
      onRefresh();
      onRefreshStaging();
    } catch {
      // Silently fail
    } finally {
      setRemovingFiles((prev) => {
        const next = new Set(prev);
        next.delete(fileHash);
        return next;
      });
    }
  }, [repo.repo_id, onRefresh, onRefreshStaging]);

  // Derived: is there *any* uncommitted change in the working tree
  // touching a crab-staged file? `crab add` always `git add`s the
  // pointer blob, but a follow-up `crab track` might have left
  // `.gitattributes` modified without the user noticing. The Staged
  // section uses this to switch between Commit and Push CTAs.
  const needsCommit = useMemo(() => {
    if (!stagingStats?.files?.length) return false;
    for (const f of stagingStats.files) {
      if (!f.file_path) continue;
      const code = gitStatusByPath.get(f.file_path);
      // `??` (untracked) and any non-empty status counts as needing
      // commit, except a fully-clean file (absent from porcelain).
      if (code) return true;
    }
    // Also flag dirty `.gitattributes` since crab track edits it.
    return gitStatusByPath.has(".gitattributes");
  }, [stagingStats, gitStatusByPath]);

  // Best-effort: if we can read ahead/behind from the git pull state
  // we'd surface it here. For now assume any committed-but-not-pushed
  // staging files mean we have unpushed commits — the push panel
  // will resolve the truth.
  const hasUnpushedCommits = !!stagingStats?.files?.length && !needsCommit;

  const handleCommit = useCallback(
    async (message: string) => {
      const paths = (stagingStats?.files ?? []).map(file => file.file_path).filter((path): path is string => !!path);
      if (gitStatusByPath.has(".gitattributes")) paths.push(".gitattributes");
      await rpc.gitCommit(repo.repo_id, { message, paths });
      onRefresh();
      onRefreshStaging();
    },
    [repo.repo_id, stagingStats, gitStatusByPath, onRefresh, onRefreshStaging],
  );

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-border px-4 py-3">
        <div className="flex items-center gap-3">
          {/* Header */}
          <div className="flex overflow-hidden rounded border border-border">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className={clsx("px-1.5 py-1", viewMode === "list" ? "bg-primary/15 text-foreground" : "text-muted-foreground hover:text-foreground")}
              onClick={() => onSetViewMode("list")}
              title="List view"
              aria-label="List view"
            >
              <List size={12} />
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className={clsx("px-1.5 py-1", viewMode === "tree" ? "bg-primary/15 text-foreground" : "text-muted-foreground hover:text-foreground")}
              onClick={() => onSetViewMode("tree")}
              title="Tree view"
              aria-label="Tree view"
            >
              <FolderTree size={12} />
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className={clsx("px-1.5 py-1", viewMode === "card" ? "bg-primary/15 text-foreground" : "text-muted-foreground hover:text-foreground")}
              onClick={() => onSetViewMode("card")}
              title="Card view"
              aria-label="Card view"
            >
              <Grid3X3 size={12} />
            </Button>
          </div>
          {!loading && (
            <span className="text-xs text-muted-foreground">
              {untrackedFiles.length} untracked
            </span>
          )}
        </div>
        <div className="flex items-center gap-2">
          <Select
            value={":" + String(sizeThreshold)}
            onValueChange={(selectedValue) => {
              const value = selectedValue.slice(1);
              return onSetSizeThreshold(Number(value));
            }}
          >
            <SelectTrigger className="rounded border border-border bg-background px-2 py-1 text-[10px] text-muted-foreground focus-visible:outline-none">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectGroup>
                <SelectItem value={":" + String(0)}>All files</SelectItem>
                <SelectItem value={":" + String(1)}>≥ 1 MB</SelectItem>
                <SelectItem value={":" + String(10)}>≥ 10 MB</SelectItem>
                <SelectItem value={":" + String(50)}>≥ 50 MB</SelectItem>
                <SelectItem value={":" + String(100)}>≥ 100 MB</SelectItem>
              </SelectGroup>
            </SelectContent>
          </Select>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="text-muted-foreground hover:text-foreground"
            onClick={onRefresh}
            title="Refresh"
            aria-label="Refresh file list"
          >
            <RefreshCw size={12} />
          </Button>
        </div>
      </div>

      {loading && (
        <div className="flex items-center gap-2 p-4 text-xs text-muted-foreground">
          <Loader2 size={12} className="animate-spin" /> Scanning...
        </div>
      )}
      {error && phase !== "tracking" && (
        <div className="mx-4 mt-3 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
          {error}
        </div>
      )}

      {/* Keep the commit draft mounted while scans refresh the file list. */}
      <div className="min-h-0 flex-1 overflow-auto">
        {/* Keep the commit draft mounted while scans refresh the file list. */}
        <StagedSection
          stats={stagingStats}
          loading={stagingStatsLoading}
          gitStatusByPath={gitStatusByPath}
          needsCommit={needsCommit}
          hasUnpushedCommits={hasUnpushedCommits}
          removingFiles={removingFiles}
          onRemoveFile={handleRemoveFile}
          onShowPush={onShowPush}
          onCommit={handleCommit}
        />

        {/* Keep the commit draft mounted while scans refresh the file list. */}
        {untrackedFiles.length > 0 && (
          <div>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="justify-start flex w-full items-center gap-2 px-4 py-2 text-left text-[11px] font-semibold uppercase tracking-wider text-muted-foreground hover:bg-accent"
              onClick={() => onSetExpandUntracked(!expandUntracked)}
            >
              {expandUntracked ? (
                <ChevronDown size={10} />
              ) : (
                <ChevronRight size={10} />
              )}
              <FileWarning size={11} className="text-warning" />
              Not Tracked ({untrackedFiles.length})
            </Button>
            {expandUntracked && (
              <div className="ml-6 border-l border-border/30">
                {phase === "browse" && untrackedFiles.length > 1 && (
                  <div className="flex items-center gap-3 border-b border-border/50 px-4 py-1.5 text-[10px] text-muted-foreground">
                    <Button
                      variant="ghost"
                      size="compact"
                      type="button"
                      className="hover:text-foreground"
                      onClick={onSelectAll}
                    >
                      Select all
                    </Button>
                    <Button
                      variant="ghost"
                      size="compact"
                      type="button"
                      className="hover:text-foreground"
                      onClick={onSelectNone}
                    >
                      Clear
                    </Button>
                    {selected.size > 0 && (
                      <span className="ml-auto text-primary">
                        {selected.size} selected ·{" "}
                        {formatBytes(untrackedFiles.filter((f) => selected.has(f.path)).reduce((s, f) => s + f.size, 0))}
                      </span>
                    )}
                  </div>
                )}
                {viewMode === "list" ? (
                  untrackedFiles.map((f) => (
                    <FileRow key={f.path} file={f} selected={selected.has(f.path)} onToggle={() => onToggleSelect(f.path)} progress={trackingProgress.get(f.path)} selectable={phase === "browse"} onContextMenu={(e) => onFileContextMenu(e, f.path)} />
                  ))
                ) : viewMode === "tree" ? (
                  <TreeView files={untrackedFiles} selected={selected} onToggle={onToggleSelect} progress={trackingProgress} selectable={phase === "browse"} onFileContextMenu={onFileContextMenu} />
                ) : (
                  <CardView files={untrackedFiles} selected={selected} onToggle={onToggleSelect} selectable={phase === "browse"} />
                )}
                {phase === "browse" && selected.size > 0 && (
                  <div className="border-t border-border/50 px-4 py-3">
                    <Button variant="default" type="button" className="text-xs" onClick={onTrack}>
                      <Package size={11} /> Track {selected.size} file
                      {selected.size > 1 ? "s" : ""}
                    </Button>
                    <p className="mt-1.5 text-[10px] text-muted-foreground">
                      Files will be chunked, deduplicated, and staged for push.
                    </p>
                  </div>
                )}
                {phase === "tracking" && (
                  <div className="flex items-center gap-2 px-4 py-3 text-xs text-primary">
                    <Loader2 size={12} className="animate-spin" />
                    Tracking...
                  </div>
                )}
              </div>
            )}
          </div>
        )}

        {/* ── Tracking result ── */}
        {phase === "tracked" && trackResult && (
          <div className="mx-4 my-3 rounded border border-success/30 bg-success/5 p-3">
            <div className="flex items-center gap-2 text-xs font-medium text-success">
              <CheckCircle2 size={12} />
              {trackResult.tracked} file{trackResult.tracked > 1 ? "s" : ""}{" "}
              tracked
            </div>
            {trackResult.errors > 0 && (
              <div className="mt-2 text-xs text-destructive">
                {trackResult.errorDetails?.map((e) => (
                  <div key={e.path} className="mt-1 font-mono text-[10px]">
                    {e.path}: {e.stderr}
                  </div>
                ))}
              </div>
            )}
            <div className="mt-3 flex gap-2">
              <Button variant="default" type="button" className="text-xs" onClick={onShowPush}>
                <Upload size={11} /> Push
              </Button>
              <Button variant="outline" type="button" className="text-xs" onClick={onReset}>
                Done
              </Button>
            </div>
          </div>
        )}
        {phase === "error" && (
          <div className="mx-4 my-3 space-y-2">
            <div className="rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
              <XCircle size={12} /> {error}
            </div>
            <Button variant="outline" type="button" className="text-xs" onClick={onReset}>
              Try Again
            </Button>
          </div>
        )}

        {/* Empty */}
        {untrackedFiles.length === 0 && !loading && !error && !stagingStats?.files?.length && (
            <div className="flex flex-col items-center justify-center p-8 text-center">
              <FolderOpen size={32} className="mb-3 text-muted-foreground/40" />
              <div className="text-sm text-foreground">
                No large files to track
              </div>
              <div className="mt-1 text-xs text-muted-foreground">
                Files ≥ {sizeThreshold} MB will appear here. Already-tracked
                files are in Storage Map.
              </div>
            </div>
          )}
      </div>
    </div>
  );
}

// ── List view: File row ─────────────────────────────────────────────

function FileRow({ file, selected, onToggle, progress, selectable = true, onContextMenu }: {
  file: LargeFileEntry; selected: boolean; onToggle: () => void;
  progress?: "pending" | "tracking" | "done" | "error"; selectable?: boolean;
  onContextMenu?: (e: React.MouseEvent) => void;
}) {
  const fileName = file.path.split("/").pop() ?? file.path;
  const dirPath = file.path.includes("/") ? file.path.slice(0, file.path.lastIndexOf("/") + 1) : "";
  const { Icon: FileIcon, color: iconColor } = getFileIcon(file.path);

  return (
    <div className={clsx("flex items-center gap-2 px-6 py-2 text-xs transition", selectable && "cursor-pointer hover:bg-accent", selected && "bg-primary/5")} onClick={selectable ? onToggle : undefined} onContextMenu={onContextMenu}>
      {selectable && !progress && (
        <Checkbox
          checked={selected}
          onCheckedChange={onToggle}
          className="h-3.5 w-3.5 shrink-0 rounded border-border accent-primary"
          onClick={(e) => e.stopPropagation()}
        />
      )}
      {progress === "tracking" && (
        <Loader2 size={12} className="shrink-0 animate-spin text-primary" />
      )}
      {progress === "done" && (
        <CheckCircle2 size={12} className="shrink-0 text-success" />
      )}
      {progress === "error" && (
        <XCircle size={12} className="shrink-0 text-destructive" />
      )}
      {!progress && (
        <FileIcon size={14} className={clsx("shrink-0", iconColor)} />
      )}
      <div className="flex min-w-0 flex-1 items-baseline gap-1.5">
        <span className="truncate font-mono text-foreground">{fileName}</span>
        {dirPath && (
          <span className="truncate text-[10px] text-muted-foreground">
            {dirPath}
          </span>
        )}
      </div>
      <span className="shrink-0 tabular-nums text-muted-foreground">
        {file.size > 0 ? formatBytes(file.size) : "?"}
      </span>
      {!file.tracked && file.kind === "untracked" && !progress && (
        <span className="shrink-0 rounded bg-warning/10 px-1.5 py-0.5 text-[10px] font-medium text-warning">
          new
        </span>
      )}
      {!file.tracked && file.kind === "committed" && !progress && (
        <span className="shrink-0 rounded bg-primary/10 px-1.5 py-0.5 text-[10px] font-medium text-primary">
          large
        </span>
      )}
    </div>
  );
}

// ── Tree view ───────────────────────────────────────────────────────

function buildTree(files: LargeFileEntry[]): TreeNode[] {
  const root: TreeNode = { name: "", path: "", isDir: true, children: [] };
  for (const file of files) {
    const parts = file.path.split("/");
    let current = root;
    for (let i = 0; i < parts.length; i++) {
      const part = parts[i];
      const isLast = i === parts.length - 1;
      if (isLast) {
        current.children.push({ name: part, path: file.path, isDir: false, children: [], file });
      } else {
        let child = current.children.find((c) => c.isDir && c.name === part);
        if (!child) {
          child = { name: part, path: parts.slice(0, i + 1).join("/"), isDir: true, children: [] };
          current.children.push(child);
        }
        current = child;
      }
    }
  }
  // Collapse single-child directories.
  function collapse(node: TreeNode): TreeNode {
    if (node.isDir && node.children.length === 1 && node.children[0].isDir) {
      const child = node.children[0];
      return collapse({ ...child, name: node.name ? `${node.name}/${child.name}` : child.name });
    }
    return { ...node, children: node.children.map(collapse) };
  }
  return root.children.map(collapse);
}

function TreeView({ files, selected, onToggle, progress, selectable, tracked, removingPaths, onRemove, onFileContextMenu }: {
  files: LargeFileEntry[];
  selected: Set<string>;
  onToggle: (path: string) => void;
  progress: Map<string, "pending" | "tracking" | "done" | "error">;
  selectable: boolean;
  tracked?: boolean;
  removingPaths?: Set<string>;
  onRemove?: (path: string) => void;
  onFileContextMenu?: (e: React.MouseEvent, filePath: string) => void;
}) {
  const tree = useMemo(() => buildTree(files), [files]);
  return (
    <div className="pl-2">
      {tree.map((node) => (
        <TreeNodeRow key={node.path} node={node} depth={0} selected={selected} onToggle={onToggle} progress={progress} selectable={selectable} tracked={tracked} removingPaths={removingPaths} onRemove={onRemove} onFileContextMenu={onFileContextMenu} />
      ))}
    </div>
  );
}

function TreeNodeRow({ node, depth, selected, onToggle, progress, selectable, tracked, removingPaths, onRemove, onFileContextMenu }: {
  node: TreeNode; depth: number;
  selected: Set<string>; onToggle: (path: string) => void;
  progress: Map<string, "pending" | "tracking" | "done" | "error">;
  selectable: boolean; tracked?: boolean;
  removingPaths?: Set<string>; onRemove?: (path: string) => void;
  onFileContextMenu?: (e: React.MouseEvent, filePath: string) => void;
}) {
  const [expanded, setExpanded] = useState(true);

  if (node.isDir) {
    return (
      <div>
        <div className="flex cursor-pointer items-center gap-1 px-2 py-0.5 text-xs text-muted-foreground hover:bg-accent" style={{ paddingLeft: `${depth * 12 + 8}px` }} onClick={() => setExpanded(!expanded)}>
          {expanded ? <ChevronDown size={10} /> : <ChevronRight size={10} />}
          <Folder size={11} className="text-primary/70" />
          <span className="font-mono">{node.name}/</span>
        </div>
        {expanded &&
          node.children.map((child) => (
            <TreeNodeRow key={child.path} node={child} depth={depth + 1} selected={selected} onToggle={onToggle} progress={progress} selectable={selectable} tracked={tracked} removingPaths={removingPaths} onRemove={onRemove} onFileContextMenu={onFileContextMenu} />
          ))}
      </div>
    );
  }

  const file = node.file!;
  const isSelected = selected.has(file.path);
  const prog = progress.get(file.path);
  const isRemoving = removingPaths?.has(file.path);

  return (
    <div className={clsx("group flex items-center gap-2 py-0.5 text-xs transition", selectable && "cursor-pointer hover:bg-accent", isSelected && "bg-primary/5")} style={{ paddingLeft: `${depth * 12 + 8}px` }} onClick={selectable ? () => onToggle(file.path) : undefined} onContextMenu={onFileContextMenu ? (e) => onFileContextMenu(e, file.path) : undefined}>
      {selectable && !prog && (
        <Checkbox
          checked={isSelected}
          onCheckedChange={() => onToggle(file.path)}
          className="h-3 w-3 shrink-0 rounded border-border accent-primary"
          onClick={(e) => e.stopPropagation()}
        />
      )}
      {prog === "tracking" && (
        <Loader2 size={10} className="shrink-0 animate-spin text-primary" />
      )}
      {prog === "done" && (
        <CheckCircle2 size={10} className="shrink-0 text-success" />
      )}
      {prog === "error" && (
        <XCircle size={10} className="shrink-0 text-destructive" />
      )}
      {!selectable && !prog && (
        <File size={10} className={clsx("shrink-0", tracked ? "text-success" : "text-muted-foreground")} />
      )}
      <span className="min-w-0 flex-1 truncate font-mono text-foreground">
        {node.name}
      </span>
      <span className="shrink-0 tabular-nums text-muted-foreground">
        {file.size > 0 ? formatBytes(file.size) : "?"}
      </span>
      {tracked && !isRemoving && onRemove && (
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="shrink-0 rounded px-1 text-[10px] text-muted-foreground opacity-0 hover:text-destructive group-hover:opacity-100"
          onClick={(e) => { e.stopPropagation(); onRemove(file.path); }}
        >
          <Trash2 size={9} />
        </Button>
      )}
      {isRemoving && (
        <Loader2 size={9} className="shrink-0 animate-spin text-muted-foreground" />
      )}
    </div>
  );
}

// ── Card view (Finder/Explorer style) ───────────────────────────────

function CardView({ files, selected, onToggle, selectable }: {
  files: LargeFileEntry[];
  selected: Set<string>;
  onToggle: (path: string) => void;
  selectable: boolean;
}) {
  return (
    <div className="grid grid-cols-3 gap-2 p-3 sm:grid-cols-4 md:grid-cols-5">
      {files.map((file) => {
        const fileName = file.path.split("/").pop() ?? file.path;
        const { Icon: FileIcon, color: iconColor } = getFileIcon(file.path);
        const isSelected = selected.has(file.path);
        return (
          <div
            key={file.path}
            className={clsx(
              "relative group flex cursor-pointer flex-col items-center gap-1.5 rounded-lg border p-3 text-center transition",
              isSelected
                ? "border-primary bg-primary/5"
                : "border-border/50 hover:border-border hover:bg-accent",
            )}
            onClick={() => selectable && onToggle(file.path)}
          >
            {isSelected && (
              <div className="absolute right-1.5 top-1.5">
                <CheckCircle2 size={12} className="text-primary" />
              </div>
            )}
            <FileIcon size={28} className={clsx(iconColor)} />
            <span className="w-full truncate text-[10px] font-mono text-foreground" title={file.path}>
              {fileName}
            </span>
            <span className="text-[9px] text-muted-foreground">
              {file.size > 0 ? formatBytes(file.size) : "?"}
            </span>
          </div>
        );
      })}
    </div>
  );
}
