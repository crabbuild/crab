import type { LoadingLevel, RepoStatus, Workspace } from "../state";

export interface WorkspaceState {
  workspaces: Workspace[];
  activeWorkspaceRootPath: string | null;
  tabBar: string[];
}

export type WorkspaceAction =
  | { type: "set-active-workspace"; rootPath: string | null }
  | { type: "workspace-set"; workspace: Workspace }
  | { type: "workspace-close"; rootPath?: string }
  | { type: "workspace-update-status"; rootPath: string; statuses: RepoStatus[] }
  | { type: "workspace-promote-l2"; rootPath: string; relativePath: string; repoId: string }
  | { type: "workspace-remove-repo"; rootPath: string; relativePath: string }
  | { type: "tab-bar-activate"; relativePath: string }
  | { type: "tab-bar-close"; relativePath: string };

export function workspaceReducer(state: WorkspaceState, action: WorkspaceAction): WorkspaceState {
  switch (action.type) {
    case "set-active-workspace": {
      return { ...state, activeWorkspaceRootPath: action.rootPath };
    }

    case "workspace-set": {
      // Add or replace workspace by rootPath
      const existing = state.workspaces.findIndex((w) => w.rootPath === action.workspace.rootPath);
      const workspaces = existing >= 0
        ? state.workspaces.map((w, i) => i === existing ? action.workspace : w)
        : [...state.workspaces, action.workspace];
      return { ...state, workspaces };
    }

    case "workspace-close": {
      if (action.rootPath) {
        const workspaces = state.workspaces.filter((w) => w.rootPath !== action.rootPath);
        // If the closed workspace was the active dashboard workspace, reset selection
        const activeWorkspaceRootPath = state.activeWorkspaceRootPath === action.rootPath
          ? (workspaces[0]?.rootPath ?? null)
          : state.activeWorkspaceRootPath;
        return { ...state, workspaces, activeWorkspaceRootPath, tabBar: [] };
      }
      // Close all workspaces if no rootPath specified
      return { ...state, workspaces: [], activeWorkspaceRootPath: null, tabBar: [] };
    }

    case "workspace-update-status": {
      const workspaces = state.workspaces.map((ws) => {
        if (ws.rootPath !== action.rootPath) return ws;
        const statusMap = new Map(action.statuses.map((s) => [s.relativePath, s]));
        const repos = ws.repos.map((entry) => {
          const status = statusMap.get(entry.relativePath);
          if (!status) return entry;
          if (status.error) return { ...entry, error: status.error };
          return {
            ...entry,
            level: Math.max(entry.level, 1) as LoadingLevel,
            branch: status.branch,
            dirtyCount: status.dirtyCount,
            ahead: status.ahead,
            behind: status.behind,
            error: null,
          };
        });
        return { ...ws, repos };
      });
      return { ...state, workspaces };
    }

    case "workspace-promote-l2": {
      const workspaces = state.workspaces.map((ws) => {
        if (ws.rootPath !== action.rootPath) return ws;
        const repos = ws.repos.map((entry) => {
          if (entry.relativePath !== action.relativePath) return entry;
          if (entry.level === 2) return entry;
          return { ...entry, level: 2 as LoadingLevel, openRepoId: action.repoId };
        });
        return { ...ws, repos };
      });
      return { ...state, workspaces };
    }

    case "workspace-remove-repo": {
      const workspaces = state.workspaces.map((ws) => {
        if (ws.rootPath !== action.rootPath) return ws;
        const repos = ws.repos.filter((entry) => entry.relativePath !== action.relativePath);
        return { ...ws, repos };
      });
      const tabBar = state.tabBar.filter((p) => p !== action.relativePath);
      return { ...state, workspaces, tabBar };
    }

    case "tab-bar-activate": {
      const now = Date.now();
      const workspaces = state.workspaces.map((ws) => {
        const hasEntry = ws.repos.some((e) => e.relativePath === action.relativePath);
        if (!hasEntry) return ws;
        const repos = ws.repos.map((entry) =>
          entry.relativePath === action.relativePath
            ? { ...entry, lastAccessedAt: now }
            : entry,
        );
        return { ...ws, repos };
      });
      // Keep tab position stable: only add if not already present.
      // Existing tabs stay in their original insertion order.
      const alreadyInTabBar = state.tabBar.includes(action.relativePath);
      let tabBar = alreadyInTabBar ? state.tabBar : [...state.tabBar, action.relativePath];
      // LRU eviction: if over max, remove the oldest (first) entry
      // that is NOT the one being activated.
      if (tabBar.length > 9) {
        const evictIndex = tabBar.findIndex((p) => p !== action.relativePath);
        if (evictIndex !== -1) {
          tabBar = [...tabBar.slice(0, evictIndex), ...tabBar.slice(evictIndex + 1)];
        } else {
          tabBar = tabBar.slice(tabBar.length - 9);
        }
      }
      return { ...state, workspaces, tabBar };
    }

    case "tab-bar-close": {
      const tabBar = state.tabBar.filter((p) => p !== action.relativePath);
      return { ...state, tabBar };
    }
  }
}
