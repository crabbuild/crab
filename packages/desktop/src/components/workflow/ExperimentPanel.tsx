import { useCallback,useEffect,useState } from "react";
import clsx from "clsx";
import {
Play,
Square,
Plus,RefreshCw,
Clock,
CheckCircle2,
XCircle,
Loader2
} from "lucide-react";
import { rpc } from "../../ipc";
import type { ExperimentEntry,ExpStatusKind } from "../../lib/workflow-types";

interface Props {
  repoId: string;
}

const statusConfig: Record<ExpStatusKind, { icon: typeof Clock; color: string; label: string }> = {
  pending: { icon: Clock, color: "text-gx-muted", label: "Pending" },
  running: { icon: Loader2, color: "text-gx-info", label: "Running" },
  done: { icon: CheckCircle2, color: "text-gx-success", label: "Done" },
  failed: { icon: XCircle, color: "text-gx-error", label: "Failed" },
};

export function ExperimentPanel({ repoId }: Props) {
  const [experiments, setExperiments] = useState<ExperimentEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [showQueue, setShowQueue] = useState(false);
  const [paramKey, setParamKey] = useState("");
  const [paramValues, setParamValues] = useState("");
  const [jobs, setJobs] = useState(2);

  const fetchExperiments = useCallback(async () => {
    setLoading(true);
    try {
      const result = await rpc.workflowExpList(repoId);
      setExperiments(result.experiments);
    } catch {
      // Silently handle — queue might not exist yet.
      setExperiments([]);
    } finally {
      setLoading(false);
    }
  }, [repoId]);

  useEffect(() => {
    fetchExperiments();
  }, [fetchExperiments]);

  const handleEnqueue = useCallback(async () => {
    if (!paramKey.trim() || !paramValues.trim()) return;

    // Parse values — support range() syntax or comma-separated.
    const values = parseValues(paramValues.trim());
    const overrides = values.map((v) => ({ [paramKey.trim()]: v }));

    try {
      await rpc.workflowExpEnqueue(repoId, overrides);
      setParamKey("");
      setParamValues("");
      setShowQueue(false);
      await fetchExperiments();
    } catch (err) {
      // TODO: show error toast
      console.error("Failed to enqueue:", err);
    }
  }, [repoId, paramKey, paramValues, fetchExperiments]);

  const handleStart = useCallback(async () => {
    try {
      await rpc.workflowExpStart(repoId, jobs);
      await fetchExperiments();
    } catch (err) {
      console.error("Failed to start:", err);
    }
  }, [repoId, jobs, fetchExperiments]);

  const handleStop = useCallback(async () => {
    try {
      await rpc.workflowExpStop(repoId);
      await fetchExperiments();
    } catch (err) {
      console.error("Failed to stop:", err);
    }
  }, [repoId, fetchExperiments]);

  const runningCount = experiments.filter((e) => e.status === "running").length;
  const pendingCount = experiments.filter((e) => e.status === "pending").length;

  return (
    <div className="flex h-full flex-col">
      {/* Controls */}
      <div className="flex items-center gap-2 border-b border-gx-border px-4 py-3">
        <button
          type="button"
          className="btn flex items-center gap-1 text-xs"
          onClick={() => setShowQueue(!showQueue)}
        >
          <Plus size={12} /> Queue
        </button>

        {pendingCount > 0 && (
          <button
            type="button"
            className="btn btn-primary flex items-center gap-1 text-xs"
            onClick={handleStart}
          >
            <Play size={12} /> Start (jobs: {jobs})
          </button>
        )}

        {runningCount > 0 && (
          <button
            type="button"
            className="btn flex items-center gap-1 text-xs text-gx-error"
            onClick={handleStop}
          >
            <Square size={12} /> Stop
          </button>
        )}

        <div className="flex-1" />

        <select
          className="input-field w-16 text-xs"
          value={jobs}
          onChange={(e) => setJobs(Number(e.target.value))}
          title="Parallel jobs"
        >
          {[1, 2, 3, 4, 6, 8].map((n) => (
            <option key={n} value={n}>{n}</option>
          ))}
        </select>

        <button
          type="button"
          className="btn flex items-center gap-1 text-xs"
          onClick={fetchExperiments}
          disabled={loading}
        >
          <RefreshCw size={12} className={loading ? "animate-spin" : ""} />
        </button>
      </div>

      {/* Queue form */}
      {showQueue && (
        <div className="border-b border-gx-border bg-gx-bg/50 px-4 py-3">
          <div className="mb-2 text-xs font-medium text-gx-text">Queue Experiments</div>
          <div className="flex items-end gap-2">
            <div className="flex-1">
              <label className="mb-1 block text-[10px] text-gx-muted">Parameter</label>
              <input
                className="input-field w-full text-xs"
                placeholder="train.lr"
                value={paramKey}
                onChange={(e) => setParamKey(e.target.value)}
              />
            </div>
            <div className="flex-1">
              <label className="mb-1 block text-[10px] text-gx-muted">
                Values (comma-separated or range)
              </label>
              <input
                className="input-field w-full text-xs"
                placeholder="0.001, 0.01, 0.1 or range(0.001, 0.01, 0.003)"
                value={paramValues}
                onChange={(e) => setParamValues(e.target.value)}
              />
            </div>
            <button
              type="button"
              className="btn btn-primary text-xs"
              onClick={handleEnqueue}
              disabled={!paramKey.trim() || !paramValues.trim()}
            >
              Queue
            </button>
          </div>
          {paramValues.trim() && (
            <div className="mt-1 text-[10px] text-gx-muted">
              Preview: {parseValues(paramValues.trim()).length} experiment(s)
            </div>
          )}
        </div>
      )}

      {/* Experiment list */}
      <div className="flex-1 overflow-y-auto">
        {experiments.length === 0 && !loading && (
          <div className="flex h-full flex-col items-center justify-center gap-2 text-sm text-gx-muted">
            <div>No experiments queued.</div>
            <button
              type="button"
              className="text-xs text-gx-accent hover:underline"
              onClick={() => setShowQueue(true)}
            >
              Queue your first experiment
            </button>
          </div>
        )}

        {experiments.length > 0 && (
          <table className="w-full text-xs">
            <thead>
              <tr className="border-b border-gx-border text-left text-gx-muted">
                <th className="px-4 py-2 font-medium">ID</th>
                <th className="px-4 py-2 font-medium">Parameters</th>
                <th className="px-4 py-2 font-medium">Status</th>
                <th className="px-4 py-2 font-medium">Queued</th>
              </tr>
            </thead>
            <tbody>
              {experiments.map((exp) => {
                const config = statusConfig[exp.status];
                const Icon = config.icon;
                return (
                  <tr key={exp.id} className="border-b border-gx-border/50 hover:bg-gx-surface-hover">
                    <td className="px-4 py-2 font-mono text-gx-text">
                      {exp.id.slice(-8)}
                    </td>
                    <td className="px-4 py-2">
                      {Object.entries(exp.param_overrides).map(([k, v]) => (
                        <span key={k} className="mr-2">
                          <span className="text-gx-muted">{k}=</span>
                          <span className="text-gx-text">{v}</span>
                        </span>
                      ))}
                    </td>
                    <td className="px-4 py-2">
                      <span className={clsx("flex items-center gap-1", config.color)}>
                        <Icon size={12} className={exp.status === "running" ? "animate-spin" : ""} />
                        {config.label}
                      </span>
                    </td>
                    <td className="px-4 py-2 text-gx-muted">{exp.queued_at}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

/** Parse value expressions: comma-separated or range(start, stop, step). */
function parseValues(input: string): string[] {
  const rangeMatch = input.match(/^range\(\s*([\d.]+)\s*,\s*([\d.]+)\s*,\s*([\d.]+)\s*\)$/);
  if (rangeMatch) {
    const start = parseFloat(rangeMatch[1]);
    const stop = parseFloat(rangeMatch[2]);
    const step = parseFloat(rangeMatch[3]);
    const values: string[] = [];
    for (let v = start; v <= stop + step * 0.001; v += step) {
      values.push(v.toFixed(Math.max(
        (rangeMatch[1].split(".")[1] || "").length,
        (rangeMatch[3].split(".")[1] || "").length,
      )));
    }
    return values;
  }
  return input.split(",").map((s) => s.trim()).filter(Boolean);
}
