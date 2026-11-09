export const VIEW_LABELS = {
  explorer: "Explorer",
  search: "Search",
  changes: "Changes",
  branches: "Branches",
  worktrees: "Worktrees",
  timeline: "History",
  "pull-requests": "Pull Requests",
  crab: "Large Files",
  workflows: "Data Workflows",
  terminal: "Terminal",
  dashboard: "Overview",
  orchestrator: "Chat",
  ssh: "SSH Connections",
  settings: "Settings",
} as const;

export type SidebarView = keyof typeof VIEW_LABELS;
export type NavigationGroup = "files" | "git";
export const VIEW_GROUPS: Record<NavigationGroup, readonly SidebarView[]> = {
  files: ["explorer", "search"],
  git: ["changes", "timeline", "branches", "worktrees", "pull-requests"],
};

export function isSidebarView(view: string): view is SidebarView {
  return Object.hasOwn(VIEW_LABELS, view);
}

export function navigationGroup(
  view: SidebarView,
): NavigationGroup | undefined {
  if (VIEW_GROUPS.files.includes(view)) return "files";
  if (VIEW_GROUPS.git.includes(view)) return "git";
}
