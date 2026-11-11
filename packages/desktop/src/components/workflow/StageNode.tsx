import { memo } from "react";
import { Handle, Position, NodeProps } from "@xyflow/react";
import clsx from "clsx";
import {
  AlertTriangle,
  CheckCircle2,
  Circle,
  Loader2,
  SkipForward,
  XCircle,
  Zap,
} from "lucide-react";
import { STAGE_WIDTH, STAGE_HEIGHT } from "../../lib/workflow-graph";
import { WorkflowStage, StageRunStatus } from "../../lib/workflow-types";

interface StageNodeData {
  stage: WorkflowStage;
  status: StageRunStatus;
  selected: boolean;
  hasCycle?: boolean;
  [key: string]: unknown;
}

const statusConfig: Record<
  StageRunStatus,
  { icon: typeof Circle; color: string }
> = {
  pending: { icon: Circle, color: "text-gx-muted" },
  queued: { icon: Circle, color: "text-gx-warn" },
  running: { icon: Loader2, color: "text-gx-info" },
  cached: { icon: Zap, color: "text-gx-thinking" },
  done: { icon: CheckCircle2, color: "text-gx-success" },
  failed: { icon: XCircle, color: "text-gx-error" },
  skipped: { icon: SkipForward, color: "text-gx-muted" },
};

export const StageNode = memo(function StageNode({ data }: NodeProps) {
  const { stage, status, selected, hasCycle } = data as unknown as StageNodeData;
  const config = statusConfig[status];
  const Icon = config.icon;

  return (
    <div
      className={clsx(
        "rounded-lg border bg-gx-surface px-4 py-3 transition-colors",
        "border-gx-border hover:border-gx-muted/50",
        selected && "!border-gx-accent ring-1 ring-gx-accent/40",
        hasCycle && !selected && "!border-gx-error",
      )}
      style={{ width: STAGE_WIDTH, height: STAGE_HEIGHT }}
      title={`${stage.name}\n${stage.cmd.value}`}
    >
      <Handle
        type="target"
        position={Position.Top}
        className="!h-3 !w-3 !border !border-gx-border !bg-gx-surface"
      />

      <div className="flex items-center gap-2">
        <Icon
          size={14}
          className={clsx("shrink-0", config.color, status === "running" && "animate-spin")}
        />
        <span className="flex-1 truncate text-sm font-medium text-gx-text">
          {stage.name}
        </span>
        {hasCycle && (
          <AlertTriangle size={14} className="shrink-0 text-gx-error" aria-label="Dependency cycle" />
        )}
      </div>

      <div className="mt-2 line-clamp-2 h-8 break-all font-mono text-xs leading-4 text-gx-muted">
        {stage.cmd.value}
      </div>

      <div className="mt-2 flex items-center justify-between text-[11px] leading-4 text-gx-muted">
        <span>{stage.deps.length} {stage.deps.length === 1 ? "input" : "inputs"} · {stage.outs.length} {stage.outs.length === 1 ? "output" : "outputs"}</span>
        <span className={clsx("capitalize", config.color)}>{stage.frozen ? "Frozen" : status}</span>
      </div>

      <Handle
        type="source"
        position={Position.Bottom}
        className="!h-3 !w-3 !border !border-gx-border !bg-gx-surface"
      />
    </div>
  );
});
