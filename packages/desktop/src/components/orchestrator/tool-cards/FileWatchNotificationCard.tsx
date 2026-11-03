/**
 * FileWatchNotificationCard — subtle inline indicator for fs/changed events.
 *
 * Renders as a compact one-line notification when a watched file or
 * directory changes. Uses muted styling so it doesn't dominate the
 * chat but still communicates the file-system event.
 */

import clsx from "clsx";
import { FilePlus, FileEdit, FileX, FolderOpen } from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import type { AgentEvent } from "../../../../electron/orchestrator-adapters";

type Props = Omit<ToolCardRendererProps, "event"> & { event: AgentEvent };
import { CollapsedCardLine } from "./CollapsedCardLine";

const kindIcon = (kind: string) => {
  switch (kind) {
    case "created": return <FilePlus size={11} className="shrink-0 text-gx-success" />;
    case "modified": return <FileEdit size={11} className="shrink-0 text-gx-info" />;
    case "deleted": return <FileX size={11} className="shrink-0 text-gx-error" />;
    default: return <FolderOpen size={11} className="shrink-0 text-gx-muted" />;
  }
};

const kindLabel = (kind: string) => {
  switch (kind) {
    case "created": return "created";
    case "modified": return "modified";
    case "deleted": return "deleted";
    default: return "changed";
  }
};

export function FileWatchNotificationCard({
  event,
  collapsed: collapsedProp,
  onToggleCollapse,
  collapsedSummary,
}: Props) {
  const path = String(event.path ?? "");
  const kind = String(event.fsChangeKind ?? "changed");
  const watchId = String(event.fsWatchId ?? "");

  // Always collapsed by default — these are background notifications
  const collapsed = collapsedProp ?? true;

  const defaultSummary = (
    <>
      {kindIcon(kind)}
      <span className="font-medium text-gx-text/60 text-[10px]">{kindLabel(kind)}</span>
      <span className="truncate text-gx-text/40 text-[10px] font-mono">{path}</span>
    </>
  );

  // Collapsed one-liner
  if (collapsed) {
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`File ${kind}: ${path}`}
      />
    );
  }

  // Expanded (rarely used, but complete)
  return (
    <div className={clsx(
      "flex items-center gap-2 rounded-md border border-gx-border/20",
      "bg-gx-surface-hover/30 px-2.5 py-1.5",
    )}>
      {kindIcon(kind)}
      <span className="text-[10px] text-gx-text/60">
        <span className="font-medium">{kindLabel(kind)}</span>
        {" "}
        <code className="text-gx-text/40 font-mono">{path}</code>
      </span>
      {watchId && (
        <span className="text-[8px] text-gx-muted/50 font-mono ml-auto">
          {watchId.slice(0, 8)}
        </span>
      )}
    </div>
  );
}
