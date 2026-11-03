/**
 * Hook for detecting external modifications to a file that's open in
 * the editor. Subscribes to `fs/changed` events and re-reads the file
 * to detect if its content has changed on disk/remote.
 *
 * Behavior:
 * - If the editor buffer is clean (not dirty): silently reloads content
 * - If the editor buffer is dirty: shows a prompt with Reload / Keep Mine
 *
 * Works for both local and remote repos — uses `mainProcess.fsReadFile`
 * for local files and `rpc.readText` for remote files.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { onAgentEvent, mainProcess, rpc } from "../ipc";
import type { OpenRepo } from "../state";
import { getWatchPath } from "./useFileWatcher";

interface ExternalChangeState {
  /** Whether the "file changed externally" prompt is visible. */
  showPrompt: boolean;
  /** The new content from disk/remote (available when prompt is shown). */
  newContent: string | null;
}

interface UseExternalFileChangeOptions {
  repo: OpenRepo;
  /** Relative path within the repo. */
  filePath: string;
  /** Whether the editor buffer currently has unsaved changes. */
  isDirty: boolean;
  /** Current content in the editor buffer (used for comparison). */
  currentContent: string | null;
  /** Whether the file is in an editable state (working tree, canEdit). */
  enabled: boolean;
  /** Called when the file should be silently reloaded (buffer was clean). */
  onReload: (newContent: string) => void;
}

interface UseExternalFileChangeResult {
  /** Whether the external change prompt is visible. */
  showPrompt: boolean;
  /** Accept the external change — reload the file content. */
  acceptReload: () => void;
  /** Reject the external change — keep the current editor content. */
  keepMine: () => void;
}

export function useExternalFileChange({
  repo,
  filePath,
  isDirty,
  currentContent,
  enabled,
  onReload,
}: UseExternalFileChangeOptions): UseExternalFileChangeResult {
  const [state, setState] = useState<ExternalChangeState>({
    showPrompt: false,
    newContent: null,
  });

  // Keep stable refs to avoid re-subscribing on every render.
  const isDirtyRef = useRef(isDirty);
  isDirtyRef.current = isDirty;
  const currentContentRef = useRef(currentContent);
  currentContentRef.current = currentContent;
  const onReloadRef = useRef(onReload);
  onReloadRef.current = onReload;
  const enabledRef = useRef(enabled);
  enabledRef.current = enabled;

  // Debounce timer to avoid checking too frequently during rapid events.
  const checkTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const watchPath = getWatchPath(repo);

  const checkForChanges = useCallback(async () => {
    if (!enabledRef.current) return;

    let newContent: string | null = null;

    try {
      if (repo.is_remote) {
        // Remote: read via RPC
        const result = await rpc.readText(repo.repo_id, filePath);
        if (result.truncated || result.text === null) return;
        newContent = result.text;
      } else if (repo.path) {
        // Local: read from disk
        const fullPath = `${repo.path}/${filePath}`;
        const result = await mainProcess.fsReadFile(fullPath);
        if (!result.exists || result.content === null) return;
        newContent = result.content;
      } else {
        return;
      }
    } catch {
      // Read failed (connection lost, file deleted, etc.) — ignore.
      return;
    }

    // Compare with current editor content.
    if (newContent === currentContentRef.current) return;

    // Content differs — decide based on dirty state.
    if (!isDirtyRef.current) {
      // Buffer is clean: silently reload.
      onReloadRef.current(newContent);
    } else {
      // Buffer is dirty: show prompt.
      setState({ showPrompt: true, newContent });
    }
  }, [repo.is_remote, repo.repo_id, repo.path, filePath]);

  // Subscribe to fs/changed events.
  useEffect(() => {
    if (!watchPath || !enabled) return;

    const off = onAgentEvent((evt) => {
      if (evt.method === "fs/changed") {
        const params = evt.params as { repo_path?: string };
        if (params.repo_path === watchPath) {
          // Debounce: wait 500ms after the last event before checking.
          // This avoids redundant reads during rapid file changes and
          // prevents re-reading immediately after our own writes.
          if (checkTimerRef.current) clearTimeout(checkTimerRef.current);
          checkTimerRef.current = setTimeout(() => {
            checkTimerRef.current = null;
            void checkForChanges();
          }, 500);
        }
      }
    });

    return () => {
      off();
      if (checkTimerRef.current) {
        clearTimeout(checkTimerRef.current);
        checkTimerRef.current = null;
      }
    };
  }, [watchPath, enabled, checkForChanges]);

  const acceptReload = useCallback(() => {
    if (state.newContent !== null) {
      onReloadRef.current(state.newContent);
    }
    setState({ showPrompt: false, newContent: null });
  }, [state.newContent]);

  const keepMine = useCallback(() => {
    setState({ showPrompt: false, newContent: null });
  }, []);

  return { showPrompt: state.showPrompt, acceptReload, keepMine };
}
