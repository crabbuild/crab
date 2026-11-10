/**
 * Hook for auto-saving remote file edits with debounce, failure
 * persistence, and reconnection retry.
 *
 * When editing a remote file, writes are sent via `rpc.writeText()`
 * with a 1-second debounce. On write failure (e.g., connection loss),
 * the content is persisted in an in-memory store and a "Save pending"
 * state is exposed. When the connection is restored (capabilities
 * change back to canEdit=true), the pending write is retried
 * automatically.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { rpc } from "../ipc";
import { useRepoCapabilities } from "./useRepoCapabilities";

/** In-memory store for pending saves that failed due to connection loss. */
const pendingSaves = new Map<string, { repoId: string; path: string; content: string }>();

/** Debounce interval for remote auto-save (ms). */
const REMOTE_SAVE_DEBOUNCE_MS = 1000;

export interface RemoteAutoSaveState {
  /** Whether a save is currently pending (failed and waiting for reconnection). */
  savePending: boolean;
  isSaving: boolean;
  /** Called on every content change — debounces the remote write. */
  handleChange: (content: string) => void;
  /** Called for immediate save (e.g., Cmd+S). */
  handleSave: (content: string) => void;
}

/**
 * Returns auto-save handlers for a remote file. The caller is
 * responsible for marking dirty state via `onDirtyChange`.
 */
export function useRemoteAutoSave(
  repoId: string,
  filePath: string,
  tabKey: string,
  onDirtyChange: (key: string, dirty: boolean) => void,
): RemoteAutoSaveState {
  const [savePending, setSavePending] = useState(false);
  const [activeWrites, setActiveWrites] = useState(0);
  const saveTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const pendingContentRef = useRef<string | null>(null);
  const capabilities = useRepoCapabilities();

  // Stable key for the pending saves map.
  const saveKey = `${repoId}::${filePath}`;

  // Check if there's already a pending save for this file on mount.
  useEffect(() => {
    if (pendingSaves.has(saveKey)) {
      setSavePending(true);
    }
  }, [saveKey]);

  // Write to remote via rpc.writeText.
  const writeToRemote = useCallback(
    async (text: string) => {
      const content = pendingContentRef.current ?? text;
      pendingContentRef.current = content;
      setActiveWrites(count => count + 1);
      try {
        await rpc.writeText(repoId, filePath, content);
        // Older acknowledgements cannot clear a newer pending buffer.
        if (pendingContentRef.current !== content) return;
        pendingContentRef.current = null;
        pendingSaves.delete(saveKey);
        setSavePending(false);
        onDirtyChange(tabKey, false);
      } catch (e) {
        // Write failed (likely connection loss). Persist locally.
        console.error("Remote save failed, persisting locally:", e);
        const pending = pendingContentRef.current;
        if (pending === null) return;
        pendingSaves.set(saveKey, { repoId, path: filePath, content: pending });
        setSavePending(true);
      } finally { setActiveWrites(count => count - 1); }
    },
    [repoId, filePath, saveKey, onDirtyChange, tabKey],
  );

  // Retry pending write when capabilities restore canEdit.
  useEffect(() => {
    if (!capabilities.canEdit) return;
    const pending = pendingSaves.get(saveKey);
    if (pending) {
      void writeToRemote(pending.content);
    }
  }, [capabilities.canEdit, saveKey, writeToRemote]);

  // Debounced change handler.
  const handleChange = useCallback(
    (content: string) => {
      pendingContentRef.current = content;
      onDirtyChange(tabKey, true);
      if (saveTimerRef.current) {
        clearTimeout(saveTimerRef.current);
      }
      saveTimerRef.current = setTimeout(() => {
        saveTimerRef.current = null;
        void writeToRemote(content);
      }, REMOTE_SAVE_DEBOUNCE_MS);
    },
    [writeToRemote, onDirtyChange, tabKey],
  );

  // Immediate save (Cmd+S).
  const handleSave = useCallback(
    (content: string) => {
      if (saveTimerRef.current) {
        clearTimeout(saveTimerRef.current);
        saveTimerRef.current = null;
      }
      pendingContentRef.current = content;
      void writeToRemote(content);
    },
    [writeToRemote],
  );

  // Flush pending content on unmount.
  useEffect(() => {
    return () => {
      if (saveTimerRef.current) {
        clearTimeout(saveTimerRef.current);
        saveTimerRef.current = null;
      }
      const pending = pendingContentRef.current;
      if (pending !== null) {
        // Persist to the pending saves map so it can be retried later.
        pendingSaves.set(saveKey, { repoId, path: filePath, content: pending });
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [repoId, filePath, saveKey]);

  return { savePending, isSaving: activeWrites > 0, handleChange, handleSave };
}

/**
 * Returns the number of files with pending saves. Useful for global
 * indicators.
 */
export function getPendingSaveCount(): number {
  return pendingSaves.size;
}
