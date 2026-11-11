import { useCallback, useRef, useState } from "react";
import { isSidebarView } from "../lib/navigation";

/**
 * VS Code-style back/forward navigation history.
 *
 * Tracks a stack of "navigation entries" (view + optional context
 * like a selected commit sha and workspace repo path). The user can
 * go back/forward through the stack. New navigations push onto the
 * stack and truncate any forward history (same as browser behaviour).
 *
 * Cross-repo navigation: when an entry includes a `repoPath` and
 * back/forward restores an entry with a different `repoPath`, the
 * provided `onRepoSwitch` callback is invoked so the app can switch
 * the active repo before restoring the view.
 *
 * Usage:
 *   const nav = useNavHistory({ onRepoSwitch });
 *   // When user navigates:
 *   nav.push({ view: "timeline", commit: sha, repoPath: "frontend" });
 *   // Back/forward:
 *   nav.back();  nav.forward();
 *   // Current entry:
 *   nav.current
 */
export interface NavEntry {
  view: string;
  commit?: string | null;
  /** Workspace-relative repo path (for cross-repo navigation) */
  repoPath?: string | null;
}

export interface UseNavHistoryOptions {
  /**
   * Called when back() or forward() restores an entry whose repoPath
   * differs from the current entry's repoPath. The app should switch
   * the active repo to the given workspace-relative path.
   */
  onRepoSwitch?: (repoPath: string) => void;
}

/** Maximum number of navigation entries retained per repo. */
const NAV_HISTORY_CAP = 50;

export function useNavHistory(options?: UseNavHistoryOptions) {
  const stackRef = useRef<NavEntry[]>([{ view: "explorer" }]);
  const indexRef = useRef(0);
  const [, forceRender] = useState(0);
  const [restoreEntry, setRestoreEntry] = useState<NavEntry>();
  const onRepoSwitchRef = useRef(options?.onRepoSwitch);
  onRepoSwitchRef.current = options?.onRepoSwitch;

  const push = useCallback((entry: NavEntry) => {
    const stack = stackRef.current;
    const idx = indexRef.current;
    // Truncate forward history.
    stack.splice(idx + 1);
    stack.push(entry);

    // Cap at NAV_HISTORY_CAP — trim oldest entries from the beginning.
    if (stack.length > NAV_HISTORY_CAP) {
      const overflow = stack.length - NAV_HISTORY_CAP;
      stack.splice(0, overflow);
    }

    indexRef.current = stack.length - 1;
    forceRender((n) => n + 1);
  }, []);

  const back = useCallback(() => {
    if (indexRef.current > 0) {
      const prevEntry = stackRef.current[indexRef.current];
      // Skip dead entries (views that no longer exist).
      let targetIdx = indexRef.current - 1;
      while (targetIdx > 0 && !isSidebarView(stackRef.current[targetIdx].view)) {
        targetIdx--;
      }
      // If the final candidate is also dead, don't navigate.
      if (!isSidebarView(stackRef.current[targetIdx].view)) return;

      indexRef.current = targetIdx;
      const newEntry = stackRef.current[indexRef.current];
      // If the restored entry has a different repoPath, trigger repo switch.
      if (
        onRepoSwitchRef.current &&
        newEntry?.repoPath != null &&
        newEntry.repoPath !== (prevEntry?.repoPath ?? null)
      ) {
        onRepoSwitchRef.current(newEntry.repoPath);
      }
      setRestoreEntry({ ...newEntry });
    }
  }, []);

  const forward = useCallback(() => {
    if (indexRef.current < stackRef.current.length - 1) {
      const prevEntry = stackRef.current[indexRef.current];
      // Skip dead entries (views that no longer exist).
      let targetIdx = indexRef.current + 1;
      const maxIdx = stackRef.current.length - 1;
      while (targetIdx < maxIdx && !isSidebarView(stackRef.current[targetIdx].view)) {
        targetIdx++;
      }
      // If the final candidate is also dead, don't navigate.
      if (!isSidebarView(stackRef.current[targetIdx].view)) return;

      indexRef.current = targetIdx;
      const newEntry = stackRef.current[indexRef.current];
      // If the restored entry has a different repoPath, trigger repo switch.
      if (
        onRepoSwitchRef.current &&
        newEntry?.repoPath != null &&
        newEntry.repoPath !== (prevEntry?.repoPath ?? null)
      ) {
        onRepoSwitchRef.current(newEntry.repoPath);
      }
      setRestoreEntry({ ...newEntry });
    }
  }, []);

  /** Returns the full navigation state for external persistence. */
  const getState = useCallback((): { entries: NavEntry[]; index: number } => {
    return { entries: [...stackRef.current], index: indexRef.current };
  }, []);

  /** Replaces the internal navigation stack (for restoring from session). */
  const setState = useCallback(
    (entries: NavEntry[], index: number) => {
      // Enforce the cap on restored entries.
      let trimmed = entries;
      let adjustedIndex = index;
      if (trimmed.length > NAV_HISTORY_CAP) {
        const overflow = trimmed.length - NAV_HISTORY_CAP;
        trimmed = trimmed.slice(overflow);
        adjustedIndex = Math.max(0, adjustedIndex - overflow);
      }
      // Clamp index to valid range.
      adjustedIndex = Math.max(0, Math.min(adjustedIndex, trimmed.length - 1));
      stackRef.current = trimmed.length > 0 ? trimmed : [{ view: "explorer" }];
      indexRef.current = trimmed.length > 0 ? adjustedIndex : 0;
      setRestoreEntry({ ...stackRef.current[indexRef.current] });
    },
    [],
  );

  return {
    current: stackRef.current[indexRef.current] as NavEntry | undefined,
    // Recorded pushes are observations, not commands to replace a newer view.
    // Each explicit restore gets a fresh snapshot even when revisiting an entry.
    restoreEntry,
    canBack: indexRef.current > 0,
    canForward: indexRef.current < stackRef.current.length - 1,
    push,
    back,
    forward,
    getState,
    setState,
  };
}
