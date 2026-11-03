import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSessionScroll } from "../hooks/useSessionScroll";
import clsx from "clsx";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  FileDiff,
  FileText,
  FolderTree,
  GitPullRequestArrow,
  Layers,
  List,
  Loader2,
  Minus,
  Plus,
  RefreshCw,
  Undo2,
} from "lucide-react";
import { Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import {
  FileTree as PierreFileTree,
  useFileTree,
  useFileTreeSelection,
} from "@pierre/trees/react";
import type { GitStatus, GitStatusEntry } from "@pierre/trees";
import { themeToTreeStyles } from "@pierre/trees";
import { OpenRepo } from "../state";
import { HunkAnchor, isStaleHunkError, rpc } from "../ipc";
import { useEffectiveTheme } from "../hooks/useEffectiveTheme";
import { InlinePatchDiff } from "../components/InlinePatchDiff";
import { TriStateValue } from "../components/TriStateCheckbox";
import { ConfirmDiscardDialog } from "../components/ConfirmDiscardDialog";
import { ViewToolbar, ToolbarAction } from "../components/ViewToolbar";
import { ContextMenu, MenuItemDef } from "../components/ContextMenu";
import { executeCommand, CommandContext } from "../lib/command-registry";
import { CommitComposer } from "./CommitComposer";
import { hunkToAnchor, lineToDiscardAnchor, ParsedHunk, parsePatch } from "../lib/parse-patch";
import { deriveFileStatesFromPatches } from "../lib/file-state";
import { countDiscardedLines, shouldConfirmDiscard } from "../lib/discard-threshold";
import { useFileWatcher, getWatchPath } from "../hooks/useFileWatcher";
import { useSettings } from "../settings";
import { BatchActionBar } from "../components/BatchActionBar";
import { StagingZones } from "../components/StagingZones";
import { CommitReadiness } from "../components/CommitReadiness";

interface Props {
  repo: OpenRepo;
  /** Kept for backwards compatibility with the existing
   *  `RepoDetail` consumer: when non-null, clicking a file also
   *  opens its working-tree-vs-HEAD diff in the editor-area tab
   *  strip (the old behaviour). New in task 1.2: selection *also*
   *  populates this view's own right pane with a staging-oriented
   *  worktree-vs-index patch. Callers who only want the split view
   *  can pass a no-op. */
  onOpenDiff?: (path: string) => void;
  /** Forwarded to the tree/list so the currently-open editor diff
   *  is highlighted if the caller is also driving the tab strip. */
  activeDiffPath?: string | null;
  /** Fired after a successful commit so the parent can refresh sibling views (e.g. Timeline). */
  onCommit?: (sha: string) => void;
}

interface StatusEntry {
  path: string;
  code: string;
}

// Porcelain code → human label. We show the user-friendly label
// and colour; code stays available as a tooltip.
const CODE_STYLE: Record<string, { label: string; cls: string }> = {
  M: { label: "modified", cls: "text-gx-warn" },
  A: { label: "added", cls: "text-gx-success" },
  D: { label: "deleted", cls: "text-gx-error" },
  R: { label: "renamed", cls: "text-gx-accent" },
  C: { label: "copied", cls: "text-gx-accent" },
  "??": { label: "untracked", cls: "text-gx-muted" },
};

function labelFor(code: string): { label: string; cls: string } {
  const trimmed = code.trim();
  return CODE_STYLE[trimmed[0]] ?? CODE_STYLE[trimmed] ?? { label: trimmed, cls: "text-gx-muted" };
}

function codeBadge(code: string): string {
  const t = code.trim();
  if (t === "??") return "U";
  if (!t) return "·";
  // Show whichever letter the user is most likely to care about.
  // Status codes are "XY" (index / worktree). The worktree side is
  // more actionable for a developer scanning changes, so prefer Y
  // when present; fall back to X.
  return (t[1] && t[1] !== " " ? t[1] : t[0]) ?? "·";
}

/** Non-interactive tri-state glyph for the tree-view decoration.
 *  Matches the list-view checkbox visuals at a text level:
 *  `☐` empty, `▣` partial, `✓` full. Kept ASCII-safe fallback
 *  would muddle the meaning — these codepoints all render cleanly
 *  in the monospaced font the tree uses. */
function triStateGlyph(state: TriStateValue): string {
  switch (state) {
    case "full":
      return "✓";
    case "partial":
      return "▣";
    default:
      return "☐";
  }
}

function toTreesStatus(code: string): GitStatus {
  const t = code.trim();
  if (t === "??") return "untracked";
  switch (t[0]) {
    case "A":
      return "added";
    case "M":
      return "modified";
    case "D":
      return "deleted";
    case "R":
      return "renamed";
    case "!":
      return "ignored";
    default:
      return "modified";
  }
}

/** Status code → "this file has something staged". Used to enable
 *  the Unstage toolbar button. The `X` position of the two-letter
 *  porcelain code is the index side; any non-space/non-`?` means
 *  there's a staged change. */
function hasStagedChanges(code: string): boolean {
  if (code === "??") return false;
  const x = code[0];
  return x != null && x !== " " && x !== "?";
}

const LS_CHANGES_MODE = "crab.changes.mode";
const LS_CHANGES_SPLIT = "crab.changes.splitPct";
type Mode = "list" | "tree" | "zones";

function loadMode(): Mode {
  const stored = localStorage.getItem(LS_CHANGES_MODE);
  if (stored === "tree" || stored === "zones") return stored;
  return "zones"; // Default to zones for the best UX
}

function loadSplit(): number {
  const raw = Number(localStorage.getItem(LS_CHANGES_SPLIT));
  return raw > 10 && raw < 80 ? raw : 32;
}

export function ChangesView({ repo, onOpenDiff, activeDiffPath, onCommit }: Props) {
  const [entries, setEntries] = useState<StatusEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [mode, setMode] = useState<Mode>(loadMode);
  const { settings } = useSettings();

  // Right-pane state.
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [patch, setPatch] = useState<string | null>(null);
  const [patchLoading, setPatchLoading] = useState(false);
  const [patchError, setPatchError] = useState<string | null>(null);
  const [toast, setToast] = useState<{
    kind: "info" | "error";
    text: string;
  } | null>(null);

  // "Open File" mode: when set, the right pane shows the full file
  // content instead of the diff view.
  const [openFilePath, setOpenFilePath] = useState<string | null>(null);
  const [openFileContent, setOpenFileContent] = useState<string | null>(null);
  const [openFileLoading, setOpenFileLoading] = useState(false);

  // Whole-repo patches used to derive the per-file tri-state
  // checkbox. One round-trip per side keeps the cost O(1) regardless
  // of how many files are in the change set (the per-file
  // `fetchPatch` above still powers the right-pane view). Pending
  // toggles live in `togglingPaths` so we can disable the checkbox
  // while the op is in flight.
  const [workTreePatch, setWorkTreePatch] = useState<string>("");
  const [cachedPatch, setCachedPatch] = useState<string>("");
  const [togglingPaths, setTogglingPaths] = useState<Set<string>>(() => new Set());

  // Multi-select state for batch operations.
  // Cmd/Ctrl+click toggles individual files; Shift+click selects a range.
  const [multiSelected, setMultiSelected] = useState<Set<string>>(() => new Set());
  const [batchBusy, setBatchBusy] = useState(false);
  const lastClickedRef = useRef<string | null>(null);

  // Large-discard gate. When a discard op — per-hunk, per-line, or
  // whole pointer file — touches more than the user's configured
  // threshold (Settings → Diff → "Confirm before large discard"),
  // we stash the pending anchors + summary here and render the
  // modal instead of firing the RPC immediately. Cancel clears;
  // Confirm fires + clears.
  const [pendingDiscard, setPendingDiscard] = useState<{
    anchors: HunkAnchor[];
    files: string[];
    lineCount: number;
    successToast: string;
  } | null>(null);

  useEffect(() => {
    localStorage.setItem(LS_CHANGES_MODE, mode);
  }, [mode]);

  // Track whether we've completed the initial load. Subsequent
  // refreshes update entries in-place without flashing a spinner.
  const hasLoadedRef = useRef(false);

  const refreshStatus = useCallback(() => {
    if (!hasLoadedRef.current) setLoading(true);
    setError(null);
    rpc
      .gitStatus(repo.repo_id)
      .then((r) => {
        setEntries(r.entries);
        hasLoadedRef.current = true;
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [repo.repo_id]);

  useEffect(refreshStatus, [refreshStatus]);

  // File watcher: auto-refresh status when files change on disk.
  // The watcher lifecycle is managed by RepoDetail via
  // useFileWatcherRoot; this hook only subscribes to events.
  useFileWatcher(getWatchPath(repo), refreshStatus);

  // Fetch both sides of the staging split (worktree-vs-index and
  // index-vs-HEAD) in parallel. Used for tri-state derivation —
  // pathless calls so one round-trip pair covers every file.
  const refreshPatchPair = useCallback(async () => {
    try {
      const [wt, cached] = await Promise.all([
        rpc.gitDiffIndexPatch(repo.repo_id, {}),
        rpc.gitDiffCachedPatch(repo.repo_id, {}),
      ]);
      setWorkTreePatch(wt.patch ?? "");
      setCachedPatch(cached.patch ?? "");
    } catch {
      // Partial failure means the checkboxes render `empty` for
      // everything — not great, but the rest of the view stays
      // usable. A follow-up toast on the specific action that
      // triggered this will surface the real error.
      setWorkTreePatch("");
      setCachedPatch("");
    }
  }, [repo.repo_id]);

  // Re-derive the patch pair whenever the status list changes.
  // Skip if a staging operation already refreshed the pair explicitly
  // (avoids redundant RPC calls and visual flicker).
  const skipPatchPairEffectRef = useRef(false);
  useEffect(() => {
    if (skipPatchPairEffectRef.current) {
      skipPatchPairEffectRef.current = false;
      return;
    }
    void refreshPatchPair();
  }, [refreshPatchPair, entries]);

  const statusByPath = useMemo(() => {
    const m = new Map<string, string>();
    for (const e of entries ?? []) m.set(e.path, e.code);
    return m;
  }, [entries]);

  // Per-file tri-state derived from the batched diff pair. The
  // derivation function also unions in any status-only paths
  // (untracked files don't show up in `git diff`) so the
  // checkboxes cover every row in the list.
  const fileStates = useMemo<Record<string, TriStateValue>>(
    () =>
      deriveFileStatesFromPatches(
        workTreePatch,
        cachedPatch,
        (entries ?? []).map((e) => e.path),
      ),
    [workTreePatch, cachedPatch, entries],
  );

  // Derive whether anything is staged — used by the commit composer
  // to enable/disable the Commit button.
  const hasStaged = useMemo<boolean>(() => {
    for (const state of Object.values(fileStates)) {
      if (state === "partial" || state === "full") return true;
    }
    return false;
  }, [fileStates]);

  // Paths of staged files — passed to the commit composer for
  // notebook detection.
  const stagedPaths = useMemo<string[]>(() => {
    const paths: string[] = [];
    for (const [path, state] of Object.entries(fileStates)) {
      if (state === "partial" || state === "full") paths.push(path);
    }
    return paths;
  }, [fileStates]);

  // --- ViewToolbar + ContextMenu state ---

  // Context menu state for right-click on file items.
  const [contextMenu, setContextMenu] = useState<{
    position: { x: number; y: number };
    targetPath: string;
  } | null>(null);

  // Build a CommandContext from the view's available state.
  const dirtyCount = entries?.length ?? 0;
  const commandContext = useMemo<CommandContext>(
    () => ({
      repoId: repo.repo_id,
      currentBranch: "",
      upstream: null,
      ahead: 0,
      behind: 0,
      dirtyCount,
      hasStagedChanges: hasStaged,
      activeOperation: null,
      forgeProvider: repo.forge_provider ?? null,
    }),
    [repo.repo_id, repo.forge_provider, dirtyCount, hasStaged],
  );

  // Toolbar actions with dynamic disabled states.
  const toolbarActions = useMemo<ToolbarAction[]>(
    () => [
      { id: "git.stage.all", icon: Plus, label: "Stage All", disabled: dirtyCount === 0 },
      { id: "git.unstage.all", icon: Minus, label: "Unstage All", disabled: !hasStaged },
      { id: "git.discard.all", icon: Undo2, label: "Discard All", disabled: dirtyCount === 0 },
      { id: "git.push", icon: ArrowUp, label: "Push" },
      { id: "git.pull", icon: ArrowDown, label: "Pull" },
      { id: "git.fetch", icon: RefreshCw, label: "Fetch" },
    ],
    [dirtyCount, hasStaged],
  );

  // Handle toolbar action clicks — route through CommandRegistry.
  // Refresh status + patches after any staging-related command so the
  // UI reflects the new git state immediately.
  const handleToolbarAction = useCallback(
    async (actionId: string) => {
      const result = await executeCommand(actionId, commandContext);
      if (result.success) {
        skipPatchPairEffectRef.current = true;
        refreshStatus();
        await refreshPatchPair();
      }
    },
    [commandContext, refreshStatus, refreshPatchPair],
  );

  // Context menu items for file right-click.
  const fileContextMenuItems = useMemo<MenuItemDef[]>(
    () => [
      { id: "git.stage.file", label: "Stage", icon: Plus },
      { id: "git.unstage.file", label: "Unstage", icon: Minus },
      { id: "git.discard.file", label: "Discard Changes", icon: Undo2, danger: true },
      { id: "open.diff", label: "Open Diff", icon: FileDiff, separator: true },
      { id: "open.file", label: "Open File", icon: FileText },
    ],
    [],
  );

  const handleFileContextMenu = useCallback(
    (e: React.MouseEvent, path: string) => {
      e.preventDefault();
      setContextMenu({ position: { x: e.clientX, y: e.clientY }, targetPath: path });
    },
    [],
  );

  // Per-file discard handler for the inline action buttons.
  // Routes through the command registry so undo/toast/history all work.
  const handleDiscardFileByPath = useCallback(
    async (path: string) => {
      const result = await executeCommand("git.discard.file", commandContext, path);
      if (result.success) {
        skipPatchPairEffectRef.current = true;
        refreshStatus();
        await refreshPatchPair();
      }
    },
    [commandContext, refreshStatus, refreshPatchPair],
  );

  // Open File handler: reads the full file content and shows it in
  // the right pane, replacing the diff view. Uses rpc.readText for
  // both local and remote repos (routes through RpcRouter).
  const handleOpenFile = useCallback(
    async (path: string) => {
      setOpenFilePath(path);
      setOpenFileLoading(true);
      setOpenFileContent(null);
      try {
        const result = await rpc.readText(repo.repo_id, path);
        setOpenFileContent(result.text);
      } catch {
        setOpenFileContent(null);
      } finally {
        setOpenFileLoading(false);
      }
    },
    [repo.repo_id],
  );

  // Fetch the appropriate patch for the currently selected file.
  // For fully-staged files (state === "full"), use `git diff --cached`
  // (index vs HEAD) so the user sees what will be committed. For
  // working/partial files, use worktree-vs-index.
  const fetchPatch = useCallback(
    async (path: string) => {
      setPatchLoading(true);
      setPatchError(null);
      try {
        const code = statusByPath.get(path) ?? "";
        const state = fileStates[path] ?? "empty";

        let r: { patch: string };
        if (state === "full") {
          // Staged file — show index vs HEAD (git diff --cached)
          r = await rpc.gitDiffCachedPatch(repo.repo_id, { path });
        } else {
          // Working/partial — show worktree vs index
          // When the file is untracked, `git diff` ignores it —
          // include the worktree-side extras so the user still sees
          // the new content. The agent honours `include_untracked`.
          r = await rpc.gitDiffIndexPatch(repo.repo_id, {
            path,
            include_untracked: code === "??",
          });
        }
        setPatch(r.patch);
      } catch (e) {
        setPatchError(e instanceof Error ? e.message : String(e));
        setPatch(null);
      } finally {
        setPatchLoading(false);
      }
    },
    [repo.repo_id, statusByPath, fileStates],
  );

  useEffect(() => {
    if (!selectedPath) {
      setPatch(null);
      setPatchError(null);
      return;
    }
    void fetchPatch(selectedPath);
  }, [selectedPath, fetchPatch]);

  // Reset selection + patch when the repo changes so we don't show
  // stale content.
  useEffect(() => {
    setSelectedPath(null);
    setPatch(null);
    setWorkTreePatch("");
    setCachedPatch("");
  }, [repo.repo_id]);

  const selectFile = useCallback(
    (path: string) => {
      setSelectedPath(path);
      // Clear "open file" mode when switching to diff view
      setOpenFilePath(null);
      setOpenFileContent(null);
      // For new untracked files, automatically show full file content
      // since there's no meaningful diff to display.
      const code = statusByPath.get(path) ?? "";
      if (code === "??") {
        void (async () => {
          setOpenFilePath(path);
          setOpenFileLoading(true);
          try {
            const result = await rpc.readText(repo.repo_id, path);
            setOpenFileContent(result.text);
          } catch {
            // Fall through — will show empty state
          } finally {
            setOpenFileLoading(false);
          }
        })();
      }
    },
    [statusByPath, repo.repo_id],
  );

  // Double-click opens the full diff in the editor-area tab strip
  // (VS Code SCM behaviour: single-click previews inline,
  // double-click opens the editor diff).
  const openFileDiff = useCallback(
    (path: string) => {
      onOpenDiff?.(path);
    },
    [onOpenDiff],
  );

  // Handle context menu item selection — route through CommandRegistry.
  // Await the result and refresh status + patches so the file list
  // updates after stage/unstage/discard.
  const handleContextMenuSelect = useCallback(
    async (actionId: string) => {
      if (!contextMenu) return;
      const targetPath = contextMenu.targetPath;
      setContextMenu(null);

      // Navigation commands are signal-only in the registry; handle
      // the actual view transition here.
      if (actionId === "open.diff") {
        selectFile(targetPath);
        onOpenDiff?.(targetPath);
        return;
      }
      if (actionId === "open.file") {
        handleOpenFile(targetPath);
        return;
      }

      const result = await executeCommand(actionId, commandContext, targetPath);
      if (result.success) {
        skipPatchPairEffectRef.current = true;
        refreshStatus();
        await refreshPatchPair();
      }
    },
    [commandContext, contextMenu, refreshStatus, refreshPatchPair, selectFile, onOpenDiff, handleOpenFile],
  );

  // Wrap a staging RPC: on success refresh the patch + status. On
  // StaleHunk error, auto-refetch and tell the user. Everything
  // else surfaces as a toast with the raw message.
  const runStagingOp = useCallback(
    async (op: () => Promise<unknown>, successToast?: string) => {
      try {
        await op();
        if (successToast) setToast({ kind: "info", text: successToast });
        if (selectedPath) await fetchPatch(selectedPath);
        // Suppress the entries-triggered refreshPatchPair since we
        // call it explicitly here — avoids a redundant RPC round-trip.
        skipPatchPairEffectRef.current = true;
        refreshStatus();
        await refreshPatchPair();
      } catch (e) {
        if (isStaleHunkError(e)) {
          setToast({
            kind: "info",
            text: "File changed on disk — refreshing diff.",
          });
          if (selectedPath) await fetchPatch(selectedPath);
          skipPatchPairEffectRef.current = true;
          refreshStatus();
          await refreshPatchPair();
          return;
        }
        setToast({
          kind: "error",
          text: e instanceof Error ? e.message : String(e),
        });
      }
    },
    [selectedPath, fetchPatch, refreshStatus, refreshPatchPair],
  );

  // Toast auto-dismiss (4 s) — simple enough that dragging in a
  // proper ToastManager (Pillar E.4) is out of scope here.
  useEffect(() => {
    if (!toast) return;
    const h = setTimeout(() => setToast(null), 4000);
    return () => clearTimeout(h);
  }, [toast]);

  /**
   * Route a discard request through the threshold gate.
   *
   * Small discards (≤ threshold or when the threshold is 0) go
   * straight to `git_discard_hunks` via `runStagingOp`. Large
   * discards stash the anchors in `pendingDiscard`, which renders
   * the `<ConfirmDiscardDialog />` modal; the user's Confirm
   * click then fires the same RPC.
   *
   * Centralising this here — rather than duplicating the check at
   * every call site — keeps hunk / line / whole-file discard
   * paths in sync when the threshold semantics change.
   */
  const requestDiscard = useCallback(
    (anchors: HunkAnchor[], successToast: string) => {
      if (anchors.length === 0) return;
      const lineCount = countDiscardedLines(anchors);
      if (!shouldConfirmDiscard(lineCount, settings.discardConfirmThreshold)) {
        void runStagingOp(
          () => rpc.gitDiscardHunks(repo.repo_id, anchors),
          successToast,
        );
        return;
      }
      // Distinct, stable-order file list for the modal's summary.
      const files: string[] = [];
      const seen = new Set<string>();
      for (const a of anchors) {
        if (!seen.has(a.path)) {
          seen.add(a.path);
          files.push(a.path);
        }
      }
      setPendingDiscard({ anchors, files, lineCount, successToast });
    },
    [repo.repo_id, runStagingOp, settings.discardConfirmThreshold],
  );

  const confirmPendingDiscard = useCallback(() => {
    const pending = pendingDiscard;
    if (!pending) return;
    setPendingDiscard(null);
    void runStagingOp(
      () => rpc.gitDiscardHunks(repo.repo_id, pending.anchors),
      pending.successToast,
    );
  }, [pendingDiscard, repo.repo_id, runStagingOp]);

  const cancelPendingDiscard = useCallback(() => {
    setPendingDiscard(null);
  }, []);

  // Hunk operations use repo.repo_id for all RPC calls, making them
  // transport-agnostic: the RpcRouter dispatches ssh:// prefixed IDs
  // to the remote agent automatically.
  const onStageHunk = useCallback(
    ({ hunk }: { hunk: Parameters<Parameters<typeof InlinePatchDiff>[0]["onStageHunk"]>[0]["hunk"] }) => {
      const a: HunkAnchor = hunkToAnchor(hunk);
      void runStagingOp(
        () => rpc.gitStageHunks(repo.repo_id, [a]),
        "Staged hunk",
      );
    },
    [repo.repo_id, runStagingOp],
  );

  const onUnstageHunk = useCallback(
    ({ hunk }: { hunk: Parameters<Parameters<typeof InlinePatchDiff>[0]["onStageHunk"]>[0]["hunk"] }) => {
      const a: HunkAnchor = hunkToAnchor(hunk);
      void runStagingOp(
        () => rpc.gitUnstageHunks(repo.repo_id, [a]),
        "Unstaged hunk",
      );
    },
    [repo.repo_id, runStagingOp],
  );

  const onDiscardHunk = useCallback(
    ({ hunk }: { hunk: Parameters<Parameters<typeof InlinePatchDiff>[0]["onStageHunk"]>[0]["hunk"] }) => {
      const a: HunkAnchor = hunkToAnchor(hunk);
      requestDiscard([a], "Discarded hunk");
    },
    [requestDiscard],
  );

  const onDiscardLine = useCallback(
    ({
      hunk,
      side,
      absoluteLine,
    }: {
      hunk: Parameters<Parameters<typeof InlinePatchDiff>[0]["onStageHunk"]>[0]["hunk"];
      side: "add" | "del";
      absoluteLine: number;
    }) => {
      const anchor = lineToDiscardAnchor(hunk, side, absoluteLine);
      if (!anchor) {
        setToast({ kind: "error", text: "Line not found in hunk body." });
        return;
      }
      requestDiscard([anchor], "Discarded line");
    },
    [requestDiscard],
  );

  /**
   * Whole-file staging for pointer blobs. `<InlinePatchDiff />`
   * swaps its per-hunk toolbar for a `<PointerFileCard />` when it
   * detects a Crab / LFS pointer signature in the patch; the
   * card calls these callbacks once with every hunk for that file
   * so we can batch into a single `git_stage_hunks` /
   * `git_unstage_hunks` / `git_discard_hunks` RPC.
   *
   * For the stage direction: when the source patch has no hunks
   * (untracked pointer file), fall back to `rpc.add` which shells
   * `crab add` to stage the file whole-file. The unstage and
   * discard directions don't need that fallback — you can't
   * unstage something that isn't staged, and you can't discard a
   * file that only exists in the working tree (it's already in
   * the state the user wants).
   */
  const onStageFile = useCallback(
    ({ hunks }: { path: string; hunks: ParsedHunk[] }) => {
      const anchors: HunkAnchor[] = hunks.map(hunkToAnchor);
      if (anchors.length === 0) {
        // Untracked pointer file — fall back to whole-file add via
        // gitAddPaths which routes through the RPC router for both
        // local and remote repos.
        const path = hunks[0]?.path;
        if (path) {
          void runStagingOp(
            () => rpc.gitAddPaths(repo.repo_id, [path]),
            "Staged pointer",
          );
        }
        return;
      }
      void runStagingOp(
        () => rpc.gitStageHunks(repo.repo_id, anchors),
        "Staged pointer",
      );
    },
    [repo.repo_id, runStagingOp],
  );

  const onUnstageFile = useCallback(
    ({ hunks }: { path: string; hunks: ParsedHunk[] }) => {
      const anchors: HunkAnchor[] = hunks.map(hunkToAnchor);
      if (anchors.length === 0) return;
      void runStagingOp(
        () => rpc.gitUnstageHunks(repo.repo_id, anchors),
        "Unstaged pointer",
      );
    },
    [repo.repo_id, runStagingOp],
  );

  const onDiscardFile = useCallback(
    ({ hunks }: { path: string; hunks: ParsedHunk[] }) => {
      const anchors: HunkAnchor[] = hunks.map(hunkToAnchor);
      if (anchors.length === 0) return;
      requestDiscard(anchors, "Discarded pointer");
    },
    [requestDiscard],
  );

  /**
   * Toggle every hunk for one file from its current tri-state.
   *
   *   - `empty` / `partial` (intent `stage`): parse the worktree-
   *     vs-index patch, slice to this file, stage every hunk.
   *     For untracked files there's no patch — fall back to
   *     `rpc.add` which stages the file whole-file.
   *   - `full` (intent `unstage`): parse the cached patch, slice
   *     to this file, unstage every hunk.
   */
  const onToggleFile = useCallback(
    async (path: string, intent: "stage" | "unstage") => {
      if (togglingPaths.has(path)) return;
      setTogglingPaths((prev) => {
        const next = new Set(prev);
        next.add(path);
        return next;
      });
      try {
        const source = intent === "stage" ? workTreePatch : cachedPatch;
        const parsed = parsePatch(source);
        const hunks: HunkAnchor[] = parsed.hunks
          .filter((h) => h.path === path)
          .map(hunkToAnchor);

        if (hunks.length === 0) {
          // Untracked file or no-op: fall back to whole-file staging
          // via `git add` which handles untracked paths correctly.
          // Only makes sense for the stage direction; unstage with no
          // hunks is a no-op.
          if (intent === "stage") {
            await runStagingOp(
              () => rpc.gitAddPaths(repo.repo_id, [path]),
              "Staged file",
            );
          }
          return;
        }

        await runStagingOp(
          () =>
            intent === "stage"
              ? rpc.gitStageHunks(repo.repo_id, hunks)
              : rpc.gitUnstageHunks(repo.repo_id, hunks),
          intent === "stage" ? "Staged file" : "Unstaged file",
        );
      } finally {
        setTogglingPaths((prev) => {
          const next = new Set(prev);
          next.delete(path);
          return next;
        });
      }
    },
    [togglingPaths, workTreePatch, cachedPatch, runStagingOp, repo.repo_id],
  );

  // --- Multi-select + Batch operations ---

  /** Handle file selection with modifier key support for multi-select. */
  const handleFileSelect = useCallback(
    (path: string, e: React.MouseEvent) => {
      if (e.metaKey || e.ctrlKey) {
        // Toggle individual file in multi-selection
        setMultiSelected((prev) => {
          const next = new Set(prev);
          if (next.has(path)) next.delete(path);
          else next.add(path);
          return next;
        });
      } else if (e.shiftKey && lastClickedRef.current && entries) {
        // Range select from last clicked to current
        const paths = entries.map((en) => en.path);
        const lastIdx = paths.indexOf(lastClickedRef.current);
        const curIdx = paths.indexOf(path);
        if (lastIdx !== -1 && curIdx !== -1) {
          const [start, end] = lastIdx < curIdx ? [lastIdx, curIdx] : [curIdx, lastIdx];
          const range = new Set(paths.slice(start, end + 1));
          setMultiSelected(range);
        }
      } else {
        // Normal click — clear multi-select, set single selection
        setMultiSelected(new Set());
        setSelectedPath(path);
        // Clear open-file mode
        setOpenFilePath(null);
        setOpenFileContent(null);
        // For new untracked files, automatically show full file content
        const code = statusByPath.get(path) ?? "";
        if (code === "??") {
          void (async () => {
            setOpenFilePath(path);
            setOpenFileLoading(true);
            try {
              const result = await rpc.readText(repo.repo_id, path);
              setOpenFileContent(result.text);
            } catch {
              // Fall through
            } finally {
              setOpenFileLoading(false);
            }
          })();
        }
      }
      lastClickedRef.current = path;
    },
    [entries, statusByPath, repo.repo_id],
  );

  const clearMultiSelect = useCallback(() => {
    setMultiSelected(new Set());
  }, []);

  /** Batch stage all multi-selected files. */
  const batchStage = useCallback(async () => {
    if (multiSelected.size === 0) return;
    setBatchBusy(true);
    try {
      await rpc.gitAddPaths(repo.repo_id, [...multiSelected]);
      skipPatchPairEffectRef.current = true;
      refreshStatus();
      await refreshPatchPair();
      setMultiSelected(new Set());
    } catch (e) {
      setToast({ kind: "error", text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBatchBusy(false);
    }
  }, [multiSelected, repo.repo_id, refreshStatus, refreshPatchPair]);

  /** Batch unstage all multi-selected files. */
  const batchUnstage = useCallback(async () => {
    if (multiSelected.size === 0) return;
    setBatchBusy(true);
    try {
      await rpc.gitResetPaths(repo.repo_id, [...multiSelected]);
      skipPatchPairEffectRef.current = true;
      refreshStatus();
      await refreshPatchPair();
      setMultiSelected(new Set());
    } catch (e) {
      setToast({ kind: "error", text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBatchBusy(false);
    }
  }, [multiSelected, repo.repo_id, refreshStatus, refreshPatchPair]);

  /** Batch discard all multi-selected files. */
  const batchDiscard = useCallback(async () => {
    if (multiSelected.size === 0) return;
    setBatchBusy(true);
    try {
      // Use git checkout -- for tracked files, clean for untracked
      await rpc.gitCheckout(repo.repo_id, ".", { pathspec: true });
      skipPatchPairEffectRef.current = true;
      refreshStatus();
      await refreshPatchPair();
      setMultiSelected(new Set());
    } catch (e) {
      setToast({ kind: "error", text: e instanceof Error ? e.message : String(e) });
    } finally {
      setBatchBusy(false);
    }
  }, [multiSelected, repo.repo_id, refreshStatus, refreshPatchPair]);

  // Keyboard shortcuts for batch operations
  useEffect(() => {
    if (multiSelected.size < 2) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        clearMultiSelect();
        e.preventDefault();
      } else if (e.key === "s" && !e.metaKey && !e.ctrlKey) {
        void batchStage();
        e.preventDefault();
      } else if (e.key === "u" && !e.metaKey && !e.ctrlKey) {
        void batchUnstage();
        e.preventDefault();
      } else if (e.key === "Delete" || e.key === "Backspace") {
        void batchDiscard();
        e.preventDefault();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [multiSelected.size, clearMultiSelect, batchStage, batchUnstage, batchDiscard]);

  // Staged file count for readiness indicator
  const stagedFileCount = useMemo(() => {
    let count = 0;
    for (const state of Object.values(fileStates)) {
      if (state === "partial" || state === "full") count++;
    }
    return count;
  }, [fileStates]);

  // Summary counts at the top — mirrors the VS Code SCM view.
  const summary = useMemo(() => {
    const s = { modified: 0, added: 0, deleted: 0, untracked: 0, renamed: 0, other: 0 };
    for (const e of entries ?? []) {
      const code = e.code.trim();
      if (code === "??") s.untracked++;
      else if (code[0] === "M" || code[1] === "M") s.modified++;
      else if (code[0] === "A") s.added++;
      else if (code[0] === "D" || code[1] === "D") s.deleted++;
      else if (code[0] === "R") s.renamed++;
      else s.other++;
    }
    return s;
  }, [entries]);

  const fileListScrollRef = useRef<HTMLDivElement | null>(null);
  const sessionScrollRef = useSessionScroll("changes");

  // Callback ref that merges the local fileListScrollRef (used by FlatList virtualizer)
  // with the sessionScrollRef (used for scroll position persistence).
  const mergedFileListRef = useCallback(
    (node: HTMLDivElement | null) => {
      fileListScrollRef.current = node;
      // Assign to the session scroll ref (MutableRefObject)
      (sessionScrollRef as React.MutableRefObject<HTMLDivElement | null>).current = node;
    },
    [sessionScrollRef],
  );

  const selectedStatus = selectedPath ? (statusByPath.get(selectedPath) ?? "") : "";
  const canUnstage = hasStagedChanges(selectedStatus);

  const onSplitResize = useCallback((size: number) => {
    if (size >= 12 && size <= 80) {
      localStorage.setItem(LS_CHANGES_SPLIT, String(size));
    }
  }, []);

  const onCommitSuccess = useCallback(
    (sha: string) => {
      refreshStatus();
      void refreshPatchPair();
      onCommit?.(sha);
    },
    [refreshStatus, refreshPatchPair, onCommit],
  );

  // Clean empty state is handled in the JSX below — we can't return
  // early here because hooks must be called unconditionally.

  // Show clean empty state when there are no changes.
  if (!loading && entries && entries.length === 0) {
    return (
      <div className="flex h-full items-center justify-center">
        <div className="text-center">
          <div className="mb-2 text-2xl text-gx-success">✓</div>
          <div className="text-sm text-gx-text">Working tree is clean</div>
          <div className="mt-1 text-[11px] text-gx-muted">No uncommitted changes</div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <ViewToolbar
        title="Changes"
        icon={GitPullRequestArrow}
        actions={toolbarActions}
        onAction={handleToolbarAction}
      />
      <div className="flex shrink-0 items-center justify-between border-b border-gx-border px-3 py-2 text-[11px] text-gx-muted">
        <div className="flex items-center gap-3">
          <GitPullRequestArrow size={12} />
          {entries ? (
            <span>
              <strong className="text-gx-text">{entries.length}</strong> change
              {entries.length === 1 ? "" : "s"}
            </span>
          ) : (
            <span>…</span>
          )}
          {entries && entries.length > 0 && (
            <span className="flex items-center gap-2">
              {summary.modified > 0 && (
                <span className="text-gx-warn">{summary.modified} modified</span>
              )}
              {summary.added > 0 && (
                <span className="text-gx-success">{summary.added} added</span>
              )}
              {summary.untracked > 0 && (
                <span className="text-gx-muted">{summary.untracked} untracked</span>
              )}
              {summary.deleted > 0 && (
                <span className="text-gx-error">{summary.deleted} deleted</span>
              )}
              {summary.renamed > 0 && (
                <span className="text-gx-accent">{summary.renamed} renamed</span>
              )}
            </span>
          )}
        </div>
        <div className="flex items-center gap-2">
          <div
            role="radiogroup"
            aria-label="View mode"
            className="flex overflow-hidden rounded border border-gx-border"
          >
            <ModeButton
              active={mode === "zones"}
              icon={<Layers size={11} />}
              label="Zones"
              onClick={() => setMode("zones")}
            />
            <ModeButton
              active={mode === "list"}
              icon={<List size={11} />}
              label="List"
              onClick={() => setMode("list")}
            />
            <ModeButton
              active={mode === "tree"}
              icon={<FolderTree size={11} />}
              label="Tree"
              onClick={() => setMode("tree")}
            />
          </div>
          <button
            type="button"
            className="text-gx-muted hover:text-gx-text"
            onClick={refreshStatus}
            title="Refresh"
          >
            <RefreshCw size={11} />
          </button>
        </div>
      </div>

      <div className="min-h-0 flex-1">
        <PanelGroup direction="horizontal" className="h-full">
          <Panel
            defaultSize={loadSplit()}
            minSize={16}
            maxSize={80}
            onResize={onSplitResize}
            className="flex min-h-0 flex-col"
          >
            {/* Batch action bar — appears when 2+ files are multi-selected */}
            <BatchActionBar
              count={multiSelected.size}
              onStage={batchStage}
              onUnstage={batchUnstage}
              onDiscard={batchDiscard}
              onClear={clearMultiSelect}
              busy={batchBusy}
            />
            <div ref={mergedFileListRef} className="min-h-0 flex-1 overflow-auto">
              {loading && (
                <div className="flex items-center gap-2 p-3 text-xs text-gx-muted">
                  <Loader2 size={12} className="animate-spin" /> Running git status…
                </div>
              )}
              {error && (
                <div className="m-3 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
                  {error}
                </div>
              )}
              {!loading && entries && entries.length === 0 && (
                <div className="p-4 text-center text-xs text-gx-muted">
                  Working tree is clean.
                </div>
              )}
              {!loading && entries && entries.length > 0 && mode === "zones" && (
                <StagingZones
                  entries={entries}
                  fileStates={fileStates}
                  selectedPath={selectedPath}
                  multiSelected={multiSelected}
                  togglingPaths={togglingPaths}
                  onSelect={handleFileSelect}
                  onDoubleClick={openFileDiff}
                  onToggleFile={onToggleFile}
                  onDiscardFile={handleDiscardFileByPath}
                  onOpenFile={handleOpenFile}
                  onContextMenu={handleFileContextMenu}
                />
              )}
              {!loading && entries && entries.length > 0 && mode === "list" && (
                <FlatList
                  entries={entries}
                  selectedPath={selectedPath}
                  activeDiffPath={activeDiffPath ?? null}
                  fileStates={fileStates}
                  togglingPaths={togglingPaths}
                  onSelect={selectFile}
                  onDoubleClick={openFileDiff}
                  onToggleFile={onToggleFile}
                  onDiscardFileByPath={handleDiscardFileByPath}
                  onOpenFile={handleOpenFile}
                  onContextMenu={handleFileContextMenu}
                  scrollRef={fileListScrollRef}
                />
              )}
              {!loading && entries && entries.length > 0 && mode === "tree" && (
                <ChangesTree
                  entries={entries}
                  selectedPath={selectedPath}
                  fileStates={fileStates}
                  onSelect={selectFile}
                  onDoubleClick={openFileDiff}
                  onContextMenu={handleFileContextMenu}
                />
              )}
            </div>
            {/* Commit readiness indicator */}
            <CommitReadiness
              stagedCount={stagedFileCount}
              hasMessage={true}
              ready={hasStaged}
            />
            <CommitComposer
              repo={repo}
              hasStaged={hasStaged}
              stagedPaths={stagedPaths}
              onCommitSuccess={onCommitSuccess}
            />
          </Panel>
          <PanelResizeHandle className="w-[3px] cursor-col-resize bg-transparent transition-colors hover:bg-gx-accent/50" />
          <Panel minSize={20} className="min-h-0 min-w-0">
            {openFilePath ? (
              <FileContentPane
                path={openFilePath}
                content={openFileContent}
                loading={openFileLoading}
                onClose={() => { setOpenFilePath(null); setOpenFileContent(null); }}
              />
            ) : (
              <PatchPane
                selectedPath={selectedPath}
                patch={patch}
                loading={patchLoading}
                error={patchError}
                canUnstage={canUnstage}
                onStageHunk={onStageHunk}
                onUnstageHunk={onUnstageHunk}
                onDiscardHunk={onDiscardHunk}
                onDiscardLine={onDiscardLine}
                onStageFile={onStageFile}
                onUnstageFile={onUnstageFile}
                onDiscardFile={onDiscardFile}
                toast={toast}
                onDismissToast={() => setToast(null)}
              />
            )}
          </Panel>
        </PanelGroup>
      </div>
      {pendingDiscard && (
        <ConfirmDiscardDialog
          files={pendingDiscard.files}
          lineCount={pendingDiscard.lineCount}
          threshold={settings.discardConfirmThreshold}
          onConfirm={confirmPendingDiscard}
          onCancel={cancelPendingDiscard}
        />
      )}
      {contextMenu && (
        <ContextMenu
          items={fileContextMenuItems}
          position={contextMenu.position}
          onSelect={handleContextMenuSelect}
          onClose={() => setContextMenu(null)}
        />
      )}
    </div>
  );
}

function ModeButton({
  active,
  icon,
  label,
  onClick,
}: {
  active: boolean;
  icon: React.ReactNode;
  label: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={active}
      onClick={onClick}
      className={clsx(
        "flex items-center gap-1 px-2 py-0.5 text-[11px] transition",
        active
          ? "bg-gx-accent/20 text-gx-text"
          : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
      )}
      title={`${label} view`}
    >
      {icon}
      {label}
    </button>
  );
}

const FILE_ROW_HEIGHT = 28;

function FlatList({
  entries,
  selectedPath,
  activeDiffPath,
  fileStates,
  togglingPaths,
  onSelect,
  onDoubleClick,
  onToggleFile,
  onDiscardFileByPath,
  onOpenFile,
  onContextMenu,
  scrollRef,
}: {
  entries: StatusEntry[];
  selectedPath: string | null;
  activeDiffPath: string | null;
  fileStates: Record<string, TriStateValue>;
  togglingPaths: Set<string>;
  onSelect: (path: string) => void;
  onDoubleClick: (path: string) => void;
  onToggleFile: (path: string, intent: "stage" | "unstage") => void;
  onDiscardFileByPath: (path: string) => void;
  onOpenFile?: (path: string) => void;
  onContextMenu: (e: React.MouseEvent, path: string) => void;
  scrollRef: React.RefObject<HTMLDivElement | null>;
}) {
  const virtualizer = useVirtualizer({
    count: entries.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => FILE_ROW_HEIGHT,
    overscan: 8,
  });

  return (
    <ul role="listbox" aria-label="Changed files" className="relative" style={{ height: virtualizer.getTotalSize() }}>
      {virtualizer.getVirtualItems().map((virtualItem) => {
        const e = entries[virtualItem.index];
        const meta = labelFor(e.code);
        const isSelected = selectedPath === e.path;
        const isActiveDiff = activeDiffPath === e.path;
        const state = fileStates[e.path] ?? "empty";
        const toggling = togglingPaths.has(e.path);
        return (
          <li
            key={e.path}
            role="option"
            aria-selected={isSelected}
            style={{
              position: "absolute",
              top: 0,
              left: 0,
              width: "100%",
              height: FILE_ROW_HEIGHT,
              transform: `translateY(${virtualItem.start}px)`,
            }}
          >
            <div
              onClick={() => onSelect(e.path)}
              onDoubleClick={() => onDoubleClick(e.path)}
              onContextMenu={(ev) => onContextMenu(ev, e.path)}
              className={clsx(
                "group/row flex h-full w-full items-center gap-2 px-3 text-left text-[12.5px] transition cursor-pointer",
                isSelected
                  ? "bg-gx-accent/20 text-gx-text"
                  : isActiveDiff
                    ? "bg-gx-surface-hover text-gx-text"
                    : "text-gx-text hover:bg-gx-surface-hover",
              )}
              title={`${e.code} ${e.path}`}
            >
              {/* Read-only staging state indicator */}
              <span
                className={clsx(
                  "inline-flex h-[14px] w-[14px] shrink-0 items-center justify-center rounded-[2px] border",
                  state === "empty"
                    ? "border-gx-border bg-transparent"
                    : "border-gx-accent bg-gx-accent",
                )}
                aria-label={
                  state === "full"
                    ? "Fully staged"
                    : state === "partial"
                      ? "Partially staged"
                      : "Not staged"
                }
              >
                {state === "full" && (
                  <svg width="10" height="10" viewBox="0 0 10 10" fill="none" aria-hidden="true" className="text-white">
                    <path d="M1.5 5.2 L4 7.7 L8.5 2.8" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
                  </svg>
                )}
                {state === "partial" && (
                  <svg width="10" height="10" viewBox="0 0 10 10" fill="none" aria-hidden="true" className="text-white">
                    <path d="M2 5 L8 5" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
                  </svg>
                )}
              </span>
              <span className={clsx("w-4 shrink-0 text-center font-mono text-[11px]", meta.cls)}>
                {codeBadge(e.code)}
              </span>
              <span className="min-w-0 flex-1 truncate font-mono">
                {e.path.split("/").pop()}
              </span>
              <span className="shrink-0 text-[10px] text-gx-muted group-hover/row:hidden">
                {e.path.includes("/") && e.path.slice(0, e.path.lastIndexOf("/"))}
              </span>
              {/* Per-file action buttons — visible on hover */}
              <span className="hidden shrink-0 items-center gap-0.5 group-hover/row:flex">
                {state !== "full" && (
                  <button
                    type="button"
                    title={`Stage ${e.path}`}
                    aria-label={`Stage ${e.path}`}
                    disabled={toggling}
                    onClick={(ev) => { ev.stopPropagation(); onToggleFile(e.path, "stage"); }}
                    className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-accent/20 hover:text-gx-success disabled:opacity-50 disabled:cursor-not-allowed transition"
                  >
                    <Plus size={13} />
                  </button>
                )}
                {state !== "empty" && (
                  <button
                    type="button"
                    title={`Unstage ${e.path}`}
                    aria-label={`Unstage ${e.path}`}
                    disabled={toggling}
                    onClick={(ev) => { ev.stopPropagation(); onToggleFile(e.path, "unstage"); }}
                    className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-accent/20 hover:text-gx-warn disabled:opacity-50 disabled:cursor-not-allowed transition"
                  >
                    <Minus size={13} />
                  </button>
                )}
                <button
                  type="button"
                  title={`Discard changes in ${e.path}`}
                  aria-label={`Discard changes in ${e.path}`}
                  disabled={toggling}
                  onClick={(ev) => { ev.stopPropagation(); onDiscardFileByPath(e.path); }}
                  className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-error/20 hover:text-gx-error disabled:opacity-50 disabled:cursor-not-allowed transition"
                >
                  <Undo2 size={13} />
                </button>
                {onOpenFile && (
                  <button
                    type="button"
                    title={`Open file ${e.path}`}
                    aria-label={`Open file ${e.path}`}
                    onClick={(ev) => { ev.stopPropagation(); onOpenFile(e.path); }}
                    className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:bg-gx-accent/20 hover:text-gx-accent transition"
                  >
                    <FileText size={13} />
                  </button>
                )}
              </span>
            </div>
          </li>
        );
      })}
    </ul>
  );
}

/**
 * Tree view of pending changes. Uses `@pierre/trees` with:
 *   - `gitStatus` prop so the library renders the native A/M/D
 *     decoration on each row
 *   - `renderRowDecoration` to append an explicit single-letter
 *     status badge plus a tri-state staging glyph at the right
 *     edge. Status stays legible independent of colour
 *     (accessibility); the glyph mirrors the list-view checkbox.
 *   - `initialExpansion: "open"` so every affected directory is
 *     pre-expanded. In a changes view the user always wants to see
 *     the files, not hunt through collapsed folders.
 *
 * Note: `@pierre/trees` accepts only `{text, title}` / `{icon,
 * title}` decorations — no custom JSX. The tri-state glyph is
 * therefore informational in tree mode; interactive toggling lives
 * in the list view. This is a deliberate compromise, documented so
 * future refactors don't re-try an overlay approach against a
 * shadow DOM.
 *
 * Selection is one-way from the tree to the patch pane: clicking
 * a file populates the right pane.
 */
function ChangesTree({
  entries,
  selectedPath,
  fileStates,
  onSelect,
  onDoubleClick,
  onContextMenu,
}: {
  entries: StatusEntry[];
  selectedPath: string | null;
  fileStates: Record<string, TriStateValue>;
  onSelect: (path: string) => void;
  onDoubleClick: (path: string) => void;
  onContextMenu: (e: React.MouseEvent, path: string) => void;
}) {
  const paths = useMemo(() => entries.map((e) => e.path).sort(), [entries]);
  const byPath = useMemo(() => {
    const m = new Map<string, StatusEntry>();
    for (const e of entries) m.set(e.path, e);
    return m;
  }, [entries]);

  const { model } = useFileTree({
    paths,
    initialExpansion: "open",
    dragAndDrop: false,
    renaming: false,
    icons: { set: "complete", colored: true },
    renderRowDecoration: ({ item }) => {
      if (item.kind === "directory") return null;
      const status = byPath.get(item.path);
      if (!status) return null;
      const meta = labelFor(status.code);
      const glyph = triStateGlyph(fileStates[item.path] ?? "empty");
      return {
        text: `${glyph} ${codeBadge(status.code)}`,
        title: `${status.code} ${meta.label} — ${fileStates[item.path] ?? "empty"}`,
      };
    },
  });

  // Keep paths + status fresh when the underlying list changes.
  useEffect(() => {
    model.resetPaths(paths);
  }, [model, paths]);

  useEffect(() => {
    const status: GitStatusEntry[] = entries.map((e) => ({
      path: e.path,
      status: toTreesStatus(e.code),
    }));
    model.setGitStatus(status);
  }, [model, entries]);

  // Selection → populate patch pane.
  const selection = useFileTreeSelection(model);
  const lastOpened = useRef<string | null>(null);
  useEffect(() => {
    const first = selection[0];
    if (!first || first === lastOpened.current) return;
    if (!byPath.has(first)) return;
    lastOpened.current = first;
    onSelect(first);
  }, [selection, byPath, onSelect]);

  // Highlight the active file by focusing it in the tree.
  useEffect(() => {
    if (!selectedPath) return;
    try {
      model.focusPath(selectedPath);
    } catch {
      // Path not present in the model (just toggled mode); harmless.
    }
  }, [model, selectedPath]);

  const effective = useEffectiveTheme();
  const themeStyles = useMemo(() => buildTreeTheme(effective), [effective]);

  // Double-click on the tree opens the full editor diff for the
  // currently selected file (VS Code SCM behaviour).
  const handleDoubleClick = useCallback(() => {
    const first = selection[0];
    if (first && byPath.has(first)) {
      onDoubleClick(first);
    }
  }, [selection, byPath, onDoubleClick]);

  // Right-click on the tree opens the context menu for the
  // currently selected file.
  const handleTreeContextMenu = useCallback(
    (e: React.MouseEvent) => {
      const first = selection[0];
      if (first && byPath.has(first)) {
        onContextMenu(e, first);
      }
    },
    [selection, byPath, onContextMenu],
  );

  return (
    <div onContextMenu={handleTreeContextMenu} className="block h-full">
      <PierreFileTree
        model={model}
        className="block h-full"
        style={themeStyles}
        onDoubleClick={handleDoubleClick}
      />
    </div>
  );
}

interface PatchPaneProps {
  selectedPath: string | null;
  patch: string | null;
  loading: boolean;
  error: string | null;
  canUnstage: boolean;
  onStageHunk: Parameters<typeof InlinePatchDiff>[0]["onStageHunk"];
  onUnstageHunk: Parameters<typeof InlinePatchDiff>[0]["onUnstageHunk"];
  onDiscardHunk: Parameters<typeof InlinePatchDiff>[0]["onDiscardHunk"];
  onDiscardLine: Parameters<typeof InlinePatchDiff>[0]["onDiscardLine"];
  onStageFile: Parameters<typeof InlinePatchDiff>[0]["onStageFile"];
  onUnstageFile: Parameters<typeof InlinePatchDiff>[0]["onUnstageFile"];
  onDiscardFile: Parameters<typeof InlinePatchDiff>[0]["onDiscardFile"];
  toast: { kind: "info" | "error"; text: string } | null;
  onDismissToast: () => void;
}

/**
 * Full file content pane — replaces the diff view when the user clicks
 * "Open File" on a file row. Shows the raw file content with line numbers.
 */
function FileContentPane({
  path,
  content,
  loading,
  onClose,
}: {
  path: string;
  content: string | null;
  loading: boolean;
  onClose: () => void;
}) {
  const lines = useMemo(() => content?.split("\n") ?? [], [content]);

  return (
    <div className="relative flex h-full min-h-0 flex-col bg-gx-bg-sub">
      <div className="flex shrink-0 items-center gap-2 border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
        <FileText size={12} className="text-gx-accent" />
        <span className="font-mono text-gx-text truncate">{path}</span>
        <span className="ml-auto text-[10px] text-gx-muted">
          {content != null && `${lines.length} lines`}
        </span>
        <button
          type="button"
          onClick={onClose}
          className="ml-2 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text transition"
          title="Close file view and return to diff"
        >
          ✕
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
        {loading && (
          <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
            <Loader2 size={14} className="animate-spin" /> Loading file…
          </div>
        )}
        {!loading && content == null && (
          <div className="flex h-full items-center justify-center text-sm text-gx-muted">
            Unable to read file content (binary or missing).
          </div>
        )}
        {!loading && content != null && (
          <pre className="p-0 m-0 text-[12px] leading-[1.6] font-mono">
            <table className="w-full border-collapse">
              <tbody>
                {lines.map((line, i) => (
                  <tr key={i} className="hover:bg-gx-surface-hover/50">
                    <td className="select-none px-3 py-0 text-right text-[11px] text-gx-muted/60 w-[1%] whitespace-nowrap align-top">
                      {i + 1}
                    </td>
                    <td className="px-3 py-0 whitespace-pre-wrap break-all text-gx-text">
                      {line || "\u00A0"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </pre>
        )}
      </div>
    </div>
  );
}

function PatchPane({
  selectedPath,
  patch,
  loading,
  error,
  canUnstage,
  onStageHunk,
  onUnstageHunk,
  onDiscardHunk,
  onDiscardLine,
  onStageFile,
  onUnstageFile,
  onDiscardFile,
  toast,
  onDismissToast,
}: PatchPaneProps) {
  return (
    <div className="relative flex h-full min-h-0 flex-col bg-gx-bg-sub">
      <div className="flex shrink-0 items-center gap-2 border-b border-gx-border bg-gx-surface px-3 py-1.5 text-[11px] text-gx-muted">
        <span className="font-mono text-gx-text">
          {selectedPath ?? "No file selected"}
        </span>
        <span className="ml-auto">Working tree vs Index</span>
      </div>
      <div className="min-h-0 flex-1 overflow-auto">
        {!selectedPath && (
          <div className="flex h-full flex-col items-center justify-center gap-2 text-sm text-gx-muted">
            <FileText size={18} />
            <div>Select a file to review its changes.</div>
          </div>
        )}
        {selectedPath && loading && (
          <div className="flex h-full items-center justify-center gap-2 text-sm text-gx-muted">
            <Loader2 size={14} className="animate-spin" /> Loading diff…
          </div>
        )}
        {selectedPath && error && (
          <div className="m-4 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
            {error}
          </div>
        )}
        {selectedPath && !loading && !error && patch != null && patch.trim().length === 0 && (
          <div className="flex h-full items-center justify-center text-sm text-gx-muted">
            No changes for this file.
          </div>
        )}
        {selectedPath && !loading && !error && patch != null && patch.trim().length > 0 && (
          <InlinePatchDiff
            patch={patch}
            canUnstage={canUnstage}
            onStageHunk={onStageHunk}
            onUnstageHunk={onUnstageHunk}
            onDiscardHunk={onDiscardHunk}
            onDiscardLine={onDiscardLine}
            onStageFile={onStageFile}
            onUnstageFile={onUnstageFile}
            onDiscardFile={onDiscardFile}
            emptyState="No hunks — this file may be binary."
          />
        )}
      </div>
      {toast && (
        <button
          type="button"
          onClick={onDismissToast}
          className={clsx(
            "absolute bottom-3 left-1/2 -translate-x-1/2 rounded border px-3 py-1.5 text-xs shadow-lg transition",
            toast.kind === "error"
              ? "border-gx-error/40 bg-gx-error/10 text-gx-error"
              : "border-gx-border bg-gx-surface text-gx-text",
          )}
        >
          <span className="mr-2 inline-flex items-center gap-1">
            {toast.kind === "error" && <AlertTriangle size={11} />}
            {toast.text}
          </span>
          <span className="text-gx-muted">dismiss</span>
        </button>
      )}
    </div>
  );
}

// Mirrors the helper in `components/FileTree.tsx`. We duplicate a
// minimal version here to avoid a cross-component import cycle;
// eventually this should graduate into a `lib/treeTheme.ts`.
function buildTreeTheme(mode: "dark" | "light"): React.CSSProperties {
  const cs =
    typeof window !== "undefined" ? getComputedStyle(document.documentElement) : null;
  const rgb = (name: string, fallback: string): string => {
    const raw = cs?.getPropertyValue(name).trim();
    if (!raw) return fallback;
    return `rgb(${raw})`;
  };
  const colors: Record<string, string> = {
    "sideBar.background": rgb("--gx-bg-sub", mode === "dark" ? "#0b0d12" : "#ffffff"),
    "editor.background": rgb("--gx-bg-sub", mode === "dark" ? "#0b0d12" : "#ffffff"),
    "sideBar.foreground": rgb("--gx-text", mode === "dark" ? "#e7eaf1" : "#171b23"),
    "sideBarSectionHeader.foreground": rgb("--gx-muted", "#8a93a5"),
    "sideBar.border": rgb("--gx-border", mode === "dark" ? "#242832" : "#dde0e8"),
    "list.hoverBackground": rgb(
      "--gx-surface-hover",
      mode === "dark" ? "#1e222b" : "#e9ecf3",
    ),
    "list.activeSelectionBackground": mode === "dark" ? "#4f8cff33" : "#2c6def22",
    "list.activeSelectionForeground": rgb("--gx-text", mode === "dark" ? "#e7eaf1" : "#171b23"),
    focusBorder: rgb("--gx-accent", mode === "dark" ? "#4f8cff" : "#2c6def"),
    "input.background": rgb("--gx-bg-sub", mode === "dark" ? "#0b0d12" : "#ffffff"),
    "input.border": rgb("--gx-border", mode === "dark" ? "#242832" : "#dde0e8"),
    "scrollbarSlider.background": rgb(
      "--gx-border",
      mode === "dark" ? "#242832" : "#dde0e8",
    ),
    "gitDecoration.addedResourceForeground": rgb(
      "--gx-success",
      mode === "dark" ? "#4ade80" : "#16a34a",
    ),
    "gitDecoration.modifiedResourceForeground": rgb(
      "--gx-warn",
      mode === "dark" ? "#facc15" : "#b48700",
    ),
    "gitDecoration.deletedResourceForeground": rgb(
      "--gx-error",
      mode === "dark" ? "#f87171" : "#dc2626",
    ),
  };
  const styles = themeToTreeStyles({
    type: mode,
    bg: colors["sideBar.background"],
    fg: colors["sideBar.foreground"],
    colors,
  });
  return styles as unknown as React.CSSProperties;
}

// Consumers of this view need the underlying status entries to
// decorate the tree with git signals. Expose a parallel hook so we
// only pay for one `git_status_porcelain` round-trip per repo.
export function useGitStatusMap(repoId: string, enabled = true) {
  const [map, setMap] = useState<Record<string, string>>({});
  const [tick, setTick] = useState(0);
  // Keep a ref to the current map so the equality check below doesn't
  // need to be a dependency of the effect.
  const mapRef = useRef(map);
  mapRef.current = map;

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    rpc
      .gitStatus(repoId)
      .then((r) => {
        if (cancelled) return;
        const next: Record<string, string> = {};
        for (const e of r.entries) next[e.path] = e.code;
        // Skip the state update when the content hasn't changed.
        // This prevents the RepoDetail → ExplorerView → FileTree
        // re-render chain from firing on every fs/changed event
        // when the working tree is actually clean.
        const prev = mapRef.current;
        const prevKeys = Object.keys(prev);
        const nextKeys = Object.keys(next);
        if (
          prevKeys.length === nextKeys.length &&
          prevKeys.every((k) => prev[k] === next[k])
        ) {
          return;
        }
        setMap(next);
      })
      .catch(() => setMap({}));
    return () => {
      cancelled = true;
    };
  }, [repoId, enabled, tick]);
  const refresh = useCallback(() => setTick((t) => t + 1), []);
  return { map, refresh };
}
