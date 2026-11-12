import { Button } from "@/components/ui/button";
import { requestPrompt } from "@/lib/application-dialogs";
import clsx from "clsx";
import {
  Check,
  ChevronDown,
  ChevronRight,
  ExternalLink,
  GitBranch,
  GitMerge,
  GitPullRequest,
  Loader2,
  Plus,
  RefreshCw,
  Settings,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import { ContextMenu, MenuItemDef } from "../components/ContextMenu";
import { ToolbarAction, ViewToolbar } from "../components/ViewToolbar";
import { useSessionScroll } from "../hooks/useSessionScroll";
import { PullRequestEntry, PullRequestStatus, rpc } from "../ipc";
import { CommandContext, executeCommand } from "../lib/command-registry";
import { OpenRepo } from "../state";

interface Props {
  repo: OpenRepo;
  onNavigateToSettings?: () => void;
  onSelectPr?: (prNumber: number) => void;
}

// ---- Status chip styles ----

const STATUS_STYLES: Record<PullRequestStatus, { bg: string; text: string; label: string; icon?: boolean }> = {
  draft: { bg: "bg-accent/20", text: "text-muted-foreground", label: "Draft" },
  open: { bg: "bg-success/20", text: "text-success", label: "Open" },
  approved: { bg: "bg-success/20", text: "text-success", label: "Approved", icon: true },
  changes_requested: { bg: "bg-warning/20", text: "text-warning", label: "Changes Requested" },
  merged: { bg: "bg-thinking/20", text: "text-thinking", label: "Merged" },
};

function PrStatusChip({ status }: { status: PullRequestStatus }) {
  const style = STATUS_STYLES[status] ?? STATUS_STYLES.open;
  return (
    <span
      className={clsx(
        "inline-flex items-center gap-1 rounded-full px-1.5 py-[1px] text-[10px] font-medium",
        style.bg,
        style.text,
      )}
    >
      {style.icon && <Check size={10} />}
      {style.label}
    </span>
  );
}

// ---- Helpers ----

function formatTime(iso: string): string {
  try {
    const d = new Date(iso);
    const now = Date.now();
    const diffMs = now - d.getTime();
    if (diffMs < 60_000) return "just now";
    if (diffMs < 3_600_000) return `${Math.floor(diffMs / 60_000)}m ago`;
    if (diffMs < 86_400_000) return `${Math.floor(diffMs / 3_600_000)}h ago`;
    if (diffMs < 604_800_000) return `${Math.floor(diffMs / 86_400_000)}d ago`;
    return d.toLocaleDateString();
  } catch {
    return iso;
  }
}

type PrGroup = "assigned" | "created" | "all";

function groupPrs(prs: PullRequestEntry[]): Record<PrGroup, PullRequestEntry[]> {
  const assigned: PullRequestEntry[] = [];
  const created: PullRequestEntry[] = [];
  const all: PullRequestEntry[] = [];

  for (const pr of prs) {
    all.push(pr);
    if (pr.is_assigned_to_me) assigned.push(pr);
    if (pr.is_mine) created.push(pr);
  }

  return { assigned, created, all };
}

// ---- Section component ----

interface SectionProps {
  title: string;
  prs: PullRequestEntry[];
  defaultOpen?: boolean;
  onSelectPr?: (prNumber: number) => void;
  onContextMenu?: (e: React.MouseEvent, pr: PullRequestEntry) => void;
}

function PrSection({ title, prs, defaultOpen = true, onSelectPr, onContextMenu }: SectionProps) {
  const [open, setOpen] = useState(defaultOpen);

  return (
    <div className="mb-1">
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="justify-start flex w-full items-center gap-1 px-3 py-1.5 text-left text-[11px] font-medium uppercase tracking-wider text-muted-foreground hover:text-foreground"
        aria-expanded={open}
      >
        {open ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
        {title}
        <span className="ml-auto text-[10px] tabular-nums">{prs.length}</span>
      </Button>
      {open && (
        <ul role="listbox" aria-label={title} className="space-y-0.5 px-2">
          {prs.length === 0 && (
            <li className="px-2 py-2 text-center text-[11px] text-muted-foreground">
              No pull requests
            </li>
          )}
          {prs.map((pr) => (
            <PrRow key={pr.number} pr={pr} onSelect={onSelectPr} onContextMenu={onContextMenu} />
          ))}
        </ul>
      )}
    </div>
  );
}

// ---- PR row ----

function PrRow({ pr, onSelect, onContextMenu }: { pr: PullRequestEntry; onSelect?: (prNumber: number) => void; onContextMenu?: (e: React.MouseEvent, pr: PullRequestEntry) => void }) {
  return (
    <li
      className="group cursor-pointer rounded px-2 py-1.5 hover:bg-accent"
      onClick={() => onSelect?.(pr.number)}
      onContextMenu={(e) => {
        e.preventDefault();
        onContextMenu?.(e, pr);
      }}
      role="option"
      tabIndex={0}
      onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") onSelect?.(pr.number); }}
    >
      <div className="flex items-start gap-2">
        <GitPullRequest size={14} className="mt-0.5 shrink-0 text-muted-foreground" />
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-1.5">
            <span className="truncate text-sm text-foreground">{pr.title}</span>
            <PrStatusChip status={pr.status} />
          </div>
          <div className="mt-0.5 flex items-center gap-2 text-[11px] text-muted-foreground">
            <span className="font-mono">#{pr.number}</span>
            <span>{pr.author}</span>
            <span className="ml-auto">{formatTime(pr.updated_at)}</span>
          </div>
          <div className="mt-0.5 flex items-center gap-1 text-[10px] text-muted-foreground">
            <span className="truncate rounded bg-card px-1 py-[1px] font-mono">
              {pr.source_branch}
            </span>
            <span>→</span>
            <span className="truncate rounded bg-card px-1 py-[1px] font-mono">
              {pr.target_branch}
            </span>
          </div>
        </div>
      </div>
    </li>
  );
}

// ---- Context menu state ----

interface PrContextMenuState {
  x: number;
  y: number;
  pr: PullRequestEntry;
}

// ---- Main view ----

export function PullRequestsView({ repo, onNavigateToSettings, onSelectPr }: Props) {
  const scrollRef = useSessionScroll("pull-requests");
  const [prs, setPrs] = useState<PullRequestEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [contextMenu, setContextMenu] = useState<PrContextMenuState | null>(null);

  const fetchPrs = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await rpc.forgeListPrs(repo.repo_id);
      setPrs(result.prs);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setError(msg);
    } finally {
      setLoading(false);
    }
  }, [repo.repo_id]);

  useEffect(() => {
    void fetchPrs();
  }, [fetchPrs]);

  // ── CommandContext for routing actions through executeCommand ───

  // Fetch current branch name for PR creation (head branch).
  const [currentBranch, setCurrentBranch] = useState("");
  useEffect(() => {
    rpc.listRefs(repo.repo_id, { kinds: ["branch"] }).then((r) => {
      const head = r.refs.find((ref) => ref.is_head && ref.kind === "branch");
      if (head) setCurrentBranch(head.name);
    }).catch(() => {});
  }, [repo.repo_id]);

  const commandContext = useMemo<CommandContext>(
    () => ({
      repoId: repo.repo_id,
      currentBranch,
      upstream: null,
      ahead: 0,
      behind: 0,
      dirtyCount: 0,
      hasStagedChanges: false,
      activeOperation: null,
      forgeProvider: repo.forge_provider ?? null,
    }),
    [repo.repo_id, repo.forge_provider, currentBranch],
  );

  // ── ViewToolbar actions ────────────────────────────────────────

  const toolbarActions = useMemo<ToolbarAction[]>(
    () => [
      { id: "pr.create", icon: Plus, label: "Create PR" },
    ],
    [],
  );

  const handleToolbarAction = useCallback(
    async (actionId: string) => {
      if (actionId === "pr.create") {
        const title = await requestPrompt("Pull request title:");
        if (!title?.trim()) return;
        void executeCommand(actionId, commandContext, title.trim());
      } else {
        void executeCommand(actionId, commandContext);
      }
    },
    [commandContext],
  );

  // ── Context menu ───────────────────────────────────────────────

  const handleContextMenu = useCallback(
    (e: React.MouseEvent, pr: PullRequestEntry) => {
      e.preventDefault();
      setContextMenu({ x: e.clientX, y: e.clientY, pr });
    },
    [],
  );

  const prContextMenuItems = useMemo<MenuItemDef[]>(() => {
    if (!contextMenu) return [];
    const isMerged = contextMenu.pr.status === "merged";
    return [
      { id: "pr.checkout", label: "Checkout PR Branch", icon: GitBranch },
      { id: "pr.merge", label: "Merge PR", icon: GitMerge, disabled: isMerged },
      { id: "pr.view-web", label: "View on Web", icon: ExternalLink, separator: true },
    ];
  }, [contextMenu]);

  const handleContextMenuSelect = useCallback(
    (actionId: string) => {
      if (!contextMenu) return;
      const prNumber = String(contextMenu.pr.number);

      void executeCommand(actionId, commandContext, prNumber).then(() => {
        fetchPrs();
      });
      setContextMenu(null);
    },
    [contextMenu, commandContext, fetchPrs],
  );

  // Loading state
  if (loading && prs.length === 0) {
    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 text-muted-foreground">
        <Loader2 size={20} className="animate-spin" />
        <span className="text-xs">Loading pull requests…</span>
      </div>
    );
  }

  // Error state — auth required or forge not configured
  if (error) {
    const isAuthError =
      error.toLowerCase().includes("auth") ||
      error.toLowerCase().includes("token") ||
      error.toLowerCase().includes("unauthorized") ||
      error.toLowerCase().includes("not configured");

    return (
      <div className="flex h-full flex-col items-center justify-center gap-3 px-4 text-center">
        <GitPullRequest size={32} className="text-muted-foreground" />
        <div className="text-sm text-foreground">
          {isAuthError ? "Forge authentication required" : "Unable to load pull requests"}
        </div>
        <div className="max-w-[240px] text-[11px] text-muted-foreground">
          {isAuthError
            ? "Connect your forge account in Settings to view pull requests."
            : error}
        </div>
        {isAuthError && onNavigateToSettings && (
          <Button variant="outline"
            type="button"
            onClick={onNavigateToSettings}
            className="mt-1 inline-flex items-center gap-1.5 text-xs"
          >
            <Settings size={12} />
            Open Settings
          </Button>
        )}
        <Button variant="outline"
          type="button"
          onClick={() => void fetchPrs()}
          className="mt-1 inline-flex items-center gap-1.5 text-xs"
        >
          <RefreshCw size={12} />
          Retry
        </Button>
      </div>
    );
  }

  // Empty state — no PRs
  if (prs.length === 0) {
    return (
      <div className="flex h-full flex-col">
        <ViewToolbar
          title="Pull Requests"
          icon={GitPullRequest}
          actions={toolbarActions}
          onAction={handleToolbarAction}
        />
        <div className="flex flex-1 flex-col items-center justify-center gap-2 px-4 text-center">
          <GitPullRequest size={32} className="text-muted-foreground" />
          <div className="text-sm text-foreground">No open pull requests</div>
          <div className="text-[11px] text-muted-foreground">
            Pull requests from your forge will appear here.
          </div>
          <Button variant="outline"
            type="button"
            onClick={() => void fetchPrs()}
            className="mt-2 inline-flex items-center gap-1.5 text-xs"
          >
            <RefreshCw size={12} />
            Refresh
          </Button>
        </div>
      </div>
    );
  }

  // Normal state — grouped PR list
  const groups = groupPrs(prs);

  return (
    <div className="flex h-full flex-col">
      <ViewToolbar
        title="Pull Requests"
        icon={GitPullRequest}
        actions={toolbarActions}
        onAction={handleToolbarAction}
        trailing={
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => void fetchPrs()}
            className="flex h-6 w-6 items-center justify-center rounded text-muted-foreground hover:bg-primary/20 hover:text-foreground"
            aria-label="Refresh pull requests"
            disabled={loading}
          >
            <RefreshCw size={14} className={loading ? "animate-spin" : ""} />
          </Button>
        }
      />
      <div ref={scrollRef} className="flex-1 overflow-auto py-1">
        {groups.assigned.length > 0 && (
          <PrSection title="Assigned to me" prs={groups.assigned} onSelectPr={onSelectPr} onContextMenu={handleContextMenu} />
        )}
        {groups.created.length > 0 && (
          <PrSection title="Created by me" prs={groups.created} onSelectPr={onSelectPr} onContextMenu={handleContextMenu} />
        )}
        <PrSection title="All" prs={groups.all} defaultOpen={groups.assigned.length === 0 && groups.created.length === 0} onSelectPr={onSelectPr} onContextMenu={handleContextMenu} />
      </div>

      {/* Context menu routed through executeCommand */}
      {contextMenu && (
        <ContextMenu
          items={prContextMenuItems}
          position={{ x: contextMenu.x, y: contextMenu.y }}
          onSelect={handleContextMenuSelect}
          onClose={() => setContextMenu(null)}
        />
      )}
    </div>
  );
}
