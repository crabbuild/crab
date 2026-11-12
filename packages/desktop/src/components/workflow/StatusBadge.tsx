import clsx from "clsx";
import {
  CheckCircle2,
  AlertTriangle,
  Circle,
  Snowflake,
  Ban,
} from "lucide-react";
import type { StageStatusKind } from "../../lib/workflow-types";

interface Props {
  status: StageStatusKind | string;
  size?: number;
  showLabel?: boolean;
}

const config: Record<string, { icon: typeof Circle; color: string; label: string }> = {
  // snake_case keys (TypeScript type convention)
  up_to_date: { icon: CheckCircle2, color: "text-success", label: "Up to date" },
  outdated: { icon: AlertTriangle, color: "text-warning", label: "Outdated" },
  not_run: { icon: Circle, color: "text-muted-foreground", label: "Not run" },
  frozen: { icon: Snowflake, color: "text-info", label: "Frozen" },
  conditional_skip: { icon: Ban, color: "text-muted-foreground/60", label: "Skipped" },
  // PascalCase keys (serde default serialization from Rust backend)
  UpToDate: { icon: CheckCircle2, color: "text-success", label: "Up to date" },
  Outdated: { icon: AlertTriangle, color: "text-warning", label: "Outdated" },
  NotRun: { icon: Circle, color: "text-muted-foreground", label: "Not run" },
  Frozen: { icon: Snowflake, color: "text-info", label: "Frozen" },
  ConditionalSkip: { icon: Ban, color: "text-muted-foreground/60", label: "Skipped" },
};

const fallbackStatus = { icon: Circle, color: "text-muted-foreground", label: "Unknown" };

export function StatusBadge({ status, size = 12, showLabel = false }: Props) {
  const { icon: Icon, color, label } = config[status] ?? fallbackStatus;

  return (
    <span className={clsx("inline-flex items-center gap-1", color)} title={label}>
      <Icon size={size} />
      {showLabel && <span className="text-[10px]">{label}</span>}
    </span>
  );
}
