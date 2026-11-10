import { useEffect, useRef, useState } from "react";
import type { Checkpoint, RestoreBackup } from "../../electron/orchestrator-checkpoints";
import { publishToast } from "./toasts";

/** Owns snapshot reads and file-restore feedback for one selected conversation. */
export function useWorkspaceSnapshots(task: { id: string; status: string }) {
  const [checkpoints, setCheckpoints] = useState<Checkpoint[]>([]);
  const [backups, setBackups] = useState<RestoreBackup[]>([]);
  const [loading, setLoading] = useState(true);
  const [restoring, setRestoring] = useState(false);
  const [error, setError] = useState<string>();
  const [revision, refresh] = useState(0);
  const owner = useRef<string | undefined>(undefined);
  owner.current = task.id;
  useEffect(() => {
    owner.current = task.id;
    setRestoring(false);
    return () => { if (owner.current === task.id) owner.current = undefined; };
  }, [task.id]);

  useEffect(() => {
    let active = true;
    setLoading(true);
    setCheckpoints([]);
    setBackups([]);
    setError(undefined);
    void window.crab.orchestrator.listCheckpoints(task.id).then(result => {
      if (!active) return;
      if (!result.success) throw new Error(result.error ?? "Could not load saved file states");
      setCheckpoints(result.checkpoints ?? []);
      setBackups(result.backups ?? []);
    }).catch((error: unknown) => {
      if (active) setError(error instanceof Error ? error.message : String(error));
    }).finally(() => { if (active) setLoading(false); });
    return () => { active = false; };
  }, [task.id, task.status, revision]);

  useEffect(() => {
    const checkpoint = window.crab.orchestrator.onCheckpointCreated(event => {
      if (event.taskId === task.id) refresh(value => value + 1);
    });
    const files = window.crab.orchestrator.onFilesChanged(event => {
      if (event.taskIds.includes(task.id)) refresh(value => value + 1);
    });
    return () => { checkpoint(); files(); };
  }, [task.id]);

  const restore = async (sourceRef: string): Promise<boolean> => {
    const taskId = task.id;
    setRestoring(true);
    setError(undefined);
    try {
      const result = await window.crab.orchestrator.restoreFiles(taskId, sourceRef);
      if (!result.success) throw new Error(result.error ?? "Could not restore files");
      if (owner.current !== taskId) return false;
      refresh(value => value + 1);
      publishToast({ kind: "success", title: "Files restored", body: "The previous file state is saved in Backups." });
      return true;
    } catch (error: unknown) {
      if (owner.current === taskId) setError(error instanceof Error ? error.message : String(error));
      return false;
    } finally { if (owner.current === taskId) setRestoring(false); }
  };
  return { checkpoints, backups, loading, restoring, error, restore, reload: () => refresh(value => value + 1) };
}
