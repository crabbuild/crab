import { Bot, ChevronDown, ChevronRight, MessageSquare } from "lucide-react";
import { Streamdown } from "streamdown";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { CollapsedCardLine } from "./CollapsedCardLine";
import { useCardExpanded } from "./useCardExpanded";
import type { ToolCardRendererProps } from "./types";

export function SubAgentCard({ event, defaultExpanded = false, collapsed, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsed, defaultExpanded);
  const input = event.input as Record<string, unknown> | undefined;
  const tool = event.tool ?? "Agent";
  const message = tool === "SendMessage" || tool.includes("send_message");
  const Icon = message ? MessageSquare : Bot;
  const prompt = String(input?.prompt ?? input?.message ?? input?.content ?? "");
  const title = String(input?.description ?? input?.name ?? input?.subagent_type ?? input?.tool ?? input?.task_id ?? event.summary);
  const recipient = String(input?.recipient ?? input?.to ?? input?.agent_id ?? "");
  const output = event.detail ?? "";
  const failed = Boolean(input?.error);
  const completed = input?.final === true;
  const status = failed ? "Failed" : completed ? message ? "Delivered" : "Finished" : "Working";
  const summary = <><Icon size={12} className="shrink-0 text-muted-foreground" /><span className="min-w-0 flex-1 truncate">{title}</span><span className={cn("shrink-0 text-xs", failed ? "text-destructive" : "text-muted-foreground")}>{status}</span></>;
  if (collapsed) return <CollapsedCardLine summary={collapsedSummary ?? summary} onExpand={() => onToggleCollapse?.(false)} ariaLabel={`Agent: ${title}`} />;
  return <div className="overflow-hidden rounded-lg border border-border">
    <Button variant="ghost" size="compact" className="min-h-9 w-full justify-start gap-2 rounded-none px-3 text-left" aria-expanded={expanded} onClick={() => setExpanded(!expanded)}>
      {summary}{expanded ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
    </Button>
    {expanded && <div className="flex flex-col gap-4 border-t border-border px-3 py-3">
      {recipient && <p className="break-all text-xs text-muted-foreground">To: {recipient}</p>}
      {prompt && <div><p className="mb-1 text-xs font-medium text-muted-foreground">{message ? "Message" : "Instructions"}</p><p className="whitespace-pre-wrap break-words text-sm leading-relaxed">{prompt}</p></div>}
      {failed && <p role="alert" className="whitespace-pre-wrap break-words text-xs text-destructive">{String(input?.error)}</p>}
      {output && <div className="min-w-0"><p className="mb-2 text-xs font-medium text-muted-foreground">Result</p><div className="prose-agent break-words text-sm"><Streamdown shikiTheme={["github-light", "github-dark"]}>{output}</Streamdown></div></div>}
      {!output && !prompt && <p className="text-xs text-muted-foreground">Waiting for the agent's response.</p>}
    </div>}
  </div>;
}
