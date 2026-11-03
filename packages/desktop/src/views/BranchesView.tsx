import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import {
  ArrowDown,
  ArrowUp,
  ChevronDown,
  ChevronRight,
  Download,
  GitBranch,
  GitMerge,
  GitPullRequest,
  GitPullRequestArrow,
  Loader2,
  Pencil,
  Plus,
  RefreshCw,
  Trash2,
} from "lucide-react";
import { OpenRepo } from "../state";
import { RefEntry, RefKind, rpc } from "../ipc";
import { useFileWatcher, getWatchPath } from "../hooks/useFileWatcher";
import { useSessionScroll } from "../hooks/useSessionScroll";
import { ConfirmForceDeleteDialog } from "../components/ConfirmForceDeleteDialog";
import { ConfirmForcePushDialog } from "../components/ConfirmForcePushDialog";
import { MergeDialog } from "../components/MergeDialog";
import { RebaseDialog } from "../components/RebaseDialog";
import { CreatePrDialog } from "../components/CreatePrDialog";
import { ViewToolbar, ToolbarAction } from "../components/ViewToolbar";
import { ContextMenu, MenuItemDef } from "../components/ContextMenu";
import { executeCommand, CommandContext } from "../lib/command-registry";
import { publishToast } from "../lib/toasts";

// ── Pull mode types ────────────────────────────────────────────────

type PullMode = "merge" | "rebase" | "ff-only";

function getPullModeKey(repoId: string, branch: string): string {
  return `crab.pull-mode.${repoId}.${branch}`;
}

function getStoredPullMode(repoId: string, branch: string): PullMode {
  try {
    const stored = localStorage.getItem(getPullModeKey(repoId, branch));
    if (stored === "merge" || stored === "rebase" || stored === "ff-only") return stored;
  } catch {
    // localStorage may be unavailable
  }
  return "merge";
}

function setStoredPullMode(repoId: string, branch: string, mode: PullMode): void {
  try {
    localStorage.setItem(getPullModeKey(repoId, branch), mode);
  } catch {
    // Silently ignore storage failures
  }
}

interface Props {
  repo: OpenRepo;
  selectedBranch?: string | null;
  onSelectBranch?: (name: string) => void;
}

// ── Tree data model ────────────────────────────────────────────────

interface BranchTreeNode {
  /** Segment name (e.g. "feat" or "auth") */
  segment: string;
  /** Full prefix path up to and including this node (e.g. "feat/") */
  prefix: string;
  children: BranchTreeNode[];
  /** Leaf branches living directly under this prefix */
  branches: RefEntry[];
}

function buildTree(refs: RefEntry[]): BranchTreeNode {
  const root: BranchTreeNode = { segment: "", prefix: "", children: [], branches: [] };

  for (const ref of refs) {
    const parts = ref.name.split("/");
    let node = root;

    if (parts.length === 1) {
      node.branches.push(ref);
    } else {
      // Walk/create intermediate folder nodes
      for (let i = 0; i < parts.length - 1; i++) {
        const seg = parts[i];
        const prefix = parts.slice(0, i + 1).join("/") + "/";
        let child = node.children.find((c) => c.segment === seg);
        if (!child) {
          child = { segment: seg, prefix, children: [], branches: [] };
          node.children.push(child);
        }
        node = child;
      }
      node.branches.push(ref);
    }
  }

  return root;
}

/** Extract the leaf name (last segment after the last `/`). */
function leafName(fullName: string): string {
  const idx = fullName.lastIndexOf("/");
  return idx === -1 ? fullName : fullName.slice(idx + 1);
}

// ── Context menu ───────────────────────────────────────────────────

interface ContextMenuState {
  x: number;
  y: number;
  branch: RefEntry;
}

// ── Main component ─────────────────────────────────────────────────

export function BranchesView({ repo, selectedBranch, onSelectBranch }: Props) {
  const scrollRef = useSessionScroll("branches");
  const [refs, setRefs] = useState<RefEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [contextMenu, setContextMenu] = useState<ContextMenuState | null>(null);
  const [renamingBranch, setRenamingBranch] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [forceDeleteBranch, setForceDeleteBranch] = useState<string | null>(null);
  const [forcePushBranch, setForcePushBranch] = useState<string | null>(null);
  const [mergeBranch, setMergeBranch] = useState<string | null>(null);
  const [rebaseBranch, setRebaseBranch] = useState<string | null>(null);
  const [toast, setToast] = useState<{ kind: "info" | "error"; text: string } | null>(null);
  const [fetchingAll, setFetchingAll] = useState(false);
  const [fetchedRemotes, setFetchedRemotes] = useState<string[] | null>(null);
  const [createPrBranch, setCreatePrBranch] = useState<string | null>(null);
  const [postPushBranch, setPostPushBranch] = useState<string | null>(null);

  const refresh = useCallback(() => {
    setLoading(true);
    setError(null);
    rpc
      .listRefs(repo.repo_id)
      .then((r) => setRefs(r.refs))
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [repo.repo_id]);

  useEffect(refresh, [refresh]);

  // Auto-refresh branch list when git refs change on disk.
  useFileWatcher(getWatchPath(repo), refresh);

  // Toast auto-dismiss.
  useEffect(() => {
    if (!toast) return;
    const h = setTimeout(() => setToast(null), 4000);
    return () => clearTimeout(h);
  }, [toast]);

  // Find the current HEAD branch name for merge target.
  const headBranch = (refs ?? []).find((r) => r.is_head && r.kind === "branch");

  // Dismiss context menu on click-outside or Escape
  useEffect(() => {
    if (!contextMenu) return;
    const dismiss = () => setContextMenu(null);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") dismiss();
    };
    window.addEventListener("click", dismiss);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("click", dismiss);
      window.removeEventListener("keydown", onKey);
    };
  }, [contextMenu]);

  // ── Actions ────────────────────────────────────────────────────

  async function handleForceDelete(branchName: string) {
    setBusy(true);
    setError(null);
    setForceDeleteBranch(null);
    try {
      await rpc.gitBranchDelete(repo.repo_id, branchName, { force: true });
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleRenameSubmit(oldName: string) {
    if (!renameValue.trim() || renameValue.trim() === oldName) {
      setRenamingBranch(null);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await rpc.gitBranchRename(repo.repo_id, oldName, renameValue.trim());
      setRenamingBranch(null);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  function handleContextMenu(e: React.MouseEvent, branch: RefEntry) {
    e.preventDefault();
    setContextMenu({ x: e.clientX, y: e.clientY, branch });
  }

  // ── Fetch all remotes ──────────────────────────────────────────

  async function handleFetchAll() {
    setFetchingAll(true);
    setError(null);
    setFetchedRemotes(null);
    try {
      await rpc.gitFetchAll(repo.repo_id);
      // Derive remote names from the remote branches in the refreshed refs list
      const freshRefs = await rpc.listRefs(repo.repo_id);
      setRefs(freshRefs.refs);
      const remoteNames = new Set<string>();
      for (const ref of freshRefs.refs) {
        if (ref.kind === "remote") {
          const firstSeg = ref.name.split("/")[0];
          if (firstSeg) remoteNames.add(firstSeg);
        }
      }
      setFetchedRemotes(Array.from(remoteNames));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setFetchingAll(false);
    }
  }

  // Auto-dismiss fetched-remotes chips after 5 seconds
  useEffect(() => {
    if (!fetchedRemotes) return;
    const h = setTimeout(() => setFetchedRemotes(null), 5000);
    return () => clearTimeout(h);
  }, [fetchedRemotes]);

  // Auto-dismiss post-push PR prompt after 8 seconds
  useEffect(() => {
    if (!postPushBranch) return;
    const h = setTimeout(() => setPostPushBranch(null), 8000);
    return () => clearTimeout(h);
  }, [postPushBranch]);

  // ── CommandContext for routing actions through executeCommand ───

  const commandContext = useMemo<CommandContext>(
    () => ({
      repoId: repo.repo_id,
      currentBranch: headBranch?.name ?? "",
      upstream: headBranch?.upstream ?? null,
      ahead: headBranch?.ahead ?? 0,
      behind: headBranch?.behind ?? 0,
      dirtyCount: 0,
      hasStagedChanges: false,
      activeOperation: null,
      forgeProvider: null,
    }),
    [repo.repo_id, headBranch],
  );

  // ── ViewToolbar actions ────────────────────────────────────────

  const toolbarActions = useMemo<ToolbarAction[]>(
    () => [
      { id: "git.branch.create", icon: Plus, label: "Create Branch" },
    ],
    [],
  );

  // ── Create branch inline prompt ──────────────────────────────────
  const [creatingBranch, setCreatingBranch] = useState(false);
  const [createBranchName, setCreateBranchName] = useState("");
  const createBranchInputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (creatingBranch) {
      // Focus the input after render
      setTimeout(() => createBranchInputRef.current?.focus(), 0);
    }
  }, [creatingBranch]);

  async function handleCreateBranchSubmit() {
    const name = createBranchName.trim();
    if (!name) {
      setCreatingBranch(false);
      setCreateBranchName("");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await rpc.gitBranchCreate(repo.repo_id, name);
      setCreatingBranch(false);
      setCreateBranchName("");
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  const handleToolbarAction = useCallback(
    (actionId: string) => {
      if (actionId === "git.branch.create") {
        setCreatingBranch(true);
        setCreateBranchName("");
      } else {
        void executeCommand(actionId, commandContext);
      }
    },
    [commandContext],
  );

  // ── Context menu items (reusable ContextMenu component) ────────

  const branchContextMenuItems = useMemo<MenuItemDef[]>(() => {
    if (!contextMenu) return [];
    const isCurrentBranch = contextMenu.branch.name === headBranch?.name;
    return [
      { id: "git.checkout", label: "Checkout", icon: GitBranch },
      { id: "git.branch.merge", label: "Merge into Current Branch", icon: GitMerge },
      { id: "git.rebase", label: "Rebase onto", icon: GitPullRequestArrow },
      { id: "git.branch.rename", label: "Rename", icon: Pencil, separator: true },
      { id: "git.branch.delete", label: "Delete", icon: Trash2, danger: true, disabled: isCurrentBranch },
    ];
  }, [contextMenu, headBranch]);

  const handleContextMenuSelect = useCallback(
    (actionId: string) => {
      if (!contextMenu) return;
      const branchName = contextMenu.branch.name;

      // Guard: cannot delete the current branch
      if (actionId === "git.branch.delete" && branchName === headBranch?.name) {
        publishToast({
          kind: "error",
          title: "Cannot delete active branch",
          body: `"${branchName}" is currently checked out.`,
        });
        setContextMenu(null);
        return;
      }

      // Route rename through the inline rename flow
      if (actionId === "git.branch.rename") {
        setRenamingBranch(branchName);
        setRenameValue(branchName);
        setContextMenu(null);
        return;
      }

      // Route merge through the MergeDialog
      if (actionId === "git.branch.merge") {
        setMergeBranch(branchName);
        setContextMenu(null);
        return;
      }

      // Route rebase through the RebaseDialog
      if (actionId === "git.rebase") {
        setRebaseBranch(branchName);
        setContextMenu(null);
        return;
      }

      // Route delete — try normal delete first, offer force on failure
      if (actionId === "git.branch.delete") {
        setContextMenu(null);
        setBusy(true);
        setError(null);
        rpc.gitBranchDelete(repo.repo_id, branchName)
          .then(() => refresh())
          .catch((e: unknown) => {
            const msg = e instanceof Error ? e.message : String(e);
            // If the branch is not fully merged, offer force delete
            if (msg.includes("not fully merged") || msg.includes("not yet merged")) {
              setForceDeleteBranch(branchName);
            } else {
              setError(msg);
            }
          })
          .finally(() => setBusy(false));
        return;
      }

      // Checkout and other commands go through executeCommand
      void executeCommand(actionId, commandContext, branchName).then(() => {
        refresh();
      });
      setContextMenu(null);
    },
    [contextMenu, commandContext, headBranch, refresh, repo.repo_id],
  );

  // ── Double-click handler for checkout ──────────────────────────

  const handleBranchDoubleClick = useCallback(
    (branch: RefEntry) => {
      void executeCommand("git.checkout", commandContext, branch.name).then(() => {
        refresh();
      });
    },
    [commandContext, refresh],
  );

  // ── Force push ─────────────────────────────────────────────────

  async function handleForcePushConfirm(branchName: string) {
    setBusy(true);
    setError(null);
    setForcePushBranch(null);
    try {
      await rpc.gitPushRemote(repo.repo_id, branchName, { force: true });
      setToast({ kind: "info", text: `Force pushed ${branchName}` });
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  // ── Partition refs ─────────────────────────────────────────────

  const localBranches = (refs ?? []).filter((r) => r.kind === "branch");
  const remoteBranches = (refs ?? []).filter((r) => r.kind === "remote");
  const tags = (refs ?? []).filter((r) => r.kind === "tag");

  const localTree = buildTree(localBranches);
  const remoteTree = buildTree(remoteBranches);

  return (
    <>
      {/* ViewToolbar with Create Branch button */}
      <ViewToolbar
        title="Branches"
        icon={GitBranch}
        actions={toolbarActions}
        onAction={handleToolbarAction}
        trailing={
          <span className="flex items-center gap-[4px]">
            <button
              type="button"
              className="flex h-6 w-6 items-center justify-center rounded text-gx-secondary hover:bg-gx-accent/20 hover:text-gx-primary disabled:cursor-not-allowed disabled:opacity-50"
              onClick={handleFetchAll}
              disabled={fetchingAll}
              title="Fetch all remotes"
              aria-label="Fetch all remotes"
            >
              {fetchingAll ? <Loader2 size={14} className="animate-spin" /> : <Download size={14} />}
            </button>
            <button
              type="button"
              className="flex h-6 w-6 items-center justify-center rounded text-gx-secondary hover:bg-gx-accent/20 hover:text-gx-primary"
              onClick={refresh}
              title="Refresh"
              aria-label="Refresh branches"
            >
              <RefreshCw size={14} />
            </button>
          </span>
        }
      />

      {/* Create branch inline input */}
      {creatingBranch && (
        <div className="shrink-0 border-b border-gx-border px-3 py-2">
          <div className="flex items-center gap-2">
            <GitBranch size={12} className="shrink-0 text-gx-accent" />
            <input
              ref={createBranchInputRef}
              className="min-w-0 flex-1 rounded border border-gx-accent bg-gx-bg px-2 py-1 text-xs text-gx-text placeholder:text-gx-muted focus:outline-none focus:ring-1 focus:ring-gx-accent"
              placeholder="New branch name…"
              value={createBranchName}
              onChange={(e) => setCreateBranchName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") void handleCreateBranchSubmit();
                if (e.key === "Escape") {
                  setCreatingBranch(false);
                  setCreateBranchName("");
                }
              }}
              onBlur={() => {
                // Delay to allow Enter to fire first
                setTimeout(() => {
                  setCreatingBranch(false);
                  setCreateBranchName("");
                }, 150);
              }}
              disabled={busy}
            />
          </div>
        </div>
      )}
      {fetchedRemotes && fetchedRemotes.length > 0 && (
        <div className="shrink-0 border-b border-gx-border px-3 py-1">
          <div className="flex flex-wrap gap-1">
            {fetchedRemotes.map((remote) => (
              <span
                key={remote}
                className="rounded bg-gx-accent/15 px-1.5 py-0.5 text-[10px] text-gx-accent"
              >
                {remote}
              </span>
            ))}
          </div>
        </div>
      )}

      {/* Body */}
      <div ref={scrollRef} className="min-h-0 flex-1 overflow-auto">
        {loading && (
          <div className="flex items-center gap-2 p-3 text-xs text-gx-muted">
            <Loader2 size={12} className="animate-spin" /> Loading branches…
          </div>
        )}
        {error && (
          <div className="m-3 rounded border border-gx-error/40 bg-gx-error/10 p-3 text-xs text-gx-error">
            {error}
          </div>
        )}
        {!loading && refs && (
          <div className="py-1">
            <TreeSection
              title="Local"
              node={localTree}
              selectedBranch={selectedBranch ?? null}
              onSelectBranch={(name) => onSelectBranch?.(name)}
              onContextMenu={handleContextMenu}
              onDoubleClick={handleBranchDoubleClick}
              renamingBranch={renamingBranch}
              renameValue={renameValue}
              onRenameChange={setRenameValue}
              onRenameSubmit={handleRenameSubmit}
              onRenameCancel={() => setRenamingBranch(null)}
              busy={busy}
              repoId={repo.repo_id}
              onRefresh={refresh}
              onCreatePr={(name) => setCreatePrBranch(name)}
              onPushSuccess={(name) => setPostPushBranch(name)}
            />
            {remoteBranches.length > 0 && (
              <TreeSection
                title="Remote"
                node={remoteTree}
                selectedBranch={selectedBranch ?? null}
                onSelectBranch={(name) => onSelectBranch?.(name)}
                onContextMenu={handleContextMenu}
                onDoubleClick={handleBranchDoubleClick}
                renamingBranch={renamingBranch}
                renameValue={renameValue}
                onRenameChange={setRenameValue}
                onRenameSubmit={handleRenameSubmit}
                onRenameCancel={() => setRenamingBranch(null)}
                busy={busy}
                repoId={repo.repo_id}
                onRefresh={refresh}
                onCreatePr={(name) => setCreatePrBranch(name)}
              />
            )}
            {tags.length > 0 && (
              <TagSection tags={tags} />
            )}
          </div>
        )}
      </div>

      {/* Context menu (reusable ContextMenu component routed through executeCommand) */}
      {contextMenu && (
        <ContextMenu
          items={branchContextMenuItems}
          position={{ x: contextMenu.x, y: contextMenu.y }}
          onSelect={handleContextMenuSelect}
          onClose={() => setContextMenu(null)}
        />
      )}

      {/* Force delete confirmation dialog */}
      {forceDeleteBranch && (
        <ConfirmForceDeleteDialog
          branchName={forceDeleteBranch}
          onConfirm={() => handleForceDelete(forceDeleteBranch)}
          onCancel={() => setForceDeleteBranch(null)}
        />
      )}

      {/* Force push confirmation dialog */}
      {forcePushBranch && (
        <ConfirmForcePushDialog
          branchName={forcePushBranch}
          onConfirm={() => handleForcePushConfirm(forcePushBranch)}
          onCancel={() => setForcePushBranch(null)}
        />
      )}

      {/* Merge dialog */}
      {mergeBranch && headBranch && (
        <MergeDialog
          sourceBranch={mergeBranch}
          targetBranch={headBranch.name}
          repoId={repo.repo_id}
          onSuccess={() => {
            setMergeBranch(null);
            setToast({ kind: "info", text: `Merged ${mergeBranch} into ${headBranch.name}` });
            refresh();
          }}
          onConflicts={(count) => {
            setMergeBranch(null);
            setToast({
              kind: "error",
              text: `Merge paused — conflicts in ${count} file${count === 1 ? "" : "s"}`,
            });
          }}
          onCancel={() => setMergeBranch(null)}
        />
      )}

      {/* Rebase dialog */}
      {rebaseBranch && headBranch && (
        <RebaseDialog
          branch={rebaseBranch}
          ontoBranch={headBranch.name}
          repoId={repo.repo_id}
          onSuccess={() => {
            setRebaseBranch(null);
            setToast({ kind: "info", text: `Rebased ${rebaseBranch} onto ${headBranch.name}` });
            refresh();
          }}
          onConflicts={() => {
            setRebaseBranch(null);
            setToast({
              kind: "error",
              text: `Rebase paused — conflicts detected`,
            });
          }}
          onCancel={() => setRebaseBranch(null)}
        />
      )}

      {/* Toast */}
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

      {/* Post-push "Create PR?" prompt */}
      {postPushBranch && (
        <div className="pointer-events-none fixed inset-x-0 bottom-4 z-50 flex justify-center">
          <div className="pointer-events-auto flex items-center gap-2 rounded border border-gx-accent/40 bg-gx-surface px-3 py-1.5 text-xs shadow-lg">
            <GitPullRequest size={12} className="text-gx-accent" />
            <span className="text-gx-text">Branch pushed. Create a pull request?</span>
            <button
              type="button"
              className="rounded bg-gx-accent px-2 py-0.5 text-[11px] font-medium text-white hover:bg-gx-accent/90"
              onClick={() => {
                setCreatePrBranch(postPushBranch);
                setPostPushBranch(null);
              }}
            >
              Create PR
            </button>
            <button
              type="button"
              className="text-gx-muted hover:text-gx-text"
              onClick={() => setPostPushBranch(null)}
            >
              dismiss
            </button>
          </div>
        </div>
      )}

      {/* Create PR dialog */}
      {createPrBranch && (
        <CreatePrDialog
          repoId={repo.repo_id}
          branchName={createPrBranch}
          localBranches={localBranches.map((b) => b.name)}
          onSuccess={(pr) => {
            setCreatePrBranch(null);
            setToast({ kind: "info", text: `PR #${pr.number} created` });
          }}
          onCancel={() => setCreatePrBranch(null)}
        />
      )}
    </>
  );
}


// ── Tree section (collapsible group) ───────────────────────────────

interface TreeSectionProps {
  title: string;
  node: BranchTreeNode;
  selectedBranch: string | null;
  onSelectBranch: (name: string) => void;
  onContextMenu: (e: React.MouseEvent, branch: RefEntry) => void;
  onDoubleClick: (branch: RefEntry) => void;
  renamingBranch: string | null;
  renameValue: string;
  onRenameChange: (v: string) => void;
  onRenameSubmit: (oldName: string) => void;
  onRenameCancel: () => void;
  busy: boolean;
  repoId: string;
  onRefresh: () => void;
  onCreatePr: (branchName: string) => void;
  onPushSuccess?: (branchName: string) => void;
}

function TreeSection({
  title,
  node,
  selectedBranch,
  onSelectBranch,
  onContextMenu,
  onDoubleClick,
  renamingBranch,
  renameValue,
  onRenameChange,
  onRenameSubmit,
  onRenameCancel,
  busy,
  repoId,
  onRefresh,
  onCreatePr,
  onPushSuccess,
}: TreeSectionProps) {
  const [open, setOpen] = useState(true);
  const count = countBranches(node);

  return (
    <div className="mb-1">
      <button
        type="button"
        className="flex w-full items-center gap-1 px-3 py-1 text-[11px] uppercase tracking-wider text-gx-muted hover:text-gx-text"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
      >
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        {title}
        <span className="ml-auto text-[10px]">({count})</span>
      </button>
      {open && (
        <div role="listbox" aria-label={`${title} branches`}>
          <TreeNodeList
            node={node}
            depth={0}
            selectedBranch={selectedBranch ?? null}
            onSelectBranch={(name) => onSelectBranch?.(name)}
            onContextMenu={onContextMenu}
            onDoubleClick={onDoubleClick}
            renamingBranch={renamingBranch}
            renameValue={renameValue}
            onRenameChange={onRenameChange}
            onRenameSubmit={onRenameSubmit}
            onRenameCancel={onRenameCancel}
            busy={busy}
            repoId={repoId}
            onRefresh={onRefresh}
            onCreatePr={onCreatePr}
            onPushSuccess={onPushSuccess}
          />
        </div>
      )}
    </div>
  );
}

function countBranches(node: BranchTreeNode): number {
  let n = node.branches.length;
  for (const child of node.children) n += countBranches(child);
  return n;
}

// ── Recursive tree renderer ────────────────────────────────────────

interface TreeNodeListProps {
  node: BranchTreeNode;
  depth: number;
  selectedBranch: string | null;
  onSelectBranch: (name: string) => void;
  onContextMenu: (e: React.MouseEvent, branch: RefEntry) => void;
  onDoubleClick: (branch: RefEntry) => void;
  renamingBranch: string | null;
  renameValue: string;
  onRenameChange: (v: string) => void;
  onRenameSubmit: (oldName: string) => void;
  onRenameCancel: () => void;
  busy: boolean;
  repoId: string;
  onRefresh: () => void;
  onCreatePr: (branchName: string) => void;
  onPushSuccess?: (branchName: string) => void;
}

function TreeNodeList({
  node,
  depth,
  selectedBranch,
  onSelectBranch,
  onContextMenu,
  onDoubleClick,
  renamingBranch,
  renameValue,
  onRenameChange,
  onRenameSubmit,
  onRenameCancel,
  busy,
  repoId,
  onRefresh,
  onCreatePr,
  onPushSuccess,
}: TreeNodeListProps) {
  return (
    <div>
      {node.children.map((child) => (
        <FolderNode
          key={child.prefix}
          node={child}
          depth={depth}
          selectedBranch={selectedBranch}
          onSelectBranch={onSelectBranch}
          onContextMenu={onContextMenu}
          onDoubleClick={onDoubleClick}
          renamingBranch={renamingBranch}
          renameValue={renameValue}
          onRenameChange={onRenameChange}
          onRenameSubmit={onRenameSubmit}
          onRenameCancel={onRenameCancel}
          busy={busy}
          repoId={repoId}
          onRefresh={onRefresh}
          onCreatePr={onCreatePr}
          onPushSuccess={onPushSuccess}
        />
      ))}
      {node.branches.map((branch) => (
        <BranchRow
          key={branch.name}
          branch={branch}
          depth={depth}
          isSelected={selectedBranch === branch.name}
          onSelect={() => onSelectBranch?.(branch.name)}
          onDoubleClick={() => onDoubleClick(branch)}
          onContextMenu={onContextMenu}
          renamingBranch={renamingBranch}
          renameValue={renameValue}
          onRenameChange={onRenameChange}
          onRenameSubmit={onRenameSubmit}
          onRenameCancel={onRenameCancel}
          busy={busy}
          repoId={repoId}
          onRefresh={onRefresh}
          onCreatePr={onCreatePr}
          onPushSuccess={onPushSuccess}
        />
      ))}
    </div>
  );
}

// ── Folder node (collapsible prefix group) ─────────────────────────

interface FolderNodeProps {
  node: BranchTreeNode;
  depth: number;
  selectedBranch: string | null;
  onSelectBranch: (name: string) => void;
  onContextMenu: (e: React.MouseEvent, branch: RefEntry) => void;
  onDoubleClick: (branch: RefEntry) => void;
  renamingBranch: string | null;
  renameValue: string;
  onRenameChange: (v: string) => void;
  onRenameSubmit: (oldName: string) => void;
  onRenameCancel: () => void;
  busy: boolean;
  repoId: string;
  onRefresh: () => void;
  onCreatePr: (branchName: string) => void;
  onPushSuccess?: (branchName: string) => void;
}

function FolderNode({
  node,
  depth,
  selectedBranch,
  onSelectBranch,
  onContextMenu,
  onDoubleClick,
  renamingBranch,
  renameValue,
  onRenameChange,
  onRenameSubmit,
  onRenameCancel,
  busy,
  repoId,
  onRefresh,
  onCreatePr,
  onPushSuccess,
}: FolderNodeProps) {
  const [open, setOpen] = useState(true);
  const paddingLeft = 12 + depth * 12;

  return (
    <div>
      <button
        type="button"
        className="flex w-full items-center gap-1 py-0.5 text-[12px] text-gx-muted hover:bg-gx-bg-sub hover:text-gx-text"
        style={{ paddingLeft }}
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
      >
        {open ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
        <span className="font-mono text-[11px]">{node.segment}/</span>
      </button>
      {open && (
        <TreeNodeList
          node={node}
          depth={depth + 1}
          selectedBranch={selectedBranch}
          onSelectBranch={onSelectBranch}
          onContextMenu={onContextMenu}
          onDoubleClick={onDoubleClick}
          renamingBranch={renamingBranch}
          renameValue={renameValue}
          onRenameChange={onRenameChange}
          onRenameSubmit={onRenameSubmit}
          onRenameCancel={onRenameCancel}
          busy={busy}
          repoId={repoId}
          onRefresh={onRefresh}
          onCreatePr={onCreatePr}
          onPushSuccess={onPushSuccess}
        />
      )}
    </div>
  );
}

// ── Branch row ─────────────────────────────────────────────────────

interface BranchRowProps {
  branch: RefEntry;
  depth: number;
  isSelected: boolean;
  onSelect: () => void;
  onDoubleClick: () => void;
  onContextMenu: (e: React.MouseEvent, branch: RefEntry) => void;
  renamingBranch: string | null;
  renameValue: string;
  onRenameChange: (v: string) => void;
  onRenameSubmit: (oldName: string) => void;
  onRenameCancel: () => void;
  busy: boolean;
  repoId: string;
  onRefresh: () => void;
  onCreatePr: (branchName: string) => void;
  onPushSuccess?: (branchName: string) => void;
}

function BranchRow({
  branch,
  depth,
  isSelected,
  onSelect,
  onDoubleClick,
  onContextMenu,
  renamingBranch,
  renameValue,
  onRenameChange,
  onRenameSubmit,
  onRenameCancel,
  busy,
  repoId,
  onRefresh,
  onCreatePr,
  onPushSuccess,
}: BranchRowProps) {
  const renameInputRef = useRef<HTMLInputElement>(null);
  const paddingLeft = 12 + depth * 12 + 16; // extra indent past folder chevron
  const isRenaming = renamingBranch === branch.name;
  const leaf = leafName(branch.name);

  const [actionBusy, setActionBusy] = useState<"push" | "pull" | "fetch" | null>(null);
  const [pullModeOpen, setPullModeOpen] = useState(false);

  const hasUpstream = !!branch.upstream;
  const ahead = branch.ahead ?? 0;
  const behind = branch.behind ?? 0;

  async function handlePush(e: React.MouseEvent) {
    e.stopPropagation();
    setActionBusy("push");
    try {
      await rpc.gitPushBranch(repoId, branch.name);
      onRefresh();
      onPushSuccess?.(branch.name);
    } catch {
      // Error handling is minimal — the parent error state will catch
      // persistent issues on the next refresh.
    } finally {
      setActionBusy(null);
    }
  }

  async function handlePull(e: React.MouseEvent) {
    e.stopPropagation();
    setPullModeOpen(true);
  }

  async function handlePullWithMode(mode: PullMode) {
    setPullModeOpen(false);
    setStoredPullMode(repoId, branch.name, mode);
    setActionBusy("pull");
    try {
      await rpc.gitPullMode(repoId, { mode });
      onRefresh();
    } catch {
      // Swallow — refresh will show updated state.
    } finally {
      setActionBusy(null);
    }
  }

  async function handleFetch(e: React.MouseEvent) {
    e.stopPropagation();
    setActionBusy("fetch");
    try {
      await rpc.gitFetchBranch(repoId, branch.name);
      onRefresh();
    } catch {
      // Swallow — refresh will show updated state.
    } finally {
      setActionBusy(null);
    }
  }

  useEffect(() => {
    if (isRenaming) renameInputRef.current?.focus();
  }, [isRenaming]);

  if (isRenaming) {
    return (
      <div
        className="flex items-center gap-1 py-0.5"
        style={{ paddingLeft }}
      >
        <input
          ref={renameInputRef}
          className="min-w-0 flex-1 rounded border border-gx-accent bg-gx-bg-sub px-1 py-0.5 font-mono text-[11px] focus-visible:outline-none"
          value={renameValue}
          onChange={(e) => onRenameChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") onRenameSubmit(branch.name);
            if (e.key === "Escape") onRenameCancel();
          }}
          onBlur={() => onRenameCancel()}
          disabled={busy}
        />
      </div>
    );
  }

  return (
    <div
      className={clsx(
        "group flex items-center gap-1 py-0.5 text-[12px]",
        isSelected ? "bg-gx-accent/15" : "hover:bg-gx-bg-sub",
      )}
      style={{ paddingLeft }}
      onClick={onSelect}
      onDoubleClick={onDoubleClick}
      onContextMenu={(e) => onContextMenu(e, branch)}
      role="option"
      aria-selected={isSelected}
    >
      {branch.is_head && (
        <span className="shrink-0 text-gx-accent" title="HEAD">*</span>
      )}
      <span
        className={`min-w-0 flex-1 truncate font-mono text-[11px] ${
          branch.is_head ? "font-bold text-gx-text" : "text-gx-text"
        }`}
        title={branch.name}
      >
        {leaf}
      </span>

      {/* Hover-revealed action buttons */}
      {hasUpstream && (
        <span className="flex shrink-0 items-center gap-0.5 opacity-0 transition-opacity group-hover:opacity-100">
          {ahead > 0 && (
            <button
              type="button"
              className="rounded p-0.5 text-gx-accent hover:bg-gx-accent/20 disabled:opacity-50"
              onClick={handlePush}
              disabled={actionBusy !== null}
              title={`Push ${branch.name}`}
              aria-label={`Push ${branch.name}`}
            >
              {actionBusy === "push" ? <Loader2 size={11} className="animate-spin" /> : <ArrowUp size={11} />}
            </button>
          )}
          {behind > 0 && (
            <button
              type="button"
              className="rounded p-0.5 text-gx-warning hover:bg-gx-warning/20 disabled:opacity-50"
              onClick={handlePull}
              disabled={actionBusy !== null}
              title={`Pull ${branch.name}`}
              aria-label={`Pull ${branch.name}`}
            >
              {actionBusy === "pull" ? <Loader2 size={11} className="animate-spin" /> : <ArrowDown size={11} />}
            </button>
          )}
          {pullModeOpen && (
            <PullModeDropdown
              repoId={repoId}
              branchName={branch.name}
              onSelect={handlePullWithMode}
              onClose={() => setPullModeOpen(false)}
            />
          )}
          <button
            type="button"
            className="rounded p-0.5 text-gx-muted hover:bg-gx-surface hover:text-gx-text disabled:opacity-50"
            onClick={handleFetch}
            disabled={actionBusy !== null}
            title={`Fetch ${branch.name}`}
            aria-label={`Fetch ${branch.name}`}
          >
            {actionBusy === "fetch" ? <Loader2 size={11} className="animate-spin" /> : <RefreshCw size={11} />}
          </button>
          <button
            type="button"
            className="rounded p-0.5 text-gx-muted hover:bg-gx-accent/80 hover:text-gx-accent disabled:opacity-50"
            onClick={(e) => { e.stopPropagation(); onCreatePr(branch.name); }}
            disabled={actionBusy !== null}
            title={`Create PR for ${branch.name}`}
            aria-label={`Create pull request for ${branch.name}`}
          >
            <GitPullRequest size={11} />
          </button>
        </span>
      )}

      <AheadBehindChips branch={branch} />
      <KindBadge kind={branch.kind} />
    </div>
  );
}

// ── Pull mode dropdown ──────────────────────────────────────────────

interface PullModeDropdownProps {
  repoId: string;
  branchName: string;
  onSelect: (mode: PullMode) => void;
  onClose: () => void;
}

function PullModeDropdown({ repoId, branchName, onSelect, onClose }: PullModeDropdownProps) {
  const ref = useRef<HTMLDivElement>(null);
  const currentMode = getStoredPullMode(repoId, branchName);

  useEffect(() => {
    const dismiss = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("mousedown", dismiss);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", dismiss);
      window.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  const modes: { value: PullMode; label: string }[] = [
    { value: "merge", label: "Merge" },
    { value: "rebase", label: "Rebase" },
    { value: "ff-only", label: "Fast-forward only" },
  ];

  return (
    <div
      ref={ref}
      className="absolute z-50 mt-1 min-w-[140px] rounded border border-gx-border bg-gx-surface py-1 shadow-lg"
      role="menu"
      onClick={(e) => e.stopPropagation()}
    >
      <div className="px-2 py-1 text-[10px] uppercase tracking-wider text-gx-muted">
        Pull mode
      </div>
      {modes.map((m) => (
        <button
          key={m.value}
          type="button"
          className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-[12px] hover:bg-gx-accent/10 ${
            currentMode === m.value ? "text-gx-accent font-medium" : "text-gx-text"
          }`}
          onClick={() => onSelect(m.value)}
          role="menuitem"
        >
          {currentMode === m.value && <span className="text-gx-accent">•</span>}
          {currentMode !== m.value && <span className="invisible">•</span>}
          {m.label}
        </button>
      ))}
    </div>
  );
}

// ── Ahead/behind chips ─────────────────────────────────────────────

function AheadBehindChips({ branch }: { branch: RefEntry }) {
  if (!branch.upstream) return null;
  const ahead = branch.ahead ?? 0;
  const behind = branch.behind ?? 0;
  if (ahead === 0 && behind === 0) return null;

  return (
    <span className="flex shrink-0 items-center gap-0.5 text-[10px]">
      {ahead > 0 && (
        <span className="rounded bg-gx-accent/20 px-1 text-gx-accent" title={`${ahead} ahead`}>
          ↑{ahead}
        </span>
      )}
      {behind > 0 && (
        <span className="rounded bg-gx-warning/20 px-1 text-gx-warning" title={`${behind} behind`}>
          ↓{behind}
        </span>
      )}
    </span>
  );
}

// ── Kind badge ─────────────────────────────────────────────────────

function KindBadge({ kind }: { kind: RefKind }) {
  if (kind === "branch") return null; // default, no badge needed in local section
  const label = kind === "remote" ? "R" : kind === "tag" ? "T" : kind;
  return (
    <span className="shrink-0 rounded bg-gx-surface px-1 text-[9px] uppercase text-gx-muted">
      {label}
    </span>
  );
}

// ── Tag section (flat list, no tree) ───────────────────────────────

function TagSection({ tags }: { tags: RefEntry[] }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="mb-1">
      <button
        type="button"
        className="flex w-full items-center gap-1 px-3 py-1 text-[11px] uppercase tracking-wider text-gx-muted hover:text-gx-text"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
      >
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        Tags
        <span className="ml-auto text-[10px]">({tags.length})</span>
      </button>
      {open && (
        <div>
          {tags.map((t) => (
            <div
              key={t.name}
              className="flex items-center gap-1 py-0.5 pl-6 text-[12px] hover:bg-gx-bg-sub"
            >
              <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-gx-text">
                {t.name}
              </span>
              <span className="shrink-0 font-mono text-[10px] text-gx-muted">
                {t.target.slice(0, 8)}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
