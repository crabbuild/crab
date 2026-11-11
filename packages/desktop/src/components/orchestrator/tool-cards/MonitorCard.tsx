import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for Monitor tool calls.
 *
 * Shows the background monitoring command with a "live" indicator
 * and streaming output when available. Output is rendered with
 * syntax highlighting when it looks like structured content.
 */


import clsx from "clsx";
import { ChevronDown, ChevronRight, Radio, Eye } from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";
import { ShikiPreview } from "./ShikiPreview";

// ── Component ───────────────────────────────────────────────────────

export function MonitorCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const input = event.input as Record<string, unknown> | undefined;
  const command = String(input?.command ?? input?.script ?? "");
  const description = String(input?.description ?? input?.name ?? "");
  const output = event.detail ?? "";
  const hasOutput = output.length > 0;

  const displayText = description || command;

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Eye size={11} className="shrink-0 text-muted-foreground" />
        <span className="font-medium text-foreground/80">Monitor</span>
        <span className="truncate text-foreground/70 font-mono">{displayText}</span>
        <span className="shrink-0 flex items-center gap-0.5 text-[9px] text-success">
          <Radio size={8} /> live
        </span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Monitor: ${displayText}`}
      />
    );
  }

  return (
    <div className="rounded-md border border-border/40 overflow-hidden">
      <button
        type="button"
        onClick={() => hasOutput && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          hasOutput && "hover:bg-accent cursor-pointer",
          !hasOutput && "cursor-default",
        )}
      >
        <Eye size={11} className="text-muted-foreground shrink-0" />
        <span className="shrink-0 font-medium text-muted-foreground">Monitor</span>

        <span className="min-w-0 flex-1 truncate font-mono text-foreground/80">
          {displayText}
        </span>

        {/* Live indicator */}
        <span className="shrink-0 flex items-center gap-1 rounded bg-success/10 px-1.5 py-0.5 text-[8px] font-medium text-success border border-success/20">
          <Radio size={8} className="animate-pulse" />
          live
        </span>

        {hasOutput && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
            : <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
        )}
      </button>

      {expanded && hasOutput && (
        <div className="border-t border-border/30 max-h-[200px] overflow-auto">
          <MonitorOutput output={output} />
        </div>
      )}
    </div>
  );
}

// ── Output renderer ─────────────────────────────────────────────────

const OUTPUT_TRUNCATION_LIMIT = 3000;

function MonitorOutput({ output }: { output: string }) {
  const text = output.length > OUTPUT_TRUNCATION_LIMIT
    ? output.slice(0, OUTPUT_TRUNCATION_LIMIT) + "\n… (truncated)"
    : output;

  // Detect JSON output (common for structured monitoring).
  const trimmed = text.trimStart();
  if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
    try {
      JSON.parse(output.length > OUTPUT_TRUNCATION_LIMIT ? output : trimmed);
      return <ShikiPreview code={text} lang="json" className="[&_pre]:!py-2 [&_pre]:!px-2.5" />;
    } catch {
      // Not valid JSON, fall through.
    }
  }

  return (
    <pre className="px-2.5 py-2 font-mono text-[10px] leading-[1.5] text-foreground/60 whitespace-pre-wrap">
      {text}
    </pre>
  );
}
