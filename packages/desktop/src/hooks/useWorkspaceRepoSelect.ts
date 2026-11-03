import { useCallback } from "react";
import { useStore } from "../state";
import { rpc, RepoHandle } from "../ipc";
import { publishToast } from "../lib/toasts";

/**
 * Hook that returns a callback for selecting a workspace repo.
 *
 * - If the repo is already at L2, dispatches `set-active` using its
 *   existing openRepoId (0ms switch).
 * - If the repo is at L0 or L1, promotes it to L2:
 *   - Local repos: calls `rpc.openLocal(path)`
 *   - Remote repos: calls `crab.ssh.openAsRepo(profileId, path)` which
 *     opens the repo on the remote agent, registers the RemoteProvider,
 *     and starts file watching automatically.
 * - On failure, shows a toast error and keeps the entry at its current
 *   level so the user can retry.
 */
export function useWorkspaceRepoSelect() {
  const { state, dispatch, addRepo, setActive } = useStore();

  const selectWorkspaceRepo = useCallback(
    async (relativePath: string) => {
      if (state.workspaces.length === 0) return;

      // Find the entry across all workspaces
      let entry = null;
      let wsRootPath: string | null = null;
      let wsKind: "local" | "remote" = "local";
      let wsProfileId: string | undefined;
      for (const ws of state.workspaces) {
        const found = ws.repos.find((e) => e.relativePath === relativePath);
        if (found) {
          entry = found;
          wsRootPath = ws.rootPath;
          wsKind = ws.kind;
          wsProfileId = ws.profileId;
          break;
        }
      }
      if (!entry || !wsRootPath) return;

      // Auto-select the parent workspace so the TopBar dropdown and
      // dashboard reflect the workspace that owns this repo.
      if (state.activeWorkspaceRootPath !== wsRootPath) {
        dispatch({ type: "set-active-workspace", rootPath: wsRootPath });
      }

      // Already at L2 — just switch active repo (instant).
      if (entry.level === 2 && entry.openRepoId) {
        setActive(entry.openRepoId);
        dispatch({ type: "tab-bar-activate", relativePath });
        return;
      }

      // Promote to L2 via agent RPC.
      try {
        let handle: RepoHandle;
        let repoId: string;

        if (wsKind === "remote" && wsProfileId) {
          // Remote repo: open via SSH agent. The main process handler
          // registers the RemoteProvider and starts file watching.
          handle = await (window as any).crab.ssh.openAsRepo(
            wsProfileId,
            entry.absolutePath,
          );
          repoId = `ssh://${wsProfileId}${entry.absolutePath}`;
        } else {
          // Local repo: open via the local agent.
          handle = await rpc.openLocal(entry.absolutePath);
          repoId = handle.repo_id;
        }

        dispatch({
          type: "workspace-promote-l2",
          rootPath: wsRootPath,
          relativePath,
          repoId,
        });
        addRepo({
          ...handle,
          label: entry.name,
          path: entry.absolutePath,
          openedAt: Date.now(),
        });
        setActive(repoId);
        dispatch({ type: "tab-bar-activate", relativePath });
      } catch (err) {
        publishToast({
          kind: "error",
          title: "Failed to open repository",
          body: err instanceof Error ? err.message : String(err),
        });
      }
    },
    [state.workspaces, dispatch, addRepo, setActive],
  );

  return selectWorkspaceRepo;
}
