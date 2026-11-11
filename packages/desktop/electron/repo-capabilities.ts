/** Capabilities shared by providers and renderer controls. */
export interface CapabilitySet {
  canEdit: boolean;
  canWatch: boolean;
  canStageHunks: boolean;
  canWorktree: boolean;
  canForge: boolean;
  canHydrate: boolean;
  canHydrateSelection?: boolean;
  canMaterialize: boolean;
  canSearch: boolean;
  canExec: boolean;
  canTrash: boolean;
}

/** Native availability and terminal recovery failure for one repository. */
export interface RepositoryRuntime {
  repoId: string;
  sessionId: string | null;
  error?: string;
}

/** Report a retained source whose native repository session was replaced. */
export function nativeSessionLost(): Error & { code: string } {
  return Object.assign(new Error("The file reader's native session ended. Reopen the file."), { code: "NATIVE_SESSION_LOST" });
}

/** Capabilities for a local repository with native services available. */
export function fullCapabilities(): CapabilitySet {
  return {
    canEdit: true,
    canWatch: true,
    canStageHunks: true,
    canWorktree: true,
    canForge: true,
    canHydrate: true,
    canMaterialize: true,
    canSearch: true,
    canExec: true,
    canTrash: true,
  };
}

/** Capabilities while a repository's native services are unavailable. */
export function noCapabilities(): CapabilitySet {
  return {
    canEdit: false,
    canWatch: false,
    canStageHunks: false,
    canWorktree: false,
    canForge: false,
    canHydrate: false,
    canMaterialize: false,
    canSearch: false,
    canExec: false,
    canTrash: false,
  };
}
