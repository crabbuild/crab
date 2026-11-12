import { Button } from "@/components/ui/button";
/**
 * Shared collapsed one-liner for all tool cards.
 *
 * Renders a single clickable row: [icon/summary content] [chevron ›].
 * Used by every tool card when in collapsed state to provide a
 * consistent, compact representation.
 *
 * Animations use the `.chat-collapsible` CSS class and respect
 * `prefers-reduced-motion`. Hover state uses a subtle background tint.
 */

import { ChevronRight } from "lucide-react";
import type { ReactNode } from "react";

export interface CollapsedCardLineProps {
  /** The summary content to display (icon + text + optional badge). */
  summary: ReactNode;
  /** Called when the user clicks to expand the card. */
  onExpand: () => void;
  /** Accessible label describing the tool for screen readers. */
  ariaLabel?: string;
}

export function CollapsedCardLine({ summary, onExpand, ariaLabel }: CollapsedCardLineProps) {
  return (
    <Button
      variant="ghost"
      size="compact"
      type="button"
      onClick={onExpand}
      aria-expanded={false}
      aria-label={ariaLabel}
      className={
        "flex w-full items-center gap-2 px-2.5 py-1.5 "
        + "text-[10px] text-left rounded-md "
        + "bg-card/40 "
        + "hover:bg-accent/40 "
        + "cursor-pointer transition-colors "
        + "chat-collapsible"
      }
    >
      <span className="min-w-0 flex-1 flex items-center gap-2 truncate">
        {summary}
      </span>
      <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
    </Button>
  );
}
