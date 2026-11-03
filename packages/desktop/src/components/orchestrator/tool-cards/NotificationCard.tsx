import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for PushNotification and RemoteTrigger tool calls.
 *
 * PushNotification: Shows that the agent sent a desktop/push notification
 * to alert the user about a long-running task or scheduled event.
 *
 * RemoteTrigger: Shows that the agent created, updated, or ran a Routine
 * on claude.ai (schedule-based automation).
 */


import clsx from "clsx";
import { ChevronDown, ChevronRight, Bell, CalendarClock } from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";

// ── Component ───────────────────────────────────────────────────────

export function NotificationCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const tool = event.tool ?? "";
  const input = event.input as Record<string, unknown> | undefined;
  const isRemoteTrigger = tool === "RemoteTrigger";

  const title = String(
    input?.title ?? input?.name ?? input?.message ?? "",
  );
  const body = String(
    input?.body ?? input?.description ?? input?.content ?? input?.prompt ?? "",
  );
  const action = isRemoteTrigger
    ? String(input?.action ?? input?.operation ?? "manage")
    : "";
  const schedule = isRemoteTrigger
    ? String(input?.schedule ?? input?.cron ?? "")
    : "";

  const Icon = isRemoteTrigger ? CalendarClock : Bell;
  const label = isRemoteTrigger ? "Routine" : "Notification";
  const summary = title || body.slice(0, 60) || (isRemoteTrigger ? action : "sent");
  const hasDetail = body.length > 0 || schedule.length > 0;

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Icon size={11} className="shrink-0 text-gx-muted" />
        <span className="font-medium text-gx-text/80">{label}</span>
        <span className="truncate text-gx-text/70">{summary}</span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`${label}: ${summary}`}
      />
    );
  }

  return (
    <div className="rounded-md border border-gx-border/40 overflow-hidden">
      <button
        type="button"
        onClick={() => hasDetail && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          hasDetail && "hover:bg-gx-surface-hover cursor-pointer",
          !hasDetail && "cursor-default",
        )}
      >
        <Icon size={11} className="text-gx-muted shrink-0" />
        <span className="shrink-0 font-medium text-gx-muted">{label}</span>
        <span className="min-w-0 flex-1 truncate text-gx-text/80">
          {summary}
        </span>

        {isRemoteTrigger && action && (
          <span className="shrink-0 rounded bg-gx-info/10 px-1.5 py-0.5 text-[8px] font-medium text-gx-info border border-gx-info/20">
            {action}
          </span>
        )}

        {!isRemoteTrigger && (
          <span className="shrink-0 rounded bg-gx-warning/10 px-1.5 py-0.5 text-[8px] font-medium text-gx-warning border border-gx-warning/20">
            <Bell size={8} className="inline mr-0.5" />
            sent
          </span>
        )}

        {hasDetail && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-gx-muted/60" />
            : <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />
        )}
      </button>

      {expanded && hasDetail && (
        <div className="border-t border-gx-border/30 px-2.5 py-2 space-y-1.5">
          {body && (
            <p className="text-[10px] text-gx-text/70 leading-relaxed whitespace-pre-wrap">
              {body.length > 500 ? body.slice(0, 500) + "…" : body}
            </p>
          )}
          {schedule && (
            <div className="flex items-center gap-1.5 text-[9px] text-gx-muted">
              <CalendarClock size={9} />
              <span className="font-mono">{schedule}</span>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
