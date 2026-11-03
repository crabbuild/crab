import { memo, useCallback, useState } from "react";
import { Handle, Position, NodeProps } from "@xyflow/react";
import clsx from "clsx";
import { ChevronDown, ChevronRight, Layers } from "lucide-react";

interface ExpandedStageGroupData {
  templateName: string;
  expandedCount: number;
  collapsed: boolean;
  stages: string[];
  [key: string]: unknown;
}

export const ExpandedStageGroup = memo(function ExpandedStageGroup({ data }: NodeProps) {
  const { templateName, expandedCount, collapsed: initialCollapsed, stages } =
    data as unknown as ExpandedStageGroupData;

  const [collapsed, setCollapsed] = useState(initialCollapsed);

  const toggleCollapse = useCallback(() => {
    setCollapsed((prev) => !prev);
  }, []);

  return (
    <div
      className={clsx(
        "rounded-lg border-2 border-gx-border bg-gx-surface shadow-md transition-all",
        !collapsed && "min-w-[260px]",
      )}
    >
      <Handle
        type="target"
        position={Position.Top}
        className="!h-2.5 !w-2.5 !border-2 !border-gx-border !bg-gx-surface"
      />

      {/* Header */}
      <button
        type="button"
        className="flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-gx-surface-hover"
        onClick={toggleCollapse}
      >
        {collapsed ? (
          <ChevronRight size={12} className="text-gx-muted" />
        ) : (
          <ChevronDown size={12} className="text-gx-muted" />
        )}
        <Layers size={14} className="text-gx-accent" />
        <span className="flex-1 truncate text-sm font-medium text-gx-text">
          {templateName}
        </span>
        <span className="inline-flex h-5 min-w-5 items-center justify-center rounded-full bg-gx-accent/20 px-1.5 text-[10px] font-bold text-gx-accent">
          {expandedCount}
        </span>
      </button>

      {/* Expanded child stages */}
      {!collapsed && (
        <div className="border-t border-gx-border px-3 py-2">
          <div className="space-y-1">
            {stages.map((stageName) => (
              <div
                key={stageName}
                className="flex items-center gap-2 rounded bg-gx-bg px-2 py-1.5"
              >
                <div className="h-1.5 w-1.5 rounded-full bg-gx-muted" />
                <span className="truncate font-mono text-xs text-gx-text">
                  {stageName}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}

      <Handle
        type="source"
        position={Position.Bottom}
        className="!h-2.5 !w-2.5 !border-2 !border-gx-border !bg-gx-surface"
      />
    </div>
  );
});
