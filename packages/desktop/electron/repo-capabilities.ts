/** Capabilities shared by providers and renderer controls. */
export interface CapabilitySet {
  canEdit: boolean;
  canWatch: boolean;
  canStageHunks: boolean;
  canWorktree: boolean;
  canForge: boolean;
  canHydrate: boolean;
  canSearch: boolean;
  canExec: boolean;
  canTrash: boolean;
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
    canSearch: false,
    canExec: false,
    canTrash: false,
  };
}
