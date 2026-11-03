import type { Workspace, WorkspaceRepoEntry } from "../../src/state";
import type { WorkspaceState } from "../../src/lib/workspace-reducer";
import { defaultWorkspaceConfig } from "../../electron/workspace-config";

export function entry(relativePath: string, overrides: Partial<WorkspaceRepoEntry> = {}): WorkspaceRepoEntry {
  return {
    relativePath,
    absolutePath: `/workspace/${relativePath}`,
    name: relativePath.split("/").pop() ?? relativePath,
    level: 0,
    branch: null,
    dirtyCount: null,
    ahead: null,
    behind: null,
    error: null,
    openRepoId: null,
    lastAccessedAt: 0,
    ...overrides,
  };
}

export function workspace(rootPath = "/workspace", repos = [entry("repo")]): Workspace {
  return { kind: "local", rootPath, name: rootPath.split("/").pop() ?? rootPath, repos, config: defaultWorkspaceConfig() };
}

export function state(workspaces: Workspace[] = []): WorkspaceState {
  return { workspaces, activeWorkspaceRootPath: workspaces[0]?.rootPath ?? null, tabBar: [] };
}
