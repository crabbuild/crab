/**
 * Hook for accessing the active repo's capability set. Components use
 * this to gate UI controls based on what the repo actually supports
 * (e.g., `canEdit`, `canWatch`) rather than checking `is_remote`.
 *
 * Returns the capabilities from the OpenRepo state (populated at open
 * time and updated reactively via `repo:capabilities-changed` events).
 * Falls back to a "full" capability set (all flags true) when
 * capabilities haven't been fetched yet, preserving backward
 * compatibility.
 */

import { useMemo } from "react";
import { useActiveRepo } from "../state";

// ── CapabilitySet type (renderer-side) ──────────────────────────────

/**
 * Feature flags describing what a given repository supports. Mirrors
 * the `CapabilitySet` defined in `electron/repo-provider.ts` for use
 * in the renderer process.
 */
export interface CapabilitySet {
  /** File writing (create, rename, delete, write). */
  canEdit: boolean;
  /** Real-time file change notifications. */
  canWatch: boolean;
  /** Hunk-level git staging. */
  canStageHunks: boolean;
  /** Git worktree management. */
  canWorktree: boolean;
  /** PR/forge operations (depends on forge_provider). */
  canForge: boolean;
  /** Crab hydrate/dehydrate. */
  canHydrate: boolean;
  /** Full-text search. */
  canSearch: boolean;
  /** Command execution (terminals, build/run). */
  canExec: boolean;
  /** Move to trash (local only — remote has no trash). */
  canTrash: boolean;
}

/**
 * Returns a CapabilitySet with all flags set to true. Used as the
 * default when capabilities haven't been populated yet, preserving
 * backward-compatible behavior (all features enabled).
 */
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

// Singleton instance to avoid allocating on every render when no
// capabilities are set on the repo.
const FULL_CAPABILITIES = fullCapabilities();

/**
 * Returns the CapabilitySet for the currently active repo. If the
 * repo doesn't have capabilities populated yet, returns a full
 * capability set so all UI controls remain enabled.
 */
export function useRepoCapabilities(): CapabilitySet {
  const repo = useActiveRepo();

  return useMemo(() => {
    return repo?.capabilities ?? FULL_CAPABILITIES;
  }, [repo?.capabilities]);
}
