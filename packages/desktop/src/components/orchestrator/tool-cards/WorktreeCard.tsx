/**
 * Specialized renderer for Worktree tool calls.
 *
 * Handles: EnterWorktree, ExitWorktree
 *
 * Shows worktree creation/switching with a git-branch visual style.
 */

import { ChevronRight, GitBranch, LogOut } from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";

// ── Component ───────────────────────────────────────────────────────

export function WorktreeCard({ event, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const tool = event.tool ?? "";
  const input = event.input as Record<string, unknown> | undefined;
  const isEnter = tool === "EnterWorktree";

  const path = String(input?.path ?? input?.worktree ?? input?.directory ?? "");
  const branch = String(input?.branch ?? input?.ref ?? "");

  const Icon = isEnter ? GitBranch : LogOut;
  const label = isEnter ? "Worktree" : "Exit Worktree";
  const description = isEnter
    ? (branch ? `→ ${branch}` : path ? `→ ${path}` : "creating isolated worktree")
    : "returning to main directory";

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Icon size={11} className="shrink-0 text-muted-foreground" />
        <span className="font-medium text-foreground/80">{label}</span>
        <span className="truncate text-foreground/70 font-mono">{description}</span>
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`${label}: ${description}`}
      />
    );
  }

  return (
    <div className="rounded-md border border-border/40 overflow-hidden">
      <div className="flex items-center gap-2 px-2.5 py-1.5 text-[10px]">
        <Icon size={11} className="text-muted-foreground shrink-0" />
        <span className="shrink-0 font-medium text-muted-foreground">
          {label}
        </span>
        <span className="min-w-0 flex-1 truncate text-foreground/80 font-mono">
          {description}
        </span>
        {path && isEnter && (
          <span className="shrink-0 text-[9px] text-muted-foreground/60 truncate max-w-[120px]">
            {path}
          </span>
        )}
        <ChevronRight size={10} className="shrink-0 text-muted-foreground/40" />
      </div>
    </div>
  );
}
