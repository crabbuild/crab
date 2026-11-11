import { useCallback, useEffect, useRef, useState } from "react";
import { AddFlowFileProgress, AddFlowSummary, AddProgressEvent, onAgentEvent, rpc, call } from "../ipc";

export type AddFlowState =
  | { phase: "idle" }
  | { phase: "running"; files: AddFlowFileProgress[]; bytesTotal: number; bytesDone: number; currentPhase: string; jobId: string }
  | { phase: "done"; summary: AddFlowSummary; files: AddFlowFileProgress[] }
  | { phase: "error"; message: string; files: AddFlowFileProgress[] };

export function useCrabAddFlow() {
  const [state, setState] = useState<AddFlowState>({ phase: "idle" });
  const jobId = useRef<string | null>(null);

  useEffect(() => onAgentEvent(event => {
    if (event.method !== "crab/add_progress") return;
    const frame = event.params as AddProgressEvent;
    if (frame.job_id !== jobId.current) return;
    const data = frame.data;
    setState(previous => {
      if (previous.phase !== "running") return previous;
      if (frame.event === "progress") {
        return { ...previous, bytesDone: Number(data.bytes ?? 0), bytesTotal: Number(data.total_bytes ?? 0), currentPhase: String(data.operation ?? "staging") };
      }
      if (frame.event !== "file_done") return previous;
      const size = Number(data.bytes ?? 0);
      const file: AddFlowFileProgress = {
        path: String(data.path), size, bytes_done: size,
        chunks: null, dedup_ratio: null,
        status: data.status === "failed" ? "error" : "done",
      };
      return { ...previous, files: [...previous.files.filter(item => item.path !== file.path), file] };
    });
  }), []);

  const start = useCallback((repoId: string, patterns: string[], opts?: { dryRun?: boolean }) => {
    const id = crypto.randomUUID();
    jobId.current = id;
    setState({ phase: "running", jobId: id, files: [], bytesDone: 0, bytesTotal: 0, currentPhase: "staging" });
    void rpc.crabAddStructured(repoId, patterns, { job_id: id, dry_run: opts?.dryRun }).then(result => {
      if (jobId.current !== id) return;
      const summary = result.summary;
      if (!summary) throw new Error("Crab returned no add result");
      setState(previous => ({ phase: "done", summary, files: previous.phase === "idle" ? [] : previous.files }));
    }).catch(error => {
      if (jobId.current !== id) return;
      setState(previous => ({ phase: "error", message: error instanceof Error ? error.message : String(error), files: previous.phase === "idle" ? [] : previous.files }));
    });
  }, []);

  const cancel = useCallback(() => {
    const id = jobId.current;
    jobId.current = null;
    if (id) void call("rpc/cancel", { op_id: id }).catch(() => undefined);
    setState(previous => ({ phase: "error", message: "Cancelled by user", files: previous.phase === "idle" ? [] : previous.files }));
  }, []);
  const reset = useCallback(() => { jobId.current = null; setState({ phase: "idle" }); }, []);
  return { state, start, cancel, reset };
}
