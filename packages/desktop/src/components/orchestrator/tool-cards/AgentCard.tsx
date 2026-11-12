import { Button } from "@/components/ui/button";
import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for Agent/subagent tool calls (Claude Code protocol).
 *
 * Handles: Agent, SendMessage, TeamCreate, TeamDelete
 *
 * For Agent/SendMessage:
 *   - input.prompt / input.message — the instruction given to the sub-agent
 *   - input.type / input.agent_type — the role/type of the sub-agent
 *   - detail — populated via tool_result when the agent returns
 *
 * Status:
 *   - Has detail → completed (agent returned)
 *   - No detail → spawned (still working, or historical with no result captured)
 */

import clsx from "clsx";
import {
  ArrowRight,
  Bot,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Send,
  UserMinus,
  Users,
} from "lucide-react";
import { Streamdown } from "streamdown";
import { CollapsedCardLine } from "./CollapsedCardLine";
import type { ToolCardRendererProps } from "./types";

// ── Component ───────────────────────────────────────────────────────

export function AgentCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const tool = event.tool ?? "";
  const input = event.input as Record<string, unknown> | undefined;

  // Extract relevant fields based on tool type.
  const prompt = String(input?.prompt ?? input?.message ?? input?.task ?? input?.description ?? "");
  const agentType = String(input?.type ?? input?.agent_type ?? input?.role ?? "");
  const agentId = String(input?.agent_id ?? input?.id ?? "");
  const output = event.detail ?? "";
  const hasOutput = output.length > 0;

  // Icon and label based on tool.
  let Icon = Bot;
  let label = "Agent";

  switch (tool) {
    case "SendMessage":
      Icon = Send;
      label = "Message";
      break;
    case "TeamCreate":
      Icon = Users;
      label = "Team";
      break;
    case "TeamDelete":
      Icon = UserMinus;
      label = "Team";
      break;
  }

  // Summary text for the header.
  const summary = agentType
    ? `${agentType}${agentId ? ` (${agentId.slice(0, 8)})` : ""}`
    : prompt.slice(0, 60) + (prompt.length > 60 ? "…" : "");

  // Status: completed if we have output, otherwise spawned.
  const isCompleted = hasOutput || tool === "TeamDelete" || tool === "TeamCreate";
  const statusLabel = tool === "SendMessage" ? "sent"
    : tool === "TeamCreate" ? "created"
    : tool === "TeamDelete" ? "disbanded"
    : isCompleted ? "completed" : "spawned";
  const statusColor = isCompleted ? "text-success" : "text-info";
  const statusBg = isCompleted
    ? "bg-success/10 border-success/30"
    : "bg-info/10 border-info/30";

  const canExpand = hasOutput || prompt.length > 80;

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Icon size={11} className="shrink-0 text-info" />
        <span className="font-medium text-foreground/80">{label}</span>
        <span className="truncate text-foreground/70">{summary}</span>
        <span className={clsx("shrink-0 text-[9px] font-medium", statusColor)}>
          {statusLabel}
        </span>
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
    <div className="rounded-md border border-border/40 overflow-hidden">
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
        <Icon size={12} className="text-info shrink-0" />
        <span className="shrink-0 font-medium text-muted-foreground">
          {label}
        </span>
        <span className="min-w-0 flex-1 truncate text-foreground/80">
          {summary}
        </span>

        <span className={clsx(
          "shrink-0 inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[9px] font-medium border",
          statusBg,
          statusColor,
        )}>
          {isCompleted
            ? <CheckCircle2 size={9} />
            : <ArrowRight size={9} />
          }
          {statusLabel}
        </span>

        {canExpand &&
          (expanded ? (
            <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
          ) : (
            <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
          ))}
      </Button>

      {expanded && canExpand && (
        <div className="border-t border-border/30 max-h-[300px] overflow-auto px-2.5 py-2 space-y-2">
          {prompt.length > 80 && (
            <div>
              <span className="text-[9px] font-medium text-muted-foreground uppercase tracking-wider">
                Prompt
              </span>
              <p className="mt-0.5 text-[10px] leading-relaxed text-foreground/70 whitespace-pre-wrap">
                {prompt}
              </p>
            </div>
          )}
          {hasOutput && (
            <div>
              <span className="text-[9px] font-medium text-muted-foreground uppercase tracking-wider">
                Result
              </span>
              <div className="mt-0.5">
                <AgentTextContent text={output.length > 5000 ? output.slice(0, 5000) + "\n\n… (truncated)" : output} />
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

// ── Content renderer ────────────────────────────────────────────────

function looksLikeMarkdown(text: string): boolean {
  let signals = 0;
  if (/^#{1,6}\s+/m.test(text)) signals++;
  if (/^\s*[-*]\s+/m.test(text)) signals++;
  if (/^```/m.test(text)) signals++;
  if (/\[[^\]]+\]\([^)]+\)/.test(text)) signals++;
  if (/\*\*[^*]+\*\*|__[^_]+__/.test(text)) signals++;
  return signals >= 2;
}

function AgentTextContent({ text }: { text: string }) {
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
    <pre className="font-mono text-[10px] leading-[1.5] text-foreground/70 whitespace-pre-wrap">
      {text}
    </pre>
  );
}
