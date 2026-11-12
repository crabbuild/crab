import { Button } from "@/components/ui/button";
import { useCallback, useEffect, useState } from "react";
import clsx from "clsx";
import {
  CheckCircle2,
  AlertTriangle,
  Circle,
  Snowflake,
  Ban,
  RefreshCw,
  Play,
} from "lucide-react";
import { rpc } from "../../ipc";
import type {
  WorkflowStatusReport,
  StageStatusEntry,
} from "../../lib/workflow-types";

interface Props {
  repoId: string;
  workflowPath: string | null;
  onRunOutdated: (stages: string[]) => void;
  onRunAll: () => void;
}

const statusConfig: Record<
  string,
  { icon: typeof Circle; color: string; label: string }
> = {
  // snake_case keys (TypeScript type convention)
  up_to_date: { icon: CheckCircle2, color: "text-success", label: "Up to date" },
  outdated: { icon: AlertTriangle, color: "text-warning", label: "Outdated" },
  not_run: { icon: Circle, color: "text-muted-foreground", label: "Not run" },
  frozen: { icon: Snowflake, color: "text-info", label: "Frozen" },
  conditional_skip: { icon: Ban, color: "text-muted-foreground", label: "Skipped" },
  // PascalCase keys (serde default serialization from Rust backend)
  UpToDate: { icon: CheckCircle2, color: "text-success", label: "Up to date" },
  Outdated: { icon: AlertTriangle, color: "text-warning", label: "Outdated" },
  NotRun: { icon: Circle, color: "text-muted-foreground", label: "Not run" },
  Frozen: { icon: Snowflake, color: "text-info", label: "Frozen" },
  ConditionalSkip: { icon: Ban, color: "text-muted-foreground", label: "Skipped" },
};

const fallbackStatus = { icon: Circle, color: "text-muted-foreground", label: "Unknown" };

export function StatusDashboard({ repoId, workflowPath, onRunOutdated, onRunAll }: Props) {
  const [report, setReport] = useState<WorkflowStatusReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchStatus = useCallback(async () => {
    if (!workflowPath) return;
    setLoading(true);
    setError(null);
    try {
      const result = await rpc.workflowStatus(repoId, workflowPath);
      setReport(result);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [repoId, workflowPath]);

  useEffect(() => {
    fetchStatus();
  }, [fetchStatus]);

  const outdatedStages = report?.stages.filter((s) => s.status === "outdated") ?? [];
  const upToDateCount = report?.stages.filter((s) => s.status === "up_to_date").length ?? 0;
  const notRunCount = report?.stages.filter((s) => s.status === "not_run").length ?? 0;
  const frozenCount = report?.stages.filter((s) => s.status === "frozen").length ?? 0;

  if (!workflowPath) {
    return (
      <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
        No workflow file loaded.
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col">
      {/* Summary bar */}
      <div className="flex items-center gap-4 border-b border-border px-4 py-3">
        <div className="flex flex-1 items-center gap-3 text-xs">
          {upToDateCount > 0 && (
            <span className="flex items-center gap-1 text-success">
              <CheckCircle2 size={12} /> {upToDateCount} up-to-date
            </span>
          )}
          {outdatedStages.length > 0 && (
            <span className="flex items-center gap-1 text-warning">
              <AlertTriangle size={12} /> {outdatedStages.length} outdated
            </span>
          )}
          {notRunCount > 0 && (
            <span className="flex items-center gap-1 text-muted-foreground">
              <Circle size={12} /> {notRunCount} not run
            </span>
          )}
          {frozenCount > 0 && (
            <span className="flex items-center gap-1 text-info">
              <Snowflake size={12} /> {frozenCount} frozen
            </span>
          )}
        </div>

        <div className="flex items-center gap-2">
          {outdatedStages.length > 0 && (
            <Button variant="default"
              type="button"
              className="flex items-center gap-1 text-xs"
              onClick={() => onRunOutdated(outdatedStages.map((s) => s.name))}
            >
              <Play size={12} /> Run Outdated
            </Button>
          )}
          <Button variant="outline"
            type="button"
            className="flex items-center gap-1 text-xs"
            onClick={onRunAll}
          >
            <Play size={12} /> Run All
          </Button>
          <Button variant="outline"
            type="button"
            className="flex items-center gap-1 text-xs"
            onClick={fetchStatus}
            disabled={loading}
          >
            <RefreshCw size={12} className={loading ? "animate-spin" : ""} /> Refresh
          </Button>
        </div>
      </div>

      {/* Error state */}
      {error && (
        <div className="border-b border-border bg-destructive/10 px-4 py-2 text-xs text-destructive">
          {error}
        </div>
      )}

      {/* Stage table */}
      <div className="flex-1 overflow-y-auto">
        {report && (
          <table className="w-full text-xs">
            <thead>
              <tr className="border-b border-border text-left text-muted-foreground">
                <th className="px-4 py-2 font-medium">Stage</th>
                <th className="px-4 py-2 font-medium">Status</th>
                <th className="px-4 py-2 font-medium">Changed</th>
                <th className="px-4 py-2 font-medium text-right">Duration</th>
              </tr>
            </thead>
            <tbody>
              {report.stages.map((stage) => (
                <StageRow key={stage.name} stage={stage} />
              ))}
            </tbody>
          </table>
        )}

        {!report && !loading && !error && (
          <div className="flex h-full items-center justify-center text-sm text-muted-foreground">
            Run the pipeline once to see status.
          </div>
        )}
      </div>
    </div>
  );
}

function StageRow({ stage }: { stage: StageStatusEntry }) {
  const config = statusConfig[stage.status] ?? fallbackStatus;
  const Icon = config.icon;
  const [expanded, setExpanded] = useState(false);

  const hasDetails = stage.changed_deps.length > 0 || stage.changed_params.length > 0;

  return (
    <>
      <tr
        className={clsx(
          "border-b border-border/50 hover:bg-accent",
          hasDetails && "cursor-pointer",
        )}
        onClick={() => hasDetails && setExpanded(!expanded)}
      >
        <td className="px-4 py-2 font-mono text-foreground">{stage.name}</td>
        <td className="px-4 py-2">
          <span className={clsx("flex items-center gap-1.5", config.color)}>
            <Icon size={12} />
            {config.label}
          </span>
        </td>
        <td className="px-4 py-2 text-muted-foreground">
          {stage.changed_deps.length > 0 && (
            <span>{stage.changed_deps.length} dep{stage.changed_deps.length > 1 ? "s" : ""}</span>
          )}
          {stage.changed_params.length > 0 && (
            <span className="ml-2">
              {stage.changed_params.length} param{stage.changed_params.length > 1 ? "s" : ""}
            </span>
          )}
        </td>
        <td className="px-4 py-2 text-right text-muted-foreground">
          {stage.last_duration_ms != null
            ? `${(stage.last_duration_ms / 1000).toFixed(1)}s`
            : "—"}
        </td>
      </tr>
      {expanded && hasDetails && (
        <tr className="border-b border-border/50 bg-background/50">
          <td colSpan={4} className="px-6 py-2">
            {stage.changed_deps.length > 0 && (
              <div className="mb-1">
                <span className="text-muted-foreground">Changed deps: </span>
                {stage.changed_deps.map((d) => (
                  <span key={d} className="mr-2 font-mono text-warning">
                    {d}
                  </span>
                ))}
              </div>
            )}
            {stage.changed_params.length > 0 && (
              <div>
                <span className="text-muted-foreground">Changed params: </span>
                {stage.changed_params.map((p) => (
                  <span key={p.key} className="mr-2">
                    <span className="font-mono text-foreground">{p.key}</span>
                    <span className="text-muted-foreground">: </span>
                    <span className="text-destructive line-through">{p.old_value}</span>
                    <span className="text-muted-foreground"> → </span>
                    <span className="text-success">{p.new_value}</span>
                  </span>
                ))}
              </div>
            )}
          </td>
        </tr>
      )}
    </>
  );
}
