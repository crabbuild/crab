import { useCallback, useEffect, useSyncExternalStore } from "react";
import { parseRepoId } from "../../electron/ssh-types";
import { repositoryKey, type OpenRepo } from "../state";
import { fileBuffers } from "../lib/file-buffer-store";

export function fileBufferKey(repo: OpenRepo, path: string): string { return JSON.stringify([repositoryKey(repo), path]); }

/** Retain a physical working buffer independently of the mounted editor. */
export function useFileBuffer(repo: OpenRepo, path: string, working: boolean, available: boolean) {
  const remote = parseRepoId(repo.repo_id);
  const root = remote?.remotePath ?? (!repo.is_remote ? repo.path : undefined);
  const key = working && root ? fileBufferKey(repo, path) : null;
  const subscribe = useCallback((listener: () => void) => key ? fileBuffers.subscribe(key, listener) : () => {}, [key]);
  const snapshot = useCallback(() => fileBuffers.snapshot(key ?? ""), [key]);
  const state = useSyncExternalStore(subscribe, snapshot, snapshot);
  useEffect(() => {
    if (key && root) fileBuffers.bind(key, { repoId: repo.repo_id, fullPath: `${root.replace(/\/$/, "")}/${path}`, available, nativeSessionId: repo.native_session_id });
  }, [key, root, repo.repo_id, repo.native_session_id, path, available]);
  return {
    ...state,
    edit: useCallback((content: string) => { if (key) fileBuffers.edit(key, content); }, [key]),
    save: useCallback(() => { if (key) void fileBuffers.save(key); }, [key]),
    reload: useCallback((content: string) => { if (key) fileBuffers.reload(key, content); }, [key]),
    beginRead: useCallback(() => key ? fileBuffers.beginRead(key) : undefined, [key]),
    finishRead: useCallback((ticket: symbol | undefined, content?: string | null) => key && ticket ? fileBuffers.finishRead(key, ticket, content) : key === null, [key]),
  };
}
