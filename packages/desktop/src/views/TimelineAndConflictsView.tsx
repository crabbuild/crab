import { useEffect, useState } from "react";
import clsx from "clsx";
import { GitGraph, GitMergeConflict } from "lucide-react";
import { CommitRow } from "../ipc";
import { OpenRepo } from "../state";
import { TimelineView } from "./TimelineView";
import { ConflictsView } from "./ConflictsView";

type Tab = "timeline" | "conflicts";

interface Props {
  repo: OpenRepo;
  /** Timeline state */
  selectedCommit: string | null;
  onSelectCommit: (sha: string | null) => void;
  onCommitsLoaded?: (commits: CommitRow[]) => void;
  timelineRefreshKey?: number;
  /** Conflicts state */
  conflictPath: string | null;
  onSelectConflictFile: (path: string) => void;
  onConflictCountChange?: (count: number) => void;
  /** Badge count for the conflicts tab. */
  conflictCount?: number;
  /** When set, overrides the active tab (used for programmatic navigation). */
  requestedTab?: Tab | null;
  /** Notifies the parent whenever the active tab changes. */
  onTabChange?: (tab: Tab) => void;
}

const LS_ACTIVE_TAB = "crab.timelineConflicts.tab";

function loadTab(): Tab {
  const stored = localStorage.getItem(LS_ACTIVE_TAB);
  if (stored === "conflicts") return "conflicts";
  return "timeline";
}

/**
 * Tabbed container combining Commit Timeline and Conflicts into a
 * single sidebar panel. Timeline is the first (default) tab,
 * Conflicts is the second.
 */
export function TimelineAndConflictsView({
  repo,
  selectedCommit,
  onSelectCommit,
  onCommitsLoaded,
  timelineRefreshKey,
  conflictPath,
  onSelectConflictFile,
  onConflictCountChange,
  conflictCount,
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
          active={activeTab === "timeline"}
          onClick={() => setActiveTab("timeline")}
          icon={<GitGraph size={12} />}
          label="Timeline"
        />
        <TabButton
          active={activeTab === "conflicts"}
          onClick={() => setActiveTab("conflicts")}
          icon={<GitMergeConflict size={12} />}
          label="Conflicts"
          badge={conflictCount}
        />
      </div>

      {/* Tab content */}
      <div className="flex min-h-0 flex-1 flex-col">
        {activeTab === "timeline" ? (
          <TimelineView
            repo={repo}
            selectedSha={selectedCommit}
            onSelectCommit={onSelectCommit}
            onCommitsLoaded={onCommitsLoaded}
            refreshKey={timelineRefreshKey}
          />
        ) : (
          <ConflictsView
            repo={repo}
            selectedPath={conflictPath}
            onSelectFile={onSelectConflictFile}
            onConflictCountChange={onConflictCountChange}
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
