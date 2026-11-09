import type { Workspace, WorkspaceRepoEntry } from "../state";

export function workspaceIdentity(
  workspace: Pick<Workspace, "kind" | "profileId" | "rootPath">,
): string {
  return JSON.stringify([
    workspace.kind,
    workspace.profileId ?? null,
    workspace.rootPath,
  ]);
}

export function workspaceRepoIdentity(
  workspace: Workspace,
  entry: WorkspaceRepoEntry,
): string {
  return workspace.kind === "remote"
    ? `ssh://${workspace.profileId}${entry.absolutePath}`
    : entry.absolutePath;
}

export function matchesRepository(
  query: string,
  name: string,
  path: string,
): boolean {
  const needle = query.trim().toLocaleLowerCase();
  return (
    !needle ||
    name.toLocaleLowerCase().includes(needle) ||
    path.toLocaleLowerCase().includes(needle)
  );
}

export interface WorkspaceSidebarState {
  collapsed: boolean;
  recent: Record<string, number>;
}

export function readWorkspaceSidebarState(key: string): WorkspaceSidebarState {
  try {
    const value = JSON.parse(localStorage.getItem(key) ?? "null");
    const recent: Record<string, number> = {};
    if (value && typeof value.recent === "object" && value.recent !== null) {
      for (const [path, time] of Object.entries(value.recent)) {
        if (typeof time === "number" && Number.isFinite(time) && time > 0)
          recent[path] = time;
      }
    }
    return { collapsed: value?.collapsed === true, recent };
  } catch {
    return { collapsed: false, recent: {} };
  }
}

export function groupWorkspaceRepositories(
  workspace: Workspace,
  pinned: Set<string>,
  recent: Record<string, number>,
  query: string,
) {
  const roots: WorkspaceRepoEntry[] = [];
  const children = new Map<string, WorkspaceRepoEntry[]>();
  for (const entry of [...workspace.repos].sort((a, b) =>
    a.relativePath.localeCompare(b.relativePath),
  )) {
    const parent = roots.find((root) =>
      entry.relativePath.startsWith(root.relativePath + "/"),
    );
    if (parent)
      children.set(parent.relativePath, [
        ...(children.get(parent.relativePath) ?? []),
        entry,
      ]);
    else roots.push(entry);
  }
  const visible = roots.filter((entry) => {
    const descendants = children.get(entry.relativePath) ?? [];
    if (matchesRepository(query, entry.name, entry.absolutePath)) return true;
    const matching = descendants.filter((child) =>
      matchesRepository(query, child.name, child.absolutePath),
    );
    children.set(entry.relativePath, matching);
    return matching.length > 0;
  });
  const pins = visible.filter((entry) =>
    pinned.has(workspaceRepoIdentity(workspace, entry)),
  );
  const unpinned = visible.filter(
    (entry) => !pinned.has(workspaceRepoIdentity(workspace, entry)),
  );
  const recentTime = (entry: WorkspaceRepoEntry) =>
    Math.max(recent[entry.relativePath] ?? 0, entry.lastAccessedAt);
  const accessed = unpinned
    .filter((entry) => recentTime(entry) > 0)
    .sort(
      (a, b) =>
        recentTime(b) - recentTime(a) ||
        a.relativePath.localeCompare(b.relativePath),
    )
    .slice(0, 5);
  const recentPaths = new Set(accessed.map((entry) => entry.relativePath));
  return {
    groups: [
      { label: "Pinned", entries: pins },
      { label: "Recent", entries: accessed },
      {
        label: "All repositories",
        entries: unpinned.filter(
          (entry) => !recentPaths.has(entry.relativePath),
        ),
      },
    ].filter((group) => group.entries.length > 0),
    children,
  };
}
