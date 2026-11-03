import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for Skill and ToolSearch tool calls.
 *
 * Shows the skill being invoked or the tool being searched for,
 * with a distinctive "plugin" visual style.
 */


import clsx from "clsx";
import { ChevronDown, ChevronRight, Puzzle, SearchCode } from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";

// ── Component ───────────────────────────────────────────────────────

export function SkillCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const tool = event.tool ?? "";
  const input = event.input as Record<string, unknown> | undefined;
  const isToolSearch = tool === "ToolSearch";

  const skillName = String(input?.name ?? input?.skill ?? input?.query ?? "");
  const description = String(input?.description ?? input?.prompt ?? "");
  const output = event.detail ?? "";
  const hasOutput = output.length > 0 || description.length > 60;

  const Icon = isToolSearch ? SearchCode : Puzzle;
  const label = isToolSearch ? "ToolSearch" : "Skill";

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Icon size={11} className="shrink-0 text-gx-muted" />
        <span className="font-medium text-gx-text/80">{label}</span>
        <code className="rounded bg-gx-accent/10 px-1 py-0.5 text-[9px] text-gx-accent font-mono truncate">
          {skillName || "unnamed"}
        </code>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`${label}: ${skillName}`}
      />
    );
  }

  return (
    <div className="rounded-md border border-gx-border/40 overflow-hidden">
      <button
        type="button"
        onClick={() => hasOutput && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          hasOutput && "hover:bg-gx-surface-hover cursor-pointer",
          !hasOutput && "cursor-default",
        )}
      >
        <Icon size={11} className="text-gx-muted shrink-0" />
        <span className="shrink-0 font-medium text-gx-muted">{label}</span>

        {/* Skill name */}
        <span className="min-w-0 flex-1 flex items-center gap-1.5 truncate">
          <code className="rounded bg-gx-accent/10 px-1 py-0.5 text-[9px] text-gx-accent font-mono truncate">
            {skillName || "unnamed"}
          </code>
          {description && description.length <= 60 && (
            <span className="text-[9px] text-gx-muted/60 truncate">{description}</span>
          )}
        </span>

        {hasOutput && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-gx-muted/60" />
            : <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />
        )}
      </button>

      {expanded && hasOutput && (
        <div className="border-t border-gx-border/30 max-h-[200px] overflow-auto px-2.5 py-2">
          {description.length > 60 && (
            <p className="text-[10px] text-gx-text/70 leading-relaxed mb-1.5">{description}</p>
          )}
          {output && (
            <pre className="font-mono text-[10px] leading-[1.5] text-gx-text/60 whitespace-pre-wrap">
              {output.length > 3000 ? output.slice(0, 3000) + "\n… (truncated)" : output}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}
