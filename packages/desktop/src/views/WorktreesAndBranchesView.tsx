import { useEffect, useState } from "react";
import clsx from "clsx";
import { FolderTree, GitBranch } from "lucide-react";
import { OpenRepo } from "../state";
import { HydratedWorktreeEntry, WorktreeStatus } from "../ipc";
import { WorktreeView } from "./WorktreeView";
import { BranchesView } from "./BranchesView";

type Tab = "worktrees" | "branches";

interface Props {
  repo: OpenRepo;
  /** Worktree selection state (passed through to WorktreeView). */
  onSelectWorktree: (wt: HydratedWorktreeEntry | null) => void;
  selectedWorktree: HydratedWorktreeEntry | null;
  statusMap: Map<string, WorktreeStatus>;
  onStatusMapChange: (map: Map<string, WorktreeStatus>) => void;
  /** Branch selection state (passed through to BranchesView). */
  selectedBranch: string | null;
  onSelectBranch: (name: string | null) => void;
  /** When set, overrides the active tab (used for programmatic navigation). */
  requestedTab?: Tab | null;
  /** Notifies the parent whenever the active tab changes (manual or programmatic). */
  onTabChange?: (tab: Tab) => void;
}

const LS_ACTIVE_TAB = "crab.worktreesBranches.tab";

function loadTab(): Tab {
  const stored = localStorage.getItem(LS_ACTIVE_TAB);
  if (stored === "branches") return "branches";
  return "worktrees";
}

/**
 * Tabbed container that combines the Worktrees view and Branches view
 * into a single unified sidebar panel — mirroring the Changes & Stashes
 * pattern. Worktrees is the first (default) tab, Branches is the second.
 */
export function WorktreesAndBranchesView({
  repo,
  onSelectWorktree,
  selectedWorktree,
  statusMap,
  onStatusMapChange,
  selectedBranch,
  onSelectBranch,
  requestedTab,
  onTabChange,
}: Props) {
  const [activeTab, setActiveTabRaw] = useState<Tab>(loadTab);

  const setActiveTab = (tab: Tab) => {
    setActiveTabRaw(tab);
    onTabChange?.(tab);
  };

  // Respond to programmatic tab switches from the parent.
  useEffect(() => {
    if (requestedTab && requestedTab !== activeTab) {
      setActiveTabRaw(requestedTab);
      onTabChange?.(requestedTab);
    }
  }, [requestedTab]);

  // Persist tab selection
  useEffect(() => {
    localStorage.setItem(LS_ACTIVE_TAB, activeTab);
  }, [activeTab]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      {/* Tab bar */}
      <div className="flex shrink-0 items-center border-b border-gx-border bg-gx-surface">
        <TabButton
          active={activeTab === "worktrees"}
          onClick={() => setActiveTab("worktrees")}
          icon={<FolderTree size={12} />}
          label="Worktrees"
        />
        <TabButton
          active={activeTab === "branches"}
          onClick={() => setActiveTab("branches")}
          icon={<GitBranch size={12} />}
          label="Branches"
        />
      </div>

      {/* Tab content */}
      <div className="flex min-h-0 flex-1 flex-col">
        {activeTab === "worktrees" ? (
          <WorktreeView
            repo={repo}
            onSelectWorktree={onSelectWorktree}
            selectedWorktree={selectedWorktree}
            statusMap={statusMap}
            onStatusMapChange={onStatusMapChange}
          />
        ) : (
          <BranchesView
            repo={repo}
            selectedBranch={selectedBranch}
            onSelectBranch={onSelectBranch}
          />
        )}
      </div>
    </div>
  );
}

// ── Tab Button ───────────────────────────────────────────────────────

function TabButton({
  active,
  onClick,
  icon,
  label,
  badge,
}: {
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
  badge?: number;
}) {
  return (
    <button
      type="button"
      className={clsx(
        "relative flex items-center gap-1.5 px-3 py-2 text-xs font-medium transition",
        active
          ? "text-gx-text"
          : "text-gx-muted hover:text-gx-text",
      )}
      onClick={onClick}
      role="tab"
      aria-selected={active}
    >
      {icon}
      {label}
      {badge !== undefined && badge > 0 && (
        <span
          className={clsx(
            "ml-0.5 inline-flex h-4 min-w-[16px] items-center justify-center rounded-full px-1 text-[10px] font-medium",
            active
              ? "bg-gx-accent/20 text-gx-accent"
              : "bg-gx-border text-gx-muted",
          )}
        >
          {badge > 99 ? "99+" : badge}
        </span>
      )}
      {/* Active indicator */}
      {active && (
        <span className="absolute inset-x-0 bottom-0 h-[2px] rounded-full bg-gx-accent" />
      )}
    </button>
  );
}
