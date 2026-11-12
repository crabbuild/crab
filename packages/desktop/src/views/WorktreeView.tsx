import { Button } from "@/components/ui/button";
import { Combobox } from "@/components/ui/combobox";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import {
  ArrowDown,
  ArrowUp,
  ExternalLink,
  FolderOpen,
  FolderTree,
  GitBranch,
  GitCompareArrows,
  HardDrive,
  Loader2,
  Lock,
  LockOpen,
  Move as MoveIcon,
  Palette,
  Pencil,
  Pin,
  Plus,
  RefreshCw,
  Scissors,
  Terminal,
  Trash2,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { ContextMenu,type MenuItemDef } from "../components/ContextMenu";
import { ViewToolbar,type ToolbarAction } from "../components/ViewToolbar";
import { WorktreeRemoveConfirmDialog } from "../components/WorktreeRemoveConfirmDialog";
import { getWatchPath, useFileWatcher } from "../hooks/useFileWatcher";
import { useRepoCapabilities } from "../hooks/useRepoCapabilities";
import { useSessionScroll } from "../hooks/useSessionScroll";
import {
  CommitRow,
  HydratedWorktreeEntry,
  PushEstimate,
  RefEntry,
  rpc,
  WorktreeStatus,
} from "../ipc";
import { executeCommand,type CommandContext } from "../lib/command-registry";
import { formatBytes } from "../lib/format";
import { localPathName } from "../lib/local-path-name";
import { OpenRepo,useStore } from "../state";

/**
 * Parse a remote repo_id (e.g. `ssh://profileId/path`) into its
 * components. Returns null for local repos.
 */
function parseRemoteRepoId(repoId: string): { profileId: string; remotePath: string } | null {
  if (!repoId.startsWith("ssh://")) return null;
  const withoutScheme = repoId.slice("ssh://".length);
  const slashIdx = withoutScheme.indexOf("/");
  if (slashIdx < 0) return { profileId: withoutScheme, remotePath: "/" };
  return { profileId: withoutScheme.slice(0, slashIdx), remotePath: withoutScheme.slice(slashIdx) };
}

interface Props {
  repo: OpenRepo;
  onSelectWorktree: (wt: HydratedWorktreeEntry | null) => void;
  selectedWorktree: HydratedWorktreeEntry | null;
  statusMap: Map<string, WorktreeStatus>;
  onStatusMapChange: (map: Map<string, WorktreeStatus>) => void;
}

type DialogMode = "add-branch" | "add-new-branch" | "add-detached" | null;

/** State for the right-click context menu on worktree cards. */
interface WorktreeContextMenuState {
  x: number;
  y: number;
  wt: HydratedWorktreeEntry;
}

/** Preset colour swatches for worktree labels. */
const COLOUR_PRESETS = [
  { value: "#ef4444", label: "Red" },
  { value: "#f97316", label: "Orange" },
  { value: "#eab308", label: "Yellow" },
  { value: "#22c55e", label: "Green" },
  { value: "#3b82f6", label: "Blue" },
  { value: "#8b5cf6", label: "Purple" },
  { value: "#ec4899", label: "Pink" },
  { value: null, label: "Clear" },
] as const;

/**
 * Worktree management panel. Displays worktrees as cards with
 * per-worktree status (dirty count, ahead/behind, colour accent).
 *
 * Worktrees let developers work on multiple branches simultaneously
 * without stashing or switching — each worktree is a separate
 * checkout of the same repository sharing the same .git directory.
 *
 * When a worktree is selected, the detail panel (file tree + diff +
 * merge-back actions) renders in the main editor area via RepoDetail.
 */
export function WorktreeView({ repo, onSelectWorktree, selectedWorktree, statusMap, onStatusMapChange }: Props) {
  return (
    <WorktreeViewInner
      repo={repo}
      selectedWorktree={selectedWorktree}
      onSelectWorktree={onSelectWorktree}
      statusMap={statusMap}
      onStatusMapChange={onStatusMapChange}
    />
  );
}

function WorktreeViewInner({
  repo,
  selectedWorktree,
  onSelectWorktree,
  statusMap,
  onStatusMapChange,
}: {
  repo: OpenRepo;
  selectedWorktree: HydratedWorktreeEntry | null;
  onSelectWorktree: (wt: HydratedWorktreeEntry | null) => void;
  statusMap: Map<string, WorktreeStatus>;
  onStatusMapChange: (map: Map<string, WorktreeStatus>) => void;
}) {
  const { state, addRepo, setActive } = useStore();
  const capabilities = useRepoCapabilities();
  const scrollRef = useSessionScroll("worktrees");
  const [worktrees, setWorktrees] = useState<HydratedWorktreeEntry[] | null>(null);
  const [estimateMap, setEstimateMap] = useState<Map<string, PushEstimate>>(new Map());
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [dialogMode, setDialogMode] = useState<DialogMode>(null);

  // Add form state
  const [addPath, setAddPath] = useState("");
  const [addBranch, setAddBranch] = useState("");
  const [addStartPoint, setAddStartPoint] = useState("");
  const [addHydration, setAddHydration] = useState<"all" | "on_access" | "pointers_only">("pointers_only");

  // Context menu state
  const [contextMenu, setContextMenu] = useState<WorktreeContextMenuState | null>(null);
  const [renaming, setRenaming] = useState<{ path: string; value: string } | null>(null);
  const [colourPicking, setColourPicking] = useState<{ path: string } | null>(null);
  const [moving, setMoving] = useState<{ path: string; value: string } | null>(null);

  // Compare worktrees state: the source worktree that initiated the compare
  const [comparingFrom, setComparingFrom] = useState<HydratedWorktreeEntry | null>(null);

  // Remove confirmation dialog state
  const [removeConfirm, setRemoveConfirm] = useState<{
    path: string;
    name: string;
    dirtyCount: number;
  } | null>(null);

  /** Fetch status (dirty count + ahead/behind) for all worktrees. */
  function fetchAllStatuses(repoId: string, entries: HydratedWorktreeEntry[]) {
    const promises = entries.map((wt) =>
      rpc
        .worktreeStatus(repoId, wt.path)
        .then((status) => [wt.path, status] as const)
        .catch(() => [wt.path, { dirty_count: 0, ahead: null, behind: null }] as const),
    );
    Promise.all(promises).then((results) => {
      const map = new Map<string, WorktreeStatus>();
      for (const [path, status] of results) {
        map.set(path, status);
      }
      onStatusMapChange(map);
    });
  }

  /** Fetch push estimates (disk usage) for all worktrees. */
  function fetchAllEstimates(repoId: string, entries: HydratedWorktreeEntry[]) {
    const promises = entries.map((wt) =>
      rpc
        .worktreePushEstimate(repoId, wt.path)
        .then((est) => [wt.path, est] as const)
        .catch(() => [wt.path, null] as const),
    );
    Promise.all(promises).then((results) => {
      const map = new Map<string, PushEstimate>();
      for (const [path, est] of results) {
        if (est) map.set(path, est);
      }
      setEstimateMap(map);
    });
  }

  /**
   * Full refresh: fetches worktree list, statuses, and estimates.
   * Shows the loading spinner — use for initial load and explicit user
   * actions (manual refresh, add/remove/prune).
   */
  const refresh = useCallback(() => {
    if (!capabilities.canWorktree) {
      setError("Worktree management is not available for this repository.");
      return;
    }
    setLoading(true);
    setError(null);
    rpc
      .worktreeList(repo.repo_id)
      .then((r) => {
        setWorktrees(r.worktrees);
        fetchAllStatuses(repo.repo_id, r.worktrees);
        fetchAllEstimates(repo.repo_id, r.worktrees);
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [repo.repo_id, capabilities.canWorktree]);

  /**
   * Lightweight refresh triggered by file-system changes. Only updates
   * statuses for already-known worktrees (dirty count, ahead/behind).
   * Does NOT show the loading spinner or re-fetch the full worktree
   * list and push estimates — those rarely change from a file edit.
   */
  const refreshOnFileChange = useCallback(() => {
    if (!capabilities.canWorktree || !worktrees || worktrees.length === 0) return;
    fetchAllStatuses(repo.repo_id, worktrees);
  }, [repo.repo_id, capabilities.canWorktree, worktrees]);

  useEffect(refresh, [refresh]);

  // Auto-refresh statuses (not the full list) when files change on disk.
  // Uses a longer debounce (800ms) to avoid thrashing during bulk operations.
  useFileWatcher(getWatchPath(repo), refreshOnFileChange, 800);

  function resetDialog() {
    setDialogMode(null);
    setAddPath("");
    setAddBranch("");
    setAddStartPoint("");
    setAddHydration("pointers_only");
  }

  async function handleAdd() {
    if (!capabilities.canWorktree || !addPath.trim()) return;
    setBusy(true);
    setError(null);
    try {
      if (dialogMode === "add-new-branch") {
        await rpc.gitWorktreeAdd(repo.repo_id, {
          path: addPath.trim(),
          branch: addBranch.trim() || undefined,
          create_branch: true,
          start_point: addStartPoint.trim() || undefined,
          hydration: addHydration,
        });
      } else if (dialogMode === "add-detached") {
        await rpc.gitWorktreeAdd(repo.repo_id, {
          path: addPath.trim(),
          branch: addBranch.trim() || undefined,
          detach: true,
          hydration: addHydration,
        });
      } else {
        await rpc.gitWorktreeAdd(repo.repo_id, {
          path: addPath.trim(),
          branch: addBranch.trim() || undefined,
          hydration: addHydration,
        });
      }
      resetDialog();
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleRemove(path: string, _force: boolean) {
    if (!capabilities.canWorktree) return;
    setBusy(true);
    setError(null);
    try {
      // Always use --force when removing from the UI. The user already
      // confirmed via the dialog, and git refuses non-force removal for
      // branches with unmerged commits (common for agent worktrees)
      // even when the working tree is clean.
      await rpc.gitWorktreeRemove(repo.repo_id, path, { force: true });
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  /** Show the remove confirmation dialog. Checks dirty status to decide
   *  whether to show the simple or force-remove variant. */
  function requestRemove(path: string) {
    const status = statusMap.get(path);
    const dirtyCount = status?.dirty_count ?? 0;
    const wt = worktrees?.find((w) => w.path === path);
    const name = wt?.meta?.display_name || dirName(path);
    setRemoveConfirm({ path, name, dirtyCount });
  }

  /** Called when the user confirms removal from the dialog. */
  function confirmRemove(force: boolean) {
    if (!removeConfirm) return;
    setRemoveConfirm(null);
    handleRemove(removeConfirm.path, force);
  }

  async function handleLock(path: string) {
    if (!capabilities.canWorktree) return;
    setBusy(true);
    setError(null);
    try {
      await rpc.gitWorktreeLock(repo.repo_id, path);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleUnlock(path: string) {
    if (!capabilities.canWorktree) return;
    setBusy(true);
    setError(null);
    try {
      await rpc.gitWorktreeUnlock(repo.repo_id, path);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handlePush(_path: string, branch: string) {
    if (!capabilities.canWorktree) return;
    setBusy(true);
    setError(null);
    try {
      await rpc.gitPushBranch(repo.repo_id, branch);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handlePull(_path: string, branch: string) {
    if (!capabilities.canWorktree) return;
    setBusy(true);
    setError(null);
    try {
      await rpc.gitPullBranch(repo.repo_id, branch);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleOpen(path: string) {
    if (!capabilities.canWorktree) return;
    setBusy(true);
    setError(null);
    try {
      // If this worktree is already open as a repo entry, just switch to it
      const existing = state.repos.find((r) => r.path === path);
      if (existing) {
        setActive(existing.repo_id);
      } else if (repo.is_remote) {
        // Remote worktree: open via SSH file browser mechanism.
        // The worktree path is on the remote host, so we use
        // openAsRepo which calls open_local on the remote agent.
        const parsed = parseRemoteRepoId(repo.repo_id);
        if (!parsed) throw new Error("Invalid remote repo_id");
        const handle = await (window as any).crab.ssh.openAsRepo(parsed.profileId, path);
        const remoteRepoId = `ssh://${parsed.profileId}${path}`;
        addRepo({
          ...handle,
          repo_id: handle.repo_id ?? remoteRepoId,
          is_remote: true,
          label: `↳ worktree:${path.split("/").pop() ?? path}`,
          path,
          openedAt: Date.now(),
        });
      } else {
        // Local worktree: open via the local agent
        const handle = await rpc.openLocal(path);
        addRepo({
          ...handle,
          label: `↳ worktree:${localPathName(path)}`,
          path,
          openedAt: Date.now(),
        });
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  /** Select a worktree to show its detail panel (file tree + merge actions). */
  function handleSelect(wt: HydratedWorktreeEntry) {
    onSelectWorktree(wt);
  }

  async function handlePrune() {
    if (!capabilities.canWorktree) return;
    setBusy(true);
    setError(null);
    try {
      await rpc.gitWorktreePrune(repo.repo_id);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  // ── Context menu handlers ──────────────────────────────────────

  function handleCardContextMenu(e: React.MouseEvent, wt: HydratedWorktreeEntry) {
    e.preventDefault();
    setContextMenu({ x: e.clientX, y: e.clientY, wt });
  }

  function dismissContextMenu() {
    setContextMenu(null);
  }

  function startRename(wt: HydratedWorktreeEntry) {
    setRenaming({ path: wt.path, value: wt.meta?.display_name || "" });
    dismissContextMenu();
  }

  async function submitRename() {
    if (!renaming) return;
    setBusy(true);
    setError(null);
    try {
      await rpc.worktreeSetMeta(repo.repo_id, {
        path: renaming.path,
        display_name: renaming.value.trim() || null,
      });
      setRenaming(null);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  function startColourPick(wt: HydratedWorktreeEntry) {
    setColourPicking({ path: wt.path });
    dismissContextMenu();
  }

  async function submitColour(colour: string | null) {
    if (!colourPicking) return;
    setBusy(true);
    setError(null);
    try {
      await rpc.worktreeSetMeta(repo.repo_id, {
        path: colourPicking.path,
        colour,
      });
      setColourPicking(null);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  function startMove(wt: HydratedWorktreeEntry) {
    setMoving({ path: wt.path, value: "" });
    dismissContextMenu();
  }

  async function submitMove() {
    if (!moving || !moving.value.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await rpc.gitWorktreeMove(repo.repo_id, moving.path, moving.value.trim());
      setMoving(null);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handlePruneSingle(_wt: HydratedWorktreeEntry) {
    dismissContextMenu();
    if (!capabilities.canWorktree) return;
    setBusy(true);
    setError(null);
    try {
      // Prune removes stale worktree entries; for a single stale worktree
      // we use the global prune which cleans all stale entries.
      await rpc.gitWorktreePrune(repo.repo_id);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  // ── Compare handlers ───────────────────────────────────────────

  function startCompare(wt: HydratedWorktreeEntry) {
    setComparingFrom(wt);
    dismissContextMenu();
  }

  function selectCompareTarget(target: HydratedWorktreeEntry) {
    if (!comparingFrom) return;
    const baseBranch = shortBranch(comparingFrom.branch);
    const headBranch = shortBranch(target.branch);
    const baseLabel = comparingFrom.meta?.display_name || dirName(comparingFrom.path);
    const headLabel = target.meta?.display_name || dirName(target.path);
    // Dispatch event so RepoDetail can render the compare view in the main pane
    window.dispatchEvent(
      new CustomEvent("crab:open-worktree-compare", {
        detail: { baseBranch, headBranch, baseLabel, headLabel },
      }),
    );
    setComparingFrom(null);
  }

  function dismissCompare() {
    setComparingFrom(null);
  }

  /** Extract a short branch name from refs/heads/... */
  function shortBranch(branch: string): string {
    if (branch.startsWith("refs/heads/")) return branch.slice("refs/heads/".length);
    if (branch.startsWith("refs/")) return branch.slice("refs/".length);
    return branch;
  }

  /** Extract the directory name from a full path for display. */
  function dirName(path: string): string {
    const parts = path.split("/");
    return parts[parts.length - 1] || path;
  }

  // Compute summary stats for the status row
  const dirtyWorktreeCount = Array.from(statusMap.values()).filter(
    (s) => s.dirty_count > 0,
  ).length;

  // Build CommandContext for routing actions through executeCommand
  const commandContext: CommandContext = useMemo(() => ({
    repoId: repo.repo_id,
    currentBranch: worktrees?.find((wt) => wt.path === repo.path)?.branch?.replace(/^refs\/heads\//, "") ?? "main",
    upstream: null,
    ahead: 0,
    behind: 0,
    dirtyCount: Array.from(statusMap.values()).reduce((sum, s) => sum + s.dirty_count, 0),
    hasStagedChanges: false,
    activeOperation: null,
    forgeProvider: null,
  }), [repo.repo_id, repo.path, worktrees, statusMap]);

  // ViewToolbar actions
  const toolbarActions: ToolbarAction[] = useMemo(() => [
    { id: "git.worktree.create", icon: Plus, label: "Create Worktree" },
  ], []);

  function handleToolbarAction(actionId: string) {
    if (actionId === "git.worktree.create") {
      setDialogMode("add-branch");
    }
  }

  // Context menu items for the ContextMenu component (spec-required + existing items)
  const worktreeContextMenuItems: MenuItemDef[] = useMemo(() => {
    const items: MenuItemDef[] = [
      { id: "worktree.open-window", label: "Open in New Window", icon: ExternalLink },
    ];
    // "Open in Terminal" for remote worktrees — opens an SSH PTY in the worktree directory
    if (repo.is_remote) {
      items.push({ id: "worktree.open-terminal", label: "Open in Terminal", icon: Terminal });
    }
    items.push(
      { id: "git.checkout", label: "Checkout Branch", icon: GitBranch },
      { id: "worktree.compare", label: "Compare with…", icon: GitCompareArrows, separator: true },
      { id: "worktree.rename", label: "Rename", icon: Pencil, separator: true },
      { id: "worktree.colour", label: "Change colour", icon: Palette },
      { id: "worktree.move", label: "Move", icon: MoveIcon },
    );
    if (contextMenu?.wt.prunable) {
      items.push({ id: "worktree.prune", label: "Prune", icon: Scissors, separator: true });
    }
    items.push({ id: "git.worktree.remove", label: "Remove Worktree", icon: Trash2, danger: true, separator: true });
    return items;
  }, [contextMenu?.wt.prunable, repo.is_remote]);

  async function handleContextMenuAction(actionId: string) {
    if (!contextMenu) return;
    const wt = contextMenu.wt;

    switch (actionId) {
      case "worktree.open-window":
        dismissContextMenu();
        handleOpen(wt.path);
        break;
      case "worktree.open-terminal": {
        dismissContextMenu();
        // Dispatch SSH terminal event with the worktree's remote path as cwd
        const parsed = parseRemoteRepoId(repo.repo_id);
        if (parsed) {
          const conn = state.sshConnections.get(parsed.profileId);
          const hostname = conn?.serverInfo?.hostname ?? "remote";
          window.dispatchEvent(new CustomEvent("crab:open-ssh-terminal-here", {
            detail: { profileId: parsed.profileId, hostname, cwd: wt.path },
          }));
        }
        break;
      }
      case "git.checkout": {
        dismissContextMenu();
        const branch = shortBranch(wt.branch);
        await executeCommand("git.checkout", commandContext, branch);
        break;
      }
      case "worktree.compare":
        startCompare(wt);
        break;
      case "worktree.rename":
        startRename(wt);
        break;
      case "worktree.colour":
        startColourPick(wt);
        break;
      case "worktree.move":
        startMove(wt);
        break;
      case "worktree.prune":
        handlePruneSingle(wt);
        break;
      case "git.worktree.remove":
        dismissContextMenu();
        requestRemove(wt.path);
        break;
    }
  }

  return (
    <>
      {/* Unified status row */}
      <ViewToolbar
        title="Worktrees"
        icon={FolderTree}
        actions={toolbarActions}
        onAction={handleToolbarAction}
      />

      {/* Unified status row */}
      {comparingFrom && worktrees && (
        <WorktreeComparePicker
          source={comparingFrom}
          worktrees={worktrees}
          shortBranch={shortBranch}
          dirName={dirName}
          onSelect={selectCompareTarget}
          onDismiss={dismissCompare}
        />
      )}

      {/* Add worktree button group */}
      {removeConfirm && (
        <WorktreeRemoveConfirmDialog
          worktreeName={removeConfirm.name}
          worktreePath={removeConfirm.path}
          dirtyCount={removeConfirm.dirtyCount}
          onConfirm={confirmRemove}
          onCancel={() => setRemoveConfirm(null)}
        />
      )}

      {/* Add worktree dialog */}
      <div className="shrink-0 border-b border-border px-3 py-2">
        <div className="mb-2 flex items-center justify-between text-[10px] uppercase tracking-wider text-muted-foreground">
          <span className="flex items-center gap-1">
            <FolderTree size={11} /> Worktrees
          </span>
          <div className="flex items-center gap-1">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="text-muted-foreground hover:text-foreground"
              onClick={handlePrune}
              title="Prune stale worktrees"
              aria-label="Prune stale worktrees"
              disabled={busy}
            >
              <Scissors size={11} />
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="text-muted-foreground hover:text-foreground"
              onClick={refresh}
              title="Refresh"
              aria-label="Refresh worktrees"
            >
              <RefreshCw size={11} />
            </Button>
          </div>
        </div>

        {/* Worktree cards */}
        {worktrees && worktrees.length > 0 && (
          <div className="mb-2 rounded bg-card px-2 py-1 text-[11px] text-muted-foreground">
            {worktrees.length} worktree{worktrees.length !== 1 ? "s" : ""}
            {dirtyWorktreeCount > 0 && (
              <span className="text-warning">
                , {dirtyWorktreeCount} with uncommitted changes
              </span>
            )}
          </div>
        )}

        {/* Worktree cards */}
        {!dialogMode && (
          <div className="flex gap-1">
            <Button variant="default"
              type="button"
              className="flex-1 justify-center text-xs"
              onClick={() => setDialogMode("add-branch")}
              disabled={busy}
            >
              <Plus size={12} /> Add Worktree
            </Button>
          </div>
        )}

        {/* Context menu — uses the reusable ContextMenu component */}
        {dialogMode && (
          <WorktreeAddDialog
            repo={repo}
            dialogMode={dialogMode}
            setDialogMode={setDialogMode}
            addPath={addPath}
            setAddPath={setAddPath}
            addBranch={addBranch}
            setAddBranch={setAddBranch}
            addStartPoint={addStartPoint}
            setAddStartPoint={setAddStartPoint}
            addHydration={addHydration}
            setAddHydration={setAddHydration}
            busy={busy}
            onSubmit={handleAdd}
            onCancel={resetDialog}
            checkedOutBranches={worktrees?.map((wt) => wt.branch) ?? []}
          />
        )}
      </div>

      {/* Context menu — uses the reusable ContextMenu component */}
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto p-2">
        {loading && (
          <div className="flex items-center gap-2 p-3 text-xs text-muted-foreground">
            <Loader2 size={12} className="animate-spin" /> Loading worktrees…
          </div>
        )}
        {error && (
          <div className="m-1 rounded border border-destructive/40 bg-destructive/10 p-3 text-xs text-destructive">
            {error}
          </div>
        )}
        {!loading && worktrees && worktrees.length === 0 && (
          <div className="p-4 text-center text-xs text-muted-foreground">
            No worktrees found.
          </div>
        )}
        {worktrees && worktrees.length > 0 && (
          <div className="flex flex-col gap-2">
            {worktrees.map((wt) => (
              <WorktreeCard
                key={wt.path}
                wt={wt}
                status={statusMap.get(wt.path)}
                estimate={estimateMap.get(wt.path)}
                isActive={wt.path === repo.path}
                isSelected={selectedWorktree?.path === wt.path}
                busy={busy}
                shortBranch={shortBranch}
                dirName={dirName}
                onSelect={handleSelect}
                onOpen={handleOpen}
                onPush={handlePush}
                onPull={handlePull}
                onLock={handleLock}
                onUnlock={handleUnlock}
                onRemove={requestRemove}
                onContextMenu={handleCardContextMenu}
                renaming={renaming?.path === wt.path ? renaming : null}
                onRenameChange={(v) =>
                  setRenaming((r) => (r ? { ...r, value: v } : null))
                }
                onRenameSubmit={submitRename}
                onRenameCancel={() => setRenaming(null)}
                colourPicking={colourPicking?.path === wt.path}
                onColourSelect={submitColour}
                onColourCancel={() => setColourPicking(null)}
                moving={moving?.path === wt.path ? moving : null}
                onMoveChange={(v) =>
                  setMoving((m) => (m ? { ...m, value: v } : null))
                }
                onMoveSubmit={submitMove}
                onMoveCancel={() => setMoving(null)}
              />
            ))}
          </div>
        )}

        {/* Context menu — uses the reusable ContextMenu component */}
        {contextMenu && (
          <ContextMenu
            items={worktreeContextMenuItems}
            position={{ x: contextMenu.x, y: contextMenu.y }}
            onSelect={handleContextMenuAction}
            onClose={dismissContextMenu}
          />
        )}
      </div>
    </>
  );
}

// ── Searchable Select Popup ──────────────────────────────────────────

interface SelectOption {
  value: string;
  label: string;
  detail?: string;
  disabled?: boolean;
}

/**
 * A searchable dropdown popup for selecting branches, commits, or tags.
 * Opens as a floating panel with a search input and scrollable list.
 */

/**
 * A button that opens a searchable select popup. Shows the current
 * selection or a placeholder.
 */

// ── Worktree Add Dialog ──────────────────────────────────────────────

function WorktreeAddDialog({
  repo,
  dialogMode,
  setDialogMode,
  addPath,
  setAddPath,
  addBranch,
  setAddBranch,
  addStartPoint,
  setAddStartPoint,
  addHydration,
  setAddHydration,
  busy,
  onSubmit,
  onCancel,
  checkedOutBranches,
}: {
  repo: OpenRepo;
  dialogMode: DialogMode;
  setDialogMode: (mode: DialogMode) => void;
  addPath: string;
  setAddPath: (v: string) => void;
  addBranch: string;
  setAddBranch: (v: string) => void;
  addStartPoint: string;
  setAddStartPoint: (v: string) => void;
  addHydration: "all" | "on_access" | "pointers_only";
  setAddHydration: (v: "all" | "on_access" | "pointers_only") => void;
  busy: boolean;
  onSubmit: () => void;
  onCancel: () => void;
  checkedOutBranches: string[];
}) {
  const [refs, setRefs] = useState<RefEntry[] | null>(null);
  const [commits, setCommits] = useState<CommitRow[] | null>(null);
  const [refsLoading, setRefsLoading] = useState(false);
  const [commitsLoading, setCommitsLoading] = useState(false);
  const [showBranchPicker, setShowBranchPicker] = useState(false);
  const [showStartPointPicker, setShowStartPointPicker] = useState(false);
  const [showDetachedPicker, setShowDetachedPicker] = useState(false);

  // Fetch refs (branches + tags) when dialog opens
  useEffect(() => {
    setRefsLoading(true);
    rpc
      .listRefs(repo.repo_id)
      .then((r) => setRefs(r.refs))
      .catch(() => setRefs([]))
      .finally(() => setRefsLoading(false));
  }, [repo.repo_id]);

  // Fetch recent commits for start point / detached picker
  useEffect(() => {
    setCommitsLoading(true);
    rpc
      .gitLog(repo.repo_id, { max: 100, all: true })
      .then((r) => setCommits(r.commits))
      .catch(() => setCommits([]))
      .finally(() => setCommitsLoading(false));
  }, [repo.repo_id]);

  // Normalize checked-out branch names for comparison
  const checkedOutSet = useMemo(() => {
    const set = new Set<string>();
    for (const b of checkedOutBranches) {
      set.add(b.replace(/^refs\/heads\//, ""));
    }
    return set;
  }, [checkedOutBranches]);

  // Build branch options (local branches only for "Existing" tab)
  // Branches already checked out in another worktree are shown but
  // marked as unavailable with a visual indicator.
  const branchOptions: SelectOption[] = useMemo(() => {
    if (!refs) return [];
    return refs
      .filter((r) => r.kind === "branch")
      .map((r) => {
        const short = r.name.replace(/^refs\/heads\//, "");
        const inUse = checkedOutSet.has(short);
        return {
          value: short,
          label: inUse ? `${short} (in use)` : short,
          detail: inUse
            ? "Already checked out in another worktree"
            : r.target?.slice(0, 7) || undefined,
          disabled: inUse,
        };
      });
  }, [refs, checkedOutSet]);

  // Build commit options for start point / detached picker
  const commitOptions: SelectOption[] = useMemo(() => {
    const opts: SelectOption[] = [];
    // Add tags first
    if (refs) {
      for (const r of refs) {
        if (r.kind === "tag") {
          const short = r.name.replace(/^refs\/tags\//, "");
          opts.push({
            value: short,
            label: `🏷 ${short}`,
            detail: r.target?.slice(0, 7) || undefined,
          });
        }
      }
    }
    // Add branches as potential start points
    if (refs) {
      for (const r of refs) {
        if (r.kind === "branch") {
          const short = r.name.replace(/^refs\/heads\//, "");
          opts.push({
            value: short,
            label: `⎇ ${short}`,
            detail: r.target?.slice(0, 7) || undefined,
          });
        }
      }
    }
    // Add recent commits
    if (commits) {
      for (const c of commits) {
        opts.push({
          value: c.sha.slice(0, 7),
          label: c.subject.length > 50 ? c.subject.slice(0, 50) + "…" : c.subject,
          detail: `${c.sha.slice(0, 7)} · ${c.author} · ${c.date}`,
        });
      }
    }
    return opts;
  }, [refs, commits]);

  async function handlePickPath() {
    if (repo.is_remote) {
      // Remote repos can't use a native file dialog — the text input
      // handles path entry directly. This function is only called for
      // local repos.
      return;
    }
    const selected = await window.crab.pickWorktreePath();
    if (selected) {
      setAddPath(selected);
    }
  }

  return (
    <div className="mt-2 rounded border border-border bg-background p-2">
      <div className="mb-2 flex items-center justify-between">
        <span className="text-[11px] font-medium text-foreground">
          {dialogMode === "add-new-branch"
            ? "New Branch Worktree"
            : dialogMode === "add-detached"
              ? "Detached Worktree"
              : "Checkout Branch in Worktree"}
        </span>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="text-muted-foreground hover:text-foreground"
          onClick={onCancel}
          aria-label="Cancel"
        >
          <X size={12} />
        </Button>
      </div>

      {/* Mode tabs */}
      <div className="mb-2 flex gap-1 text-[10px]">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={`rounded px-2 py-0.5 transition ${
            dialogMode === "add-branch"
              ? "bg-primary/20 text-foreground"
              : "text-muted-foreground hover:text-foreground"
          }`}
          onClick={() => setDialogMode("add-branch")}
        >
          Existing
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={`rounded px-2 py-0.5 transition ${
            dialogMode === "add-new-branch"
              ? "bg-primary/20 text-foreground"
              : "text-muted-foreground hover:text-foreground"
          }`}
          onClick={() => setDialogMode("add-new-branch")}
        >
          New Branch
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={`rounded px-2 py-0.5 transition ${
            dialogMode === "add-detached"
              ? "bg-primary/20 text-foreground"
              : "text-muted-foreground hover:text-foreground"
          }`}
          onClick={() => setDialogMode("add-detached")}
        >
          Detached
        </Button>
      </div>

      {/* Mode tabs */}
      {repo.is_remote ? (
        <div className="mb-1.5">
          <Input
            className="w-full rounded border border-border bg-background px-2 py-1 text-xs focus-visible:outline-none focus:border-primary"
            placeholder="Relative path (e.g. ../my-worktree)"
            value={addPath}
            onChange={(e) => setAddPath(e.target.value)}
          />
          <p className="mt-0.5 text-[9px] leading-tight text-muted-foreground/70">
            Path is resolved relative to the remote project directory.
          </p>
        </div>
      ) : (
        <div className="mb-1.5 flex items-center gap-1">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="flex flex-1 items-center justify-between rounded border border-border bg-background px-2 py-1 text-left text-xs transition hover:border-muted-foreground/60 focus-visible:outline-none focus:border-primary"
            onClick={handlePickPath}
            title="Select worktree folder"
          >
            <span className={addPath ? "truncate text-foreground" : "text-muted-foreground/60"}>
              {addPath || "Select worktree location…"}
            </span>
            <FolderOpen size={12} className="shrink-0 text-muted-foreground" />
          </Button>
          {addPath && (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="shrink-0 rounded p-1 text-muted-foreground hover:text-foreground"
              onClick={() => setAddPath("")}
              title="Clear path"
              aria-label="Clear path"
            >
              <X size={11} />
            </Button>
          )}
        </div>
      )}

      {/* Path selector — text input for remote repos, folder picker for local */}
      {dialogMode === "add-branch" && (
        <Combobox
          options={branchOptions}
          value={addBranch}
          emptyMessage="No branches found"
          loading={refsLoading}
          placeholder="Select branch to check out…"
          searchPlaceholder="Search branches…"
          onValueChange={setAddBranch}
          open={showBranchPicker}
          onOpenChange={setShowBranchPicker}
        />
      )}

      {dialogMode === "add-new-branch" && (
        <>
          {/* Branch selector — varies by mode */}
          <Input
            className="mb-1.5 w-full rounded border border-border bg-background px-2 py-1 text-xs focus-visible:outline-none focus:border-primary"
            placeholder="New branch name"
            value={addBranch}
            onChange={(e) => setAddBranch(e.target.value)}
          />

          {/* Branch selector — varies by mode */}
          <Combobox
            options={commitOptions}
            value={addStartPoint}
            emptyMessage="No commits found"
            loading={commitsLoading || refsLoading}
            placeholder="Start point (default: HEAD)…"
            searchPlaceholder="Search commits, branches, tags…"
            onValueChange={setAddStartPoint}
            open={showStartPointPicker}
            onOpenChange={setShowStartPointPicker}
          />
        </>
      )}

      {dialogMode === "add-detached" && (
        <Combobox
          options={commitOptions}
          value={addBranch}
          emptyMessage="No commits or tags found"
          loading={commitsLoading || refsLoading}
          placeholder="Select commit or tag…"
          searchPlaceholder="Search commits, branches, tags…"
          onValueChange={setAddBranch}
          open={showDetachedPicker}
          onOpenChange={setShowDetachedPicker}
        />
      )}

      {/* New branch name — still a text input since user types a new name */}
      <fieldset className="mb-1.5">
        <legend className="mb-1 text-[10px] font-medium text-muted-foreground">
          Pointer file hydration
        </legend>
        <RadioGroup
          value={addHydration}
          onValueChange={(next) => setAddHydration(next as typeof addHydration)}
          className="flex gap-1"
        >
          <label
            className={`flex-1 cursor-pointer rounded px-2 py-1 text-center text-[10px] transition ${
              addHydration === "pointers_only"
                ? "bg-primary/20 text-foreground font-medium"
                : "bg-card text-muted-foreground hover:text-foreground"
            }`}
          >
            <RadioGroupItem value="pointers_only" className="sr-only" />
            Keep as pointers
          </label>
          <label
            className={`flex-1 cursor-pointer rounded px-2 py-1 text-center text-[10px] transition ${
              addHydration === "on_access"
                ? "bg-primary/20 text-foreground font-medium"
                : "bg-card text-muted-foreground hover:text-foreground"
            }`}
          >
            <RadioGroupItem value="on_access" className="sr-only" />
            On access
          </label>
          <label
            className={`flex-1 cursor-pointer rounded px-2 py-1 text-center text-[10px] transition ${
              addHydration === "all"
                ? "bg-primary/20 text-foreground font-medium"
                : "bg-card text-muted-foreground hover:text-foreground"
            }`}
          >
            <RadioGroupItem value="all" className="sr-only" />
            Hydrate all
          </label>
        </RadioGroup>
        <p className="mt-1 text-[9px] leading-tight text-muted-foreground/70">
          {addHydration === "pointers_only" &&
            "Files stay as lightweight pointers. Fastest checkout, no extra downloads."}
          {addHydration === "on_access" &&
            "Files are hydrated lazily when you open or read them."}
          {addHydration === "all" &&
            "All pointer files are downloaded immediately. Uses more disk and bandwidth."}
        </p>
      </fieldset>

      <Button variant="default"
        type="button"
        className="mt-1 w-full justify-center text-xs"
        onClick={onSubmit}
        disabled={busy || !addPath.trim()}
      >
        {busy ? (
          <Loader2 size={12} className="animate-spin" />
        ) : (
          <Plus size={12} />
        )}
        Create Worktree
      </Button>
    </div>
  );
}

/** A single worktree card with status chips and actions. */
function WorktreeCard({
  wt,
  status,
  estimate,
  isActive,
  isSelected,
  busy,
  shortBranch,
  dirName,
  onSelect,
  onOpen,
  onPush,
  onPull,
  onLock,
  onUnlock,
  onRemove,
  onContextMenu,
  renaming,
  onRenameChange,
  onRenameSubmit,
  onRenameCancel,
  colourPicking,
  onColourSelect,
  onColourCancel,
  moving,
  onMoveChange,
  onMoveSubmit,
  onMoveCancel,
}: {
  wt: HydratedWorktreeEntry;
  status: WorktreeStatus | undefined;
  estimate: PushEstimate | undefined;
  isActive: boolean;
  isSelected: boolean;
  busy: boolean;
  shortBranch: (b: string) => string;
  dirName: (p: string) => string;
  onSelect: (wt: HydratedWorktreeEntry) => void;
  onOpen: (path: string) => void;
  onPush: (path: string, branch: string) => void;
  onPull: (path: string, branch: string) => void;
  onLock: (path: string) => void;
  onUnlock: (path: string) => void;
  onRemove: (path: string) => void;
  onContextMenu: (e: React.MouseEvent, wt: HydratedWorktreeEntry) => void;
  renaming: { path: string; value: string } | null;
  onRenameChange: (value: string) => void;
  onRenameSubmit: () => void;
  onRenameCancel: () => void;
  colourPicking: boolean;
  onColourSelect: (colour: string | null) => void;
  onColourCancel: () => void;
  moving: { path: string; value: string } | null;
  onMoveChange: (value: string) => void;
  onMoveSubmit: () => void;
  onMoveCancel: () => void;
}) {
  const displayName = wt.meta?.display_name || dirName(wt.path);
  const colourAccent = wt.meta?.colour || undefined;

  return (
    <div
      className={`group/card relative cursor-pointer rounded-lg border p-3 transition ${
        isSelected
          ? "border-primary bg-primary/8 ring-1 ring-primary/40"
          : isActive
            ? "border-primary/50 bg-primary/5"
            : "border-border bg-card hover:border-muted-foreground/60"
      }`}
      style={colourAccent ? { borderLeftColor: colourAccent, borderLeftWidth: "3px" } : undefined}
      onClick={() => onSelect(wt)}
      onContextMenu={(e) => onContextMenu(e, wt)}
      role="button"
      tabIndex={0}
      onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") onSelect(wt); }}
      aria-label={`View worktree ${displayName}`}
    >
      {/* Top row: display name + badges */}
      <div className="flex items-center gap-1.5">
        <FolderOpen size={13} className="shrink-0 text-muted-foreground" />
        <span className="truncate text-[12.5px] font-medium text-foreground" title={wt.path}>
          {displayName}
        </span>
        {wt.is_main && (
          <span className="shrink-0 rounded bg-primary/20 px-1.5 py-0.5 text-[9px] font-semibold text-primary">
            MAIN
          </span>
        )}
        {isActive && (
          <span className="shrink-0 rounded bg-success/20 px-1.5 py-0.5 text-[9px] font-semibold text-success">
            ACTIVE
          </span>
        )}
        {wt.locked && (
          <span title={wt.lock_reason ? `Locked: ${wt.lock_reason}` : "Locked"}>
            <Lock size={10} className="shrink-0 text-warning" />
          </span>
        )}
        {wt.prunable && (
          <span className="shrink-0 rounded bg-destructive/20 px-1.5 py-0.5 text-[9px] font-semibold text-destructive">
            STALE
          </span>
        )}
        {wt.meta?.pinned && (
          <span title="Pinned">
            <Pin size={10} className="shrink-0 text-primary" />
          </span>
        )}

        {/* Top row: display name + badges */}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="ml-auto shrink-0 rounded bg-card px-2 py-0.5 text-[10px] font-medium text-muted-foreground hover:bg-primary/15 hover:text-primary transition"
          onClick={(e) => { e.stopPropagation(); onOpen(wt.path); }}
          disabled={busy}
          title="Open worktree as full workspace"
          aria-label="Open worktree as full workspace"
        >
          <ExternalLink size={10} className="inline -mt-px mr-0.5" />
          Open
        </Button>

        {/* Top row: display name + badges */}
        {!wt.is_main && (
          <span className="flex shrink-0 items-center gap-0.5 opacity-0 transition group-hover/card:opacity-100" onClick={(e) => e.stopPropagation()}>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-success"
              onClick={() => onPush(wt.path, shortBranch(wt.branch))}
              disabled={busy}
              title="Push"
              aria-label="Push worktree branch"
            >
              <ArrowUp size={12} />
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-info"
              onClick={() => onPull(wt.path, shortBranch(wt.branch))}
              disabled={busy}
              title="Pull"
              aria-label="Pull worktree branch"
            >
              <ArrowDown size={12} />
            </Button>
            {wt.locked ? (
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-warning"
                onClick={() => onUnlock(wt.path)}
                disabled={busy}
                title="Unlock worktree"
                aria-label="Unlock worktree"
              >
                <LockOpen size={12} />
              </Button>
            ) : (
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-warning"
                onClick={() => onLock(wt.path)}
                disabled={busy}
                title="Lock worktree"
                aria-label="Lock worktree"
              >
                <Lock size={12} />
              </Button>
            )}
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-destructive"
              onClick={() => onRemove(wt.path)}
              disabled={busy}
              title="Remove worktree"
              aria-label="Remove worktree"
            >
              <Trash2 size={12} />
            </Button>
          </span>
        )}

        {/* Hover action icons: Push, Pull, Lock/Unlock, Remove */}
        {wt.is_main && (
          <span className="flex shrink-0 items-center gap-0.5 opacity-0 transition group-hover/card:opacity-100" onClick={(e) => e.stopPropagation()}>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-success"
              onClick={() => onPush(wt.path, shortBranch(wt.branch))}
              disabled={busy}
              title="Push"
              aria-label="Push worktree branch"
            >
              <ArrowUp size={12} />
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-info"
              onClick={() => onPull(wt.path, shortBranch(wt.branch))}
              disabled={busy}
              title="Pull"
              aria-label="Pull worktree branch"
            >
              <ArrowDown size={12} />
            </Button>
          </span>
        )}
      </div>

      {/* For the main worktree, still show Push/Pull on hover */}
      <div className="mt-1.5 flex items-center gap-1.5 text-[11px] text-muted-foreground">
        <span className="truncate font-mono">{shortBranch(wt.branch)}</span>
        {wt.head && (
          <span className="font-mono opacity-60">({wt.head.slice(0, 7)})</span>
        )}
      </div>

      {/* For the main worktree, still show Push/Pull on hover */}
      {(status || (estimate && estimate.total_bytes > 0)) && (
        <div className="mt-2 flex flex-wrap items-center gap-1.5">
          {/* For the main worktree, still show Push/Pull on hover */}
          {status && status.ahead !== null && status.ahead > 0 && (
            <span className="inline-flex items-center rounded bg-success/15 px-1.5 py-0.5 text-[10px] font-medium text-success">
              ↑{status.ahead}
            </span>
          )}
          {status && status.behind !== null && status.behind > 0 && (
            <span className="inline-flex items-center rounded bg-info/15 px-1.5 py-0.5 text-[10px] font-medium text-info">
              ↓{status.behind}
            </span>
          )}
          {/* Branch row */}
          {status && status.dirty_count > 0 && (
            <span className="inline-flex items-center rounded bg-warning/15 px-1.5 py-0.5 text-[10px] font-medium text-warning">
              {status.dirty_count} change{status.dirty_count !== 1 ? "s" : ""}
            </span>
          )}
          {/* Branch row */}
          {estimate && estimate.total_bytes > 0 && (
            <span
              className="inline-flex items-center gap-0.5 rounded bg-muted-foreground/10 px-1.5 py-0.5 text-[10px] font-medium text-muted-foreground"
              title={`${formatBytes(estimate.total_bytes)} estimated disk usage${estimate.pointer_count > 0 ? ` (${estimate.pointer_count} pointer${estimate.pointer_count !== 1 ? "s" : ""})` : ""}`}
            >
              <HardDrive size={9} />
              {formatBytes(estimate.total_bytes)}
              {estimate.pointer_count > 0 && (
                <span className="opacity-70">
                  ({estimate.pointer_count}{" "}
                  {estimate.pointer_count === 1 ? "ptr" : "ptrs"})
                </span>
              )}
            </span>
          )}
        </div>
      )}

      {/* Lock reason */}
      {wt.lock_reason && (
        <div className="mt-1.5 text-[10px] italic text-warning/80">
          Lock reason: {wt.lock_reason}
        </div>
      )}

      {/* Full path (subtle) */}
      <div
        className="mt-1.5 truncate font-mono text-[10px] text-muted-foreground/60"
        title={wt.path}
      >
        {wt.path}
      </div>

      {/* Inline rename input */}
      {renaming && (
        <div className="mt-2 flex items-center gap-1" onClick={(e) => e.stopPropagation()}>
          <Input
            className="flex-1 rounded border border-border bg-background px-2 py-1 text-xs focus-visible:outline-none focus:border-primary"
            placeholder="Display name (empty to clear)"
            value={renaming.value}
            onChange={(e) => onRenameChange(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") onRenameSubmit();
              if (e.key === "Escape") onRenameCancel();
            }}
            autoFocus
          />
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded bg-primary/15 px-2 py-1 text-[10px] font-medium text-primary hover:bg-primary/25"
            onClick={onRenameSubmit}
            disabled={busy}
          >
            Save
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded px-1.5 py-1 text-muted-foreground hover:text-foreground"
            onClick={onRenameCancel}
          >
            <X size={12} />
          </Button>
        </div>
      )}

      {/* Inline colour picker */}
      {colourPicking && (
        <div className="mt-2" onClick={(e) => e.stopPropagation()}>
          <div className="mb-1 text-[10px] text-muted-foreground">
            Pick a colour:
          </div>
          <div className="flex items-center gap-1.5">
            {COLOUR_PRESETS.map((preset) => (
              <Button
                variant="ghost"
                size="compact"
                key={preset.label}
                type="button"
                className={`h-5 w-5 rounded-full border transition hover:scale-110 ${
                  preset.value === null
                    ? "border-border bg-card flex items-center justify-center"
                    : "border-transparent"
                }`}
                style={preset.value ? { backgroundColor: preset.value } : undefined}
                title={preset.label}
                onClick={() => onColourSelect(preset.value)}
              >
                {preset.value === null && (
                  <X size={10} className="text-muted-foreground" />
                )}
              </Button>
            ))}
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="ml-1 rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:text-foreground"
              onClick={onColourCancel}
            >
              Cancel
            </Button>
          </div>
        </div>
      )}

      {/* Inline move input */}
      {moving && (
        <div className="mt-2 flex items-center gap-1" onClick={(e) => e.stopPropagation()}>
          <Input
            className="flex-1 rounded border border-border bg-background px-2 py-1 text-xs focus-visible:outline-none focus:border-primary"
            placeholder="New path (e.g. ../new-location)"
            value={moving.value}
            onChange={(e) => onMoveChange(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") onMoveSubmit();
              if (e.key === "Escape") onMoveCancel();
            }}
            autoFocus
          />
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded bg-primary/15 px-2 py-1 text-[10px] font-medium text-primary hover:bg-primary/25"
            onClick={onMoveSubmit}
            disabled={busy || !moving.value.trim()}
          >
            Move
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded px-1.5 py-1 text-muted-foreground hover:text-foreground"
            onClick={onMoveCancel}
          >
            <X size={12} />
          </Button>
        </div>
      )}
    </div>
  );
}

/**
 * Picker overlay shown when the user selects "Compare with…" from the
 * context menu. Lists all other worktrees as selectable targets.
 */
function WorktreeComparePicker({
  source,
  worktrees,
  shortBranch,
  dirName,
  onSelect,
  onDismiss,
}: {
  source: HydratedWorktreeEntry;
  worktrees: HydratedWorktreeEntry[];
  shortBranch: (b: string) => string;
  dirName: (p: string) => string;
  onSelect: (target: HydratedWorktreeEntry) => void;
  onDismiss: () => void;
}) {
  const targets = worktrees.filter((wt) => wt.path !== source.path);
  const sourceLabel = source.meta?.display_name || dirName(source.path);

  useEffect(() => {
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onDismiss();
    };
    window.addEventListener("keydown", handleKey);
    return () => window.removeEventListener("keydown", handleKey);
  }, [onDismiss]);

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onDismiss();
      }}
    >
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        className="block gap-0 p-0 w-[320px] rounded-lg border border-border bg-card p-4 shadow-xl"
      >
        <DialogTitle className="sr-only">Compare worktrees</DialogTitle>
        <div className="mb-3 flex items-center gap-2">
          <GitCompareArrows size={14} className="text-primary" />
          <span className="text-[12px] font-medium text-foreground">
            Compare "{sourceLabel}" with…
          </span>
        </div>

        {targets.length === 0 ? (
          <div className="py-4 text-center text-[11px] text-muted-foreground">
            No other worktrees to compare with.
          </div>
        ) : (
          <div className="max-h-[240px] overflow-auto">
            {targets.map((wt) => {
              const label = wt.meta?.display_name || dirName(wt.path);
              return (
                <Button
                  variant="ghost"
                  size="compact"
                  key={wt.path}
                  type="button"
                  className="justify-start flex w-full items-center gap-2 rounded px-3 py-2 text-left transition hover:bg-primary/10"
                  onClick={() => onSelect(wt)}
                >
                  <FolderOpen size={12} className="shrink-0 text-muted-foreground" />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-[12px] font-medium text-foreground">
                      {label}
                    </div>
                    <div className="truncate font-mono text-[10px] text-muted-foreground">
                      {shortBranch(wt.branch)}
                    </div>
                  </div>
                </Button>
              );
            })}
          </div>
        )}

        <div className="mt-3 flex justify-end">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded px-3 py-1 text-[11px] text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={onDismiss}
          >
            Cancel
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
