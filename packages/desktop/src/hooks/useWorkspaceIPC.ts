import { useEffect, useRef } from "react";
import { useStore } from "../state";
import type { RepoStatus, WorkspaceConfig } from "../state";

const LS_WORKSPACE = "crab.workspace.v1";

/**
 * Subscribes to workspace IPC events from the main process and
 * dispatches the appropriate state actions. Also handles startup
 * restore of persisted workspace state.
 *
 * Simplified model: a workspace is either entirely local or entirely
 * remote. All repos are discovered under the workspace root. There is
 * no hybrid workspace mixing local and remote repos.
 *
 * Persistence strategy:
 * - Workspace root paths are stored in localStorage as a JSON array.
 * - On startup, each persisted path is re-opened via the main process.
 * - If a path fails to open (directory moved/deleted), it's removed
 *   from the persisted list without affecting other workspaces.
 */
export function useWorkspaceIPC() {
  const { dispatch } = useStore();
  const restoringRef = useRef(false);
  // Track the most recently discovered workspace rootPath for status updates
  const lastRootPathRef = useRef<string | null>(null);
  // Track which workspace paths have been successfully restored
  const restoredPathsRef = useRef<Set<string>>(new Set());

  // Subscribe to workspace:discovered events AND handle startup restore
  // in the same effect to avoid race conditions (listener must be
  // registered before open() is called).
  useEffect(() => {
    if (typeof window === "undefined" || !window.crab?.workspace) return;

    const unsubscribe = window.crab.workspace.onDiscovered(
      ({ rootPath, repos }: { rootPath: string; repos: Array<{ absolutePath: string; relativePath: string; name: string }> }) => {
        const name = rootPath.split("/").pop() ?? rootPath.split("\\").pop() ?? "Workspace";
        lastRootPathRef.current = rootPath;

        // Persist workspace paths so they survive reload/restart.
        persistWorkspacePath(rootPath);

        const config: WorkspaceConfig = {
          version: 1,
          discovery: { maxDepth: 3, excludePatterns: [] },
          repos: { removed: [] },
          display: { tabBarState: [] },
        };

        dispatch({
          type: "workspace-set",
          workspace: {
            kind: "local",
            rootPath,
            name,
            repos: repos.map((r) => ({
              relativePath: r.relativePath,
              absolutePath: r.absolutePath,
              name: r.name,
              level: 0 as const,
              branch: null,
              dirtyCount: null,
              ahead: null,
              behind: null,
              error: null,
              openRepoId: null,
              lastAccessedAt: 0,
            })),
            config,
          },
        });

        // Tab bar and active repo restore is now handled by
        // WorkspaceSessionManager in Shell (task 6.3). The legacy
        // single-key format is migrated on first restore() call.
        if (restoringRef.current) {
          restoringRef.current = false;
        }
      },
    );

    // Startup restore: re-open all persisted workspace paths.
    // This runs after the listener is registered so discovery events
    // are captured immediately.
    restoreWorkspaces();

    return unsubscribe;

    async function restoreWorkspaces() {
      const paths = loadPersistedWorkspacePaths();
      if (paths.length === 0) return;

      restoringRef.current = true;

      // Open each workspace sequentially. The main process currently
      // supports one active workspace for polling, but the renderer
      // state supports multiple. Each successful open triggers a
      // workspace:discovered event that populates the state.
      const validPaths: string[] = [];
      for (const p of paths) {
        try {
          await window.crab.workspace.open(p);
          validPaths.push(p);
          restoredPathsRef.current.add(p);
        } catch {
          // This workspace path is no longer valid (moved/deleted).
          // Don't add it to validPaths — it will be pruned from storage.
        }
      }

      // Update persisted list to only include paths that opened successfully.
      if (validPaths.length !== paths.length) {
        try {
          localStorage.setItem(LS_WORKSPACE, JSON.stringify(validPaths));
        } catch {
          // best-effort
        }
      }

      // If no workspaces could be restored, clear the restoring flag.
      if (validPaths.length === 0) {
        restoringRef.current = false;
      }
    }
  }, [dispatch]);

  // Subscribe to workspace:status-update events
  useEffect(() => {
    if (typeof window === "undefined" || !window.crab?.workspace) return;

    const unsubscribe = window.crab.workspace.onStatusUpdate(
      (statuses: Array<{ relativePath: string; branch: string | null; dirtyCount: number; ahead: number | null; behind: number | null; error: string | null }>) => {
        const rootPath = lastRootPathRef.current;
        if (!rootPath) return;
        dispatch({
          type: "workspace-update-status",
          rootPath,
          statuses: statuses as RepoStatus[],
        });
      },
    );

    return unsubscribe;
  }, [dispatch]);
}

// ── Persistence helpers ─────────────────────────────────────────────

/** Load persisted workspace paths from localStorage. */
function loadPersistedWorkspacePaths(): string[] {
  try {
    const raw = localStorage.getItem(LS_WORKSPACE);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    // Handle legacy single-path format
    if (Array.isArray(parsed)) return parsed;
    if (typeof parsed === "string") return [parsed];
    return [];
  } catch {
    // Legacy format: raw string path
    const raw = localStorage.getItem(LS_WORKSPACE);
    return raw ? [raw] : [];
  }
}

/** Add a workspace path to the persisted list (deduped). */
function persistWorkspacePath(rootPath: string) {
  try {
    const paths = loadPersistedWorkspacePaths();
    if (!paths.includes(rootPath)) {
      paths.push(rootPath);
      localStorage.setItem(LS_WORKSPACE, JSON.stringify(paths));
    }
  } catch {
    // best-effort
  }
}
