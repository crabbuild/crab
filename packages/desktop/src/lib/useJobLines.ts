import { useMemo } from "react";
import { useStore } from "../state";
import { ProgressLine } from "../ipc";

// Filter the global log ring for a specific job id. Defined as a hook
// so components don't have to re-derive this list and can rely on
// memoisation when a job produces many lines.
export function useJobLines(jobId: string | null): ProgressLine[] {
  const { state } = useStore();
  return useMemo(() => {
    if (!jobId) return [];
    return state.logs.filter((l) => l.job_id === jobId);
  }, [state.logs, jobId]);
}
