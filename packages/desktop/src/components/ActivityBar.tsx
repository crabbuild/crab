import { useState } from "react";
import clsx from "clsx";
import {
  Bot,
  Files,
  FolderTree,
  GitGraph,
  GitPullRequest,
  LayoutDashboard,
  Monitor,
  Package,
  Search,
  Settings,
  SquareDot,
  Terminal,
  Globe,
  Workflow,
  type LucideIcon,
  Blocks,
} from "lucide-react";

export type SidebarView =
  | "explorer"
  | "search"
  | "changes"
  | "branches"
  | "worktrees"
  | "conflicts"
  | "timeline"
  | "pull-requests"
  | "crab"
  | "workflows"
  | "agent-workflows"
  | "terminal"
  | "dashboard"
  | "orchestrator"
  | "agents"
  | "browser"
  | "ssh"
  | "settings";

interface ViewDef {
  id: SidebarView;
  label: string;
  icon: LucideIcon;
}

export const VIEWS: ViewDef[] = [
  { id: "dashboard", label: "Dashboard", icon: LayoutDashboard },
  { id: "orchestrator", label: "Chat", icon: Bot },
  { id: "explorer", label: "Explorer", icon: Files },
  { id: "search", label: "Search", icon: Search },
  { id: "crab", label: "Large Files", icon: Package },
  { id: "changes", label: "Changes & Stashes", icon: SquareDot },
  { id: "worktrees", label: "Worktrees & Branches", icon: FolderTree },
  { id: "timeline", label: "Timeline & Conflicts", icon: GitGraph },
  { id: "pull-requests", label: "Pull Requests", icon: GitPullRequest },
  { id: "agent-workflows", label: "Agent Workflows", icon: Blocks },
  { id: "workflows", label: "Data Workflows", icon: Workflow },
  { id: "browser", label: "Browser", icon: Globe },
    { id: "ssh", label: "SSH Connections", icon: Monitor },
  { id: "terminal", label: "Terminal", icon: Terminal },
  { id: "settings", label: "Settings", icon: Settings },
];

interface Props {
  active: SidebarView;
  sidebarOpen: boolean;
  onSelect: (view: SidebarView) => void;
  /** Views to hide from the activity bar (e.g. pull-requests when no
   *  forge provider is detected). */
  hiddenViews?: SidebarView[];
  /** Badge counts to display on view icons (e.g. conflict count). */
  badges?: Partial<Record<SidebarView, number>>;
}

export function ActivityBar({ active, sidebarOpen: _sidebarOpen, onSelect, hiddenViews = [], badges = {} }: Props) {
  const [hoveredId, setHoveredId] = useState<string | null>(null);

  // Tasks view is always visible — the orchestrator setting controls
  // whether agents can be spawned, not whether the panel is accessible.
  const visibleViews = hiddenViews.length > 0
    ? VIEWS.filter((v) => !hiddenViews.includes(v.id))
    : VIEWS;

  return (
    <nav
      className="flex h-full w-12 shrink-0 flex-col items-center border-r border-gx-border bg-gx-surface py-2"
      aria-label="Primary"
    >
      {visibleViews.map((v) => {
        const isActive = active === v.id;
        return (
          <div key={v.id} className="relative">
            <button
              type="button"
              onClick={() => onSelect(v.id)}
              onMouseEnter={() => setHoveredId(v.id)}
              onMouseLeave={() => setHoveredId(null)}
              className={clsx(
                "relative mb-1 flex h-10 w-10 items-center justify-center rounded text-gx-muted transition",
                isActive ? "text-gx-text" : "hover:text-gx-text hover:bg-gx-surface-hover",
              )}
              aria-label={v.label}
              aria-pressed={isActive}
            >
              <v.icon size={20} />
              {isActive && (
                <span className="absolute left-0 top-1 h-8 w-[2px] rounded-r bg-gx-accent" />
              )}
              {/* Badge count indicator */}
              {badges[v.id] != null && badges[v.id]! > 0 && (
                <span className="absolute -right-0.5 -top-0.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-gx-error px-1 text-[9px] font-bold text-white">
                  {badges[v.id]! > 99 ? "99+" : badges[v.id]}
                </span>
              )}
            </button>
            {/* VS Code-style tooltip — appears to the right of the icon */}
            {hoveredId === v.id && (
              <div
                className="pointer-events-none absolute left-[48px] top-1/2 z-50 -translate-y-1/2 whitespace-nowrap rounded border border-gx-border bg-gx-surface px-2 py-1 text-[11px] text-gx-text shadow-lg"
                role="tooltip"
              >
                {v.label}
              </div>
            )}
          </div>
        );
      })}
    </nav>
  );
}
