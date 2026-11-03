import { memo } from "react";
import { Handle, Position, NodeProps } from "@xyflow/react";
import clsx from "clsx";
import {
  CheckCircle2,
  Circle,
  Loader2,
  SkipForward,
  XCircle,
  Zap,
} from "lucide-react";
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
        "rounded border bg-gx-surface px-1.5 py-1 shadow-sm transition-all",
        "border-gx-border hover:border-gx-muted/50",
        selected && "!border-gx-accent ring-1 ring-gx-accent/40",
        hasCycle && !selected && "!border-red-500/60 ring-1 ring-red-500/30",
      )}
      style={{ width: 100 }}
    >
      <Handle
        type="target"
        position={Position.Top}
        className="!h-1.5 !w-1.5 !border !border-gx-border !bg-gx-surface"
      />

      <div className="flex items-center gap-1">
        <Icon
          size={8}
          className={clsx(config.color, status === "running" && "animate-spin")}
        />
        <span className="flex-1 truncate text-[10px] font-medium leading-none text-gx-text">
          {stage.name}
        </span>
        {hasCycle && (
          <span className="text-[8px] text-red-400" title="Part of a dependency cycle">⚠</span>
        )}
      </div>

      <div className="mt-0.5 truncate font-mono text-[8px] leading-none text-gx-muted">
        {stage.cmd.value}
      </div>

      <Handle
        type="source"
        position={Position.Bottom}
        className="!h-1.5 !w-1.5 !border !border-gx-border !bg-gx-surface"
      />
    </div>
  );
});
