import { useCallback, useEffect, useRef, useState } from "react";
import { PushFlowSummary, PushProgressEvent, PushStepInfo, PushUploadProgress, onAgentEvent, rpc, call } from "../ipc";

export type PushPhaseGroup = "finalize";
export const PUSH_STEPS = [{ index: 1, name: "push", label: "Publish repository", group: "finalize" as const }];
const steps = (status: PushStepInfo["status"]): PushStepInfo[] => [{ index: 1, name: "push", status, duration_ms: null }];

export type PushFlowState =
  | { phase: "idle" }
  | { phase: "running"; steps: PushStepInfo[]; currentStep: number; uploads: PushUploadProgress; jobId: string }
  | { phase: "done"; summary: PushFlowSummary; steps: PushStepInfo[] }
  | { phase: "error"; message: string; steps: PushStepInfo[] };

export function useCrabPushFlow() {
  const [state, setState] = useState<PushFlowState>({ phase: "idle" });
  const jobId = useRef<string | null>(null);

  useEffect(() => onAgentEvent(event => {
    if (event.method !== "crab/push_progress") return;
    const frame = event.params as PushProgressEvent;
    if (frame.job_id !== jobId.current || frame.event !== "progress") return;
    const data = frame.data;
    // Packing bytes are not network uploads; show only the CLI's upload phase here.
    if (data.operation !== "uploading") return;
    setState(previous => previous.phase !== "running" ? previous : {
      ...previous, uploads: {
        bytes_uploaded: Number(data.bytes ?? 0), bytes_total: Number(data.total_bytes) || null,
        xorbs_done: Number(data.current ?? 0), xorbs_total: Number(data.total) || null,
        throughput_bps: Number(data.rate_bytes_per_sec ?? 0),
      },
    });
  }), []);

  const start = useCallback((repoId: string, opts?: { remote?: string; force?: boolean }) => {
    const id = crypto.randomUUID();
    jobId.current = id;
    setState({ phase: "running", jobId: id, steps: steps("running"), currentStep: 1,
      uploads: { bytes_uploaded: 0, bytes_total: null, xorbs_done: 0, xorbs_total: null, throughput_bps: 0 } });
    void rpc.crabPushStructured(repoId, { ...opts, job_id: id }).then(result => {
      if (jobId.current !== id) return;
      const summary = result.summary;
      if (!summary) throw new Error("Crab returned no push result");
      setState({ phase: "done", summary, steps: [{ ...steps("done")[0], duration_ms: summary.duration_ms }] });
    }).catch(error => {
      if (jobId.current !== id) return;
      setState({ phase: "error", message: error instanceof Error ? error.message : String(error), steps: steps("error") });
    });
  }, []);
  const cancel = useCallback(() => {
    const id = jobId.current;
    jobId.current = null;
    if (id) void call("rpc/cancel", { op_id: id }).catch(() => undefined);
    setState({ phase: "error", message: "Cancelled by user", steps: steps("error") });
  }, []);
  const reset = useCallback(() => { jobId.current = null; setState({ phase: "idle" }); }, []);
  return { state, start, cancel, reset };
}
