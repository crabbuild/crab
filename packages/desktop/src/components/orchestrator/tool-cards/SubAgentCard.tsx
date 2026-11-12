import { Button } from "@/components/ui/button";
import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for sub-agent/task tool calls (Claude Code protocol).
 *
 * Claude Code sub-agent lifecycle:
 *   1. `Task` tool_use — agent spawns a sub-agent with `input.description`
 *      The `detail` field is populated later via tool_result when the
 *      sub-agent returns its output.
 *   2. `TaskOutput` — intermediate streaming output from the sub-agent
 *   3. `TaskStop` — sub-agent has been terminated
 *
 * Status logic:
 *   - Task + no detail → "spawned" (running)
 *   - Task + detail → "completed" (sub-agent returned)
 *   - TaskOutput → "output" (intermediate result)
 *   - TaskStop → "stopped"
 */

import clsx from "clsx";
import {
  ArrowRight,
  Bot,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  MessageSquare,
  StopCircle,
} from "lucide-react";
import { Streamdown } from "streamdown";
import { CollapsedCardLine } from "./CollapsedCardLine";
import type { ToolCardRendererProps } from "./types";

// ── Component ───────────────────────────────────────────────────────

export function SubAgentCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const input = event.input as Record<string, unknown> | undefined;
  const description = String(input?.description ?? input?.task_id ?? input?.prompt ?? "");
  const output = event.detail ?? "";
  const hasOutput = output.length > 0;

  const tool = event.tool ?? "Task";

  // Status detection based on Claude Code protocol:
  //   - "Task" with detail populated → sub-agent returned (completed)
  //   - "Task" without detail → sub-agent still running (spawned)
  //   - "TaskOutput" → intermediate output from sub-agent
  //   - "TaskStop" → sub-agent explicitly stopped
  let statusLabel: string;
  let statusColor: string;
  let statusBg: string;
  let StatusIcon: typeof CheckCircle2;

  if (tool === "TaskStop") {
    StatusIcon = StopCircle;
    statusLabel = "stopped";
    statusColor = "text-muted-foreground";
    statusBg = "bg-accent border-border/50";
  } else if (tool === "TaskOutput") {
    StatusIcon = MessageSquare;
    statusLabel = "output";
    statusColor = "text-muted-foreground";
    statusBg = "bg-accent border-border/50";
  } else if (hasOutput) {
    // Task event with result populated — sub-agent has completed.
    StatusIcon = CheckCircle2;
    statusLabel = "completed";
    statusColor = "text-success";
    statusBg = "bg-success/10 border-success/30";
  } else {
    // Task event without result — sub-agent was spawned (may still be running).
    StatusIcon = ArrowRight;
    statusLabel = "spawned";
    statusColor = "text-info";
    statusBg = "bg-info/10 border-info/30";
  }

  const canExpand = hasOutput || description.length > 60;

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Bot size={11} className="shrink-0 text-info" />
        <span className="font-medium text-foreground/80">Agent</span>
        <span className="truncate text-foreground/70">
          {description.slice(0, 60)}
          {description.length > 60 ? "…" : ""}
        </span>
        <span className={clsx("shrink-0 text-[9px] font-medium", statusColor)}>
          {statusLabel}
        </span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Sub-agent: ${description.slice(0, 60)}`}
      />
    );
  }

  return (
    <div className="rounded-md border border-border/40 overflow-hidden">
      {/* Header */}
      <Button
        variant="ghost"
        size="compact"
        type="button"
        onClick={() => canExpand && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[11px] text-left transition",
          canExpand && "hover:bg-accent cursor-pointer",
          !canExpand && "cursor-default",
        )}
      >
        <Bot size={12} className="text-info shrink-0" />
        <span className="shrink-0 font-medium text-muted-foreground">
          Agent
        </span>

        {/* Description / prompt (truncated) */}
        <span className="min-w-0 flex-1 truncate text-foreground/80">
          {description.slice(0, 80)}
          {description.length > 80 ? "…" : ""}
        </span>

        {/* Status badge */}
        <span className={clsx(
          "shrink-0 inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[9px] font-medium border",
          statusBg,
          statusColor,
        )}>
          <StatusIcon size={9} />
          {statusLabel}
        </span>

        {canExpand &&
          (expanded ? (
            <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
          ) : (
            <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
          ))}
      </Button>

      {/* Expanded: full prompt and/or result */}
      {expanded && canExpand && (
        <div className="border-t border-border/30">
          {/* Show full prompt when truncated */}
          {description.length > 80 && (
            <div className="px-2.5 py-1.5 border-b border-border/20">
              <span className="text-[9px] font-medium text-muted-foreground uppercase tracking-wider">
                Prompt
              </span>
              <p className="mt-0.5 text-[10px] leading-relaxed text-foreground/70 whitespace-pre-wrap">
                {description}
              </p>
            </div>
          )}

          {/* Sub-agent result output */}
          {hasOutput && (
            <div className="max-h-[300px] overflow-auto px-2.5 py-2">
              <span className="text-[9px] font-medium text-muted-foreground uppercase tracking-wider">
                Result
              </span>
              <div className="mt-1">
                <SubAgentOutput output={output} />
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

// ── Output renderer ─────────────────────────────────────────────────

const OUTPUT_TRUNCATION_LIMIT = 5000;

/**
 * Heuristic: if the output has markdown signals (headings, lists,
 * code fences, links), render through Streamdown for rich display.
 */
function looksLikeMarkdown(text: string): boolean {
  let signals = 0;
  if (/^#{1,6}\s+/m.test(text)) signals++;
  if (/^\s*[-*]\s+/m.test(text)) signals++;
  if (/^```/m.test(text)) signals++;
  if (/\[[^\]]+\]\([^)]+\)/.test(text)) signals++;
  if (/\*\*[^*]+\*\*|__[^_]+__/.test(text)) signals++;
  return signals >= 2;
}

function SubAgentOutput({ output }: { output: string }) {
  const text = output.length > OUTPUT_TRUNCATION_LIMIT
    ? output.slice(0, OUTPUT_TRUNCATION_LIMIT) + "\n\n… (truncated)"
    : output;

  if (looksLikeMarkdown(text)) {
    return (
      <div className="prose-agent text-[11px]">
        <Streamdown shikiTheme={["github-light", "github-dark"]}>
          {text}
        </Streamdown>
      </div>
    );
  }

  return (
    <pre className="font-mono text-[10px] leading-[1.5] text-foreground/60 whitespace-pre-wrap">
      {text}
    </pre>
  );
}
