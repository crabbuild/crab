/**
 * Pulsing caret rendered at the end of the streaming agent text node
 * while a turn has unfinished text (the most recent event is a `text`
 * delta and no `done` event has arrived yet).
 *
 * Uses the `.streaming-cursor` CSS class defined in index.css which
 * handles the pulsing animation and the `prefers-reduced-motion` gate.
 *
 * Auto-removes within one animation frame when `done` arrives for the
 * turn by transitioning from rendered to null via a rAF-gated effect.
 */

import { useEffect, useRef, useState } from "react";

export interface StreamingCursorProps {
  /** Whether the turn is still actively streaming (not complete). */
  active: boolean;
}

/**
 * Renders a thin pulsing caret span when `active` is true.
 *
 * When `active` transitions from true → false (i.e. `done` arrives),
 * the cursor is removed within one animation frame so the user sees
 * the text freeze cleanly without a lingering caret.
 */
export function StreamingCursor({ active }: StreamingCursorProps) {
  const [visible, setVisible] = useState(active);
  const rafRef = useRef<number | null>(null);

  useEffect(() => {
    if (active) {
      // Show immediately when streaming starts
      setVisible(true);
    } else {
      // Remove within one animation frame when done arrives
      rafRef.current = requestAnimationFrame(() => {
        setVisible(false);
      });
    }

    return () => {
      if (rafRef.current != null) {
        cancelAnimationFrame(rafRef.current);
      }
    };
  }, [active]);

  if (!visible) return null;

  return <span className="streaming-cursor" aria-hidden="true" />;
}
