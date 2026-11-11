// Provides per-view scroll position persistence. Attach the returned
// ref to a scrollable container — the hook restores the saved position
// on mount and saves it on scroll (debounced) and unmount.

import { useEffect, useRef, useState } from "react";

import { useSession, useActiveRepoKey } from "../lib/session-context";
import { useSetting } from "../settings";

/** Minimum scrollHeight (px) to consider content "loaded enough" for restore. */
const MIN_CONTENT_HEIGHT = 50;

/** Max retries when content hasn't rendered yet. */
const MAX_RESTORE_RETRIES = 3;

/** Delay between retries (ms). */
const RETRY_DELAY_MS = 100;

/** Debounce interval for saving scroll position on scroll events (ms). */
const SAVE_DEBOUNCE_MS = 250;

/**
 * Persists and restores scroll position for a scrollable view container.
 *
 * On mount: restores the saved scroll position (clamped to valid range)
 * after a requestAnimationFrame to let content render. If the container
 * hasn't loaded content yet (scrollHeight < 50px), retries up to 3 times
 * at 100ms intervals.
 *
 * On scroll: saves the position via sessionManager (debounced 250ms).
 * On unmount: saves the final position immediately.
 *
 * When the `scrollRestore` setting is disabled, the hook is inert — no
 * save, no restore.
 *
 * @param viewId - Stable identifier for this view (e.g. "timeline", "branches").
 * @returns A ref to attach to the scrollable container div.
 */
export function useSessionScroll(viewId: string): React.RefObject<HTMLDivElement> {
  const ref = useRef<HTMLDivElement>(null);
  const sessionManager = useSession();
  const repoPath = useActiveRepoKey();
  const [restored, setRestored] = useState(false);
  const sessionPersistence = useSetting("sessionPersistence");
  const scrollRestore = sessionPersistence.scrollRestore;

  // Restore scroll position after initial content render
  useEffect(() => {
    if (!scrollRestore || !ref.current || restored || !repoPath) return;

    const saved = sessionManager.getScrollPosition(repoPath, viewId);
    if (saved == null) {
      setRestored(true);
      return;
    }

    let retryCount = 0;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;

    const attemptRestore = () => {
      const el = ref.current;
      if (!el) return;

      // If content hasn't loaded yet, retry after a short delay
      if (el.scrollHeight < MIN_CONTENT_HEIGHT && retryCount < MAX_RESTORE_RETRIES) {
        retryCount++;
        retryTimer = setTimeout(attemptRestore, RETRY_DELAY_MS);
        return;
      }

      const maxScroll = el.scrollHeight - el.clientHeight;
      el.scrollTop = Math.min(saved, Math.max(0, maxScroll));
      setRestored(true);
    };

    // Wait one frame for initial content to render before attempting restore
    const rafId = requestAnimationFrame(attemptRestore);

    return () => {
      cancelAnimationFrame(rafId);
      if (retryTimer != null) clearTimeout(retryTimer);
    };
  }, [repoPath, viewId, restored, sessionManager, scrollRestore]);

  // Save scroll position on scroll (debounced) and on unmount
  useEffect(() => {
    if (!scrollRestore) return;

    const el = ref.current;
    if (!el || !repoPath) return;

    let timer: ReturnType<typeof setTimeout>;

    const onScroll = () => {
      clearTimeout(timer);
      timer = setTimeout(() => {
        sessionManager.updateScrollPosition(repoPath, viewId, el.scrollTop);
      }, SAVE_DEBOUNCE_MS);
    };

    el.addEventListener("scroll", onScroll, { passive: true });

    return () => {
      el.removeEventListener("scroll", onScroll);
      clearTimeout(timer);
      // Save final position on unmount
      sessionManager.updateScrollPosition(repoPath, viewId, el.scrollTop);
    };
  }, [repoPath, viewId, sessionManager, scrollRestore]);

  return ref;
}
