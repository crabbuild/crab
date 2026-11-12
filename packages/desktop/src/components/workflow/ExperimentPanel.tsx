import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import clsx from "clsx";
import {
  CheckCircle2,
  Clock,
  Loader2,
  Play,
  Plus,
  RefreshCw,
  Square,
  XCircle,
} from "lucide-react";
import { useCallback,useEffect,useState } from "react";
import { rpc } from "../../ipc";
import type { ExperimentEntry,ExpStatusKind } from "../../lib/workflow-types";

interface Props {
  repoId: string;
}

const statusConfig: Record<ExpStatusKind, { icon: typeof Clock; color: string; label: string }> = {
  pending: { icon: Clock, color: "text-muted-foreground", label: "Pending" },
  running: { icon: Loader2, color: "text-info", label: "Running" },
  done: { icon: CheckCircle2, color: "text-success", label: "Done" },
  failed: { icon: XCircle, color: "text-destructive", label: "Failed" },
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
      <div className="flex items-center gap-2 border-b border-border px-4 py-3">
        <Button variant="outline"
          type="button"
          className="flex items-center gap-1 text-xs"
          onClick={() => setShowQueue(!showQueue)}
        >
          <Plus size={12} /> Queue
        </Button>

        {pendingCount > 0 && (
          <Button variant="default"
            type="button"
            className="flex items-center gap-1 text-xs"
            onClick={handleStart}
          >
            <Play size={12} /> Start (jobs: {jobs})
          </Button>
        )}

        {runningCount > 0 && (
          <Button variant="outline"
            type="button"
            className="flex items-center gap-1 text-xs text-destructive"
            onClick={handleStop}
          >
            <Square size={12} /> Stop
          </Button>
        )}

        <div className="flex-1" />

        <Select
          value={":" + String(jobs)}
          onValueChange={(selectedValue) => {
            const value = selectedValue.slice(1);
            return setJobs(Number(value));
          }}
        >
          <SelectTrigger className="w-16 text-xs" title="Parallel jobs">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectGroup>
              {[1, 2, 3, 4, 6, 8].map((n) => (
                <SelectItem key={n} value={":" + String(n)}>
                  {n}
                </SelectItem>
              ))}
            </SelectGroup>
          </SelectContent>
        </Select>

        <Button variant="outline"
          type="button"
          className="flex items-center gap-1 text-xs"
          onClick={fetchExperiments}
          disabled={loading}
        >
          <RefreshCw size={12} className={loading ? "animate-spin" : ""} />
        </Button>
      </div>

      {/* Queue form */}
      {showQueue && (
        <div className="border-b border-border bg-background/50 px-4 py-3">
          <div className="mb-2 text-xs font-medium text-foreground">
            Queue Experiments
          </div>
          <div className="flex items-end gap-2">
            <div className="flex-1">
              <label className="mb-1 block text-[10px] text-muted-foreground">
                Parameter
              </label>
              <Input
                className="w-full text-xs"
                placeholder="train.lr"
                value={paramKey}
                onChange={(e) => setParamKey(e.target.value)}
              />
            </div>
            <div className="flex-1">
              <label className="mb-1 block text-[10px] text-muted-foreground">
                Values (comma-separated or range)
              </label>
              <Input
                className="w-full text-xs"
                placeholder="0.001, 0.01, 0.1 or range(0.001, 0.01, 0.003)"
                value={paramValues}
                onChange={(e) => setParamValues(e.target.value)}
              />
            </div>
            <Button variant="default"
              type="button"
              className="text-xs"
              onClick={handleEnqueue}
              disabled={!paramKey.trim() || !paramValues.trim()}
            >
              Queue
            </Button>
          </div>
          {paramValues.trim() && (
            <div className="mt-1 text-[10px] text-muted-foreground">
              Preview: {parseValues(paramValues.trim()).length} experiment(s)
            </div>
          )}
        </div>
      )}

      {/* Experiment list */}
      <div className="flex-1 overflow-y-auto">
        {experiments.length === 0 && !loading && (
          <div className="flex h-full flex-col items-center justify-center gap-2 text-sm text-muted-foreground">
            <div>No experiments queued.</div>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="text-xs text-primary hover:underline"
              onClick={() => setShowQueue(true)}
            >
              Queue your first experiment
            </Button>
          </div>
        )}

        {experiments.length > 0 && (
          <table className="w-full text-xs">
            <thead>
              <tr className="border-b border-border text-left text-muted-foreground">
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
                  <tr key={exp.id} className="border-b border-border/50 hover:bg-accent">
                    <td className="px-4 py-2 font-mono text-foreground">
                      {exp.id.slice(-8)}
                    </td>
                    <td className="px-4 py-2">
                      {Object.entries(exp.param_overrides).map(([k, v]) => (
                        <span key={k} className="mr-2">
                          <span className="text-muted-foreground">{k}=</span>
                          <span className="text-foreground">{v}</span>
                        </span>
                      ))}
                    </td>
                    <td className="px-4 py-2">
                      <span className={clsx("flex items-center gap-1", config.color)}>
                        <Icon size={12} className={exp.status === "running" ? "animate-spin" : ""} />
                        {config.label}
                      </span>
                    </td>
                    <td className="px-4 py-2 text-muted-foreground">
                      {exp.queued_at}
                    </td>
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
