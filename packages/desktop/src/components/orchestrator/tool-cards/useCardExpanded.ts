import { useRef, useState } from "react";

/**
 * Manages the local "expanded" state for a tool card, synchronized
 * with the parent's `collapsed` prop.
 *
 * Problem: tool cards have two layers of collapse:
 *   1. The parent ToolGroup controls whether the card shows its
 *      one-liner (`collapsed = true`) or its full body.
 *   2. Within the full body, the card has a header that toggles
 *      whether the output/detail section is visible.
 *
 * When the user clicks the one-liner to expand (layer 1 → false),
 * they expect to immediately see the content — not have to click
 * again to expand layer 2. This hook auto-expands the local state
 * when the parent transitions from collapsed → expanded.
 */
export function useCardExpanded(
  collapsedProp: boolean | undefined,
  defaultExpanded: boolean,
): [boolean, (next: boolean) => void] {
  const [expanded, setExpanded] = useState(defaultExpanded);
  const prevCollapsedRef = useRef(collapsedProp);

  // When the parent expands this card from its collapsed one-liner,
  // auto-expand the output section so the user doesn't need a second
  // click to see the content they asked for.
  if (prevCollapsedRef.current === true && collapsedProp === false && !expanded) {
    setExpanded(true);
  }
  prevCollapsedRef.current = collapsedProp;

  return [expanded, setExpanded];
}
