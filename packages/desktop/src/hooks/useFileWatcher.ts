/**
 * Hook for subscribing to file-system change events from the native
 * file watcher. Views call `useFileWatcher(repoPath, callback)` to
 * get notified when files in the repo change on disk.
 *
 * The watcher lifecycle (start/stop) is managed by `useFileWatcherRoot`
 * which should be called once per repo at the RepoDetail level. Views
 * only subscribe to events — they don't start/stop the watcher.
 *
 * Key design decisions to avoid UI regression:
 *   - Two-layer debounce: native watcher (300ms) + per-subscriber (configurable)
 *   - Visibility gating: callbacks are deferred when the window is hidden
 *   - Coalescing: rapid events during the debounce window produce one callback
 *   - Stable refs: callback identity changes don't cause re-subscriptions
 */

import { useCallback, useEffect, useRef } from "react";
import { mainProcess, onAgentEvent } from "../ipc";
import type { OpenRepo } from "../state";
import { parseRepoId } from "../../electron/ssh-types";
import { repositoryWorktree } from "../lib/repository-worktree";

/**
 * Compute the watch path for a repo. Remote repos use `repo_id`
 * (e.g. `ssh://profile/path`) as the watch target; local repos use
 * the filesystem path. Returns `undefined` for deferred repos or
 * repos with no usable path.
 */
export function getWatchPath(repo: OpenRepo): string | undefined {
  const root = repositoryWorktree(repo);
  return root ? (parseRepoId(repo.repo_id) ? repo.repo_id : root) : undefined;
}

/**
 * Start the native file watcher for a repo. Call once at the
 * top-level component that owns the repo lifecycle (RepoDetail).
 * Automatically stops the watcher on unmount or when the repo path
 * changes.
 */
export function useFileWatcherRoot(repoPath: string | undefined): void {
  useEffect(() => {
    if (!repoPath) return;
    mainProcess.fsWatchStart(repoPath).catch(() => {
      // Non-fatal — watcher is a UX enhancement, not critical.
    });
    return () => {
      mainProcess.fsWatchStop(repoPath).catch(() => {});
    };
  }, [repoPath]);
}

/**
 * Subscribe to file-system change events for a specific repo path.
 * The callback is debounced internally so rapid bursts of events
 * (e.g. a `git checkout` touching many files) result in a single
 * invocation.
 *
 * When the document is hidden (window minimized, tab backgrounded),
 * the callback is deferred until the window becomes visible again.
 * This prevents wasted RPC calls when the user isn't looking.
 *
 * @param repoPath - The repo directory to listen for. Pass `undefined`
 *   to disable the subscription (e.g. for deferred repos).
 * @param onChanged - Callback fired when files change. Receives no
 *   arguments — the view should re-fetch whatever data it needs.
 * @param debounceMs - Debounce interval in ms. Defaults to 200ms.
 *   Use higher values (500-1000ms) for expensive operations like
 *   fetching full commit history or file trees.
 */
export function useFileWatcher(
  repoPath: string | undefined,
  onChanged: () => void,
  debounceMs = 200,
): void {
  // Keep a stable ref to the callback so the effect doesn't
  // re-subscribe on every render.
  const callbackRef = useRef(onChanged);
  callbackRef.current = onChanged;

  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Track whether a change arrived while the document was hidden.
  const pendingRef = useRef(false);

  const fireCallback = useCallback(() => {
    // If the window is hidden, defer the callback until it becomes
    // visible. This avoids wasted network calls and prevents the UI
    // from doing expensive re-renders while backgrounded.
    if (document.hidden) {
      pendingRef.current = true;
      return;
    }
    callbackRef.current();
  }, []);

  const debouncedCallback = useCallback(() => {
    if (timerRef.current) clearTimeout(timerRef.current);
    timerRef.current = setTimeout(fireCallback, debounceMs);
  }, [debounceMs, fireCallback]);

  // When the document becomes visible again, fire the pending callback.
  useEffect(() => {
    const onVisibilityChange = () => {
      if (!document.hidden && pendingRef.current) {
        pendingRef.current = false;
        callbackRef.current();
      }
    };
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, []);

  useEffect(() => {
    if (!repoPath) return;

    const off = onAgentEvent((evt) => {
      if (evt.method === "fs/changed") {
        const params = evt.params as { repo_path?: string };
        if (params.repo_path === repoPath) {
          debouncedCallback();
        }
      }
    });

    return () => {
      off();
      if (timerRef.current) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
      pendingRef.current = false;
    };
  }, [repoPath, debouncedCallback]);
}
