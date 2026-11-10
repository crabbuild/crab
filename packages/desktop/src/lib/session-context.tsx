/**
 * SessionContext — provides singleton SessionManager and
 * WorkspaceSessionManager instances to the React tree and convenience
 * hooks for accessing session state.
 *
 * Usage:
 *   // Wrap inside StoreProvider:
 *   <SessionProvider>…</SessionProvider>
 *
 *   // Consume anywhere:
 *   const session = useSession();
 *   const wsSession = useWorkspaceSession();
 *   const repoPath = useActiveRepoKey();
 */

import { createContext, useContext, useRef, type ReactNode } from "react";

import { repositoryKey, useStore } from "../state";
import { SessionManager, type ISessionManager } from "./session-manager";
import { WorkspaceSessionManager, type IWorkspaceSessionManager } from "./workspace-session-manager";

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

const SessionContext = createContext<ISessionManager | null>(null);
const WorkspaceSessionContext = createContext<IWorkspaceSessionManager | null>(null);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/**
 * Instantiates a single SessionManager and WorkspaceSessionManager and
 * provides them via context. Must be rendered inside StoreProvider
 * (useActiveRepoKey depends on it).
 */
export function SessionProvider({ children }: { children: ReactNode }) {
  const managerRef = useRef<ISessionManager | null>(null);
  if (managerRef.current === null) {
    managerRef.current = new SessionManager();
  }

  const wsManagerRef = useRef<IWorkspaceSessionManager | null>(null);
  if (wsManagerRef.current === null) {
    wsManagerRef.current = new WorkspaceSessionManager();
  }

  return (
    <SessionContext.Provider value={managerRef.current}>
      <WorkspaceSessionContext.Provider value={wsManagerRef.current}>
        {children}
      </WorkspaceSessionContext.Provider>
    </SessionContext.Provider>
  );
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/**
 * Returns the SessionManager instance from context.
 * Throws if called outside a SessionProvider.
 */
export function useSession(): ISessionManager {
  const ctx = useContext(SessionContext);
  if (!ctx) {
    throw new Error("useSession must be used within a SessionProvider");
  }
  return ctx;
}

/**
 * Returns the WorkspaceSessionManager instance from context.
 * Throws if called outside a SessionProvider.
 */
export function useWorkspaceSession(): IWorkspaceSessionManager {
  const ctx = useContext(WorkspaceSessionContext);
  if (!ctx) {
    throw new Error("useWorkspaceSession must be used within a SessionProvider");
  }
  return ctx;
}

/**
 * Return the active repository's stable UI/session identity,
 * or null if no repo is active.
 */
export function useActiveRepoKey(): string | null {
  const { state } = useStore();
  if (!state.activeRepoId) return null;
  const repo = state.repos.find((r) => r.repo_id === state.activeRepoId);
  return repo ? repositoryKey(repo) : null;
}
