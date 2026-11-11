/**
 * Compact View Toggle — a small icon button in the chat header that
 * toggles between compact (auto-collapsed) and expanded view modes.
 *
 * Reads the current compactView state from the ChatUIState store for
 * the given task and writes via setCompactView on toggle.
 */

import { Minimize2, Maximize2 } from "lucide-react";
import { useChatUIState } from "../../lib/chat-ui-state";

const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.userAgent);
const mod = isMac ? "⌘" : "Ctrl";

interface CompactViewToggleProps {
  taskId: string;
}

export function CompactViewToggle({ taskId }: CompactViewToggleProps) {
  const store = useChatUIState();
  const compactView = store.byTask.get(taskId)?.compactView ?? true;

  const handleToggle = () => {
    store.setCompactView(taskId, !compactView);
  };

  const tooltip = compactView
    ? `Expand all cards (${mod}+Shift+E)`
    : `Compact view (${mod}+E)`;

  return (
    <button
      type="button"
      onClick={handleToggle}
      className="flex items-center gap-1 px-2 py-2 text-xs text-muted-foreground hover:text-foreground transition"
      title={tooltip}
      aria-label={tooltip}
      aria-pressed={compactView}
    >
      {compactView ? <Minimize2 size={12} /> : <Maximize2 size={12} />}
    </button>
  );
}
