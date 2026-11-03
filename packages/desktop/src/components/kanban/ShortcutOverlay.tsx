import { useCallback, useEffect, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { Keyboard, X } from "lucide-react";

export interface ShortcutOverlayProps {
  isOpen: boolean;
  onClose: () => void;
}

interface ShortcutEntry {
  keys: string[];
  description: string;
}

interface ShortcutCategory {
  title: string;
  shortcuts: ShortcutEntry[];
}

const SHORTCUT_CATEGORIES: ShortcutCategory[] = [
  {
    title: "Navigation",
    shortcuts: [
      { keys: ["↑", "↓", "←", "→"], description: "Navigate between cards" },
      { keys: ["/"], description: "Focus search" },
      { keys: ["⌘K"], description: "Command palette" },
    ],
  },
  {
    title: "Card Actions",
    shortcuts: [
      { keys: ["C"], description: "Create card" },
      { keys: ["E"], description: "Edit title" },
      { keys: ["Enter"], description: "Open detail panel" },
      { keys: ["D"], description: "Delete card" },
      { keys: ["⌫"], description: "Delete card" },
      { keys: ["X"], description: "Toggle selection" },
      { keys: ["0"], description: "Set priority: none" },
      { keys: ["1"], description: "Set priority: critical" },
      { keys: ["2"], description: "Set priority: high" },
      { keys: ["3"], description: "Set priority: medium" },
      { keys: ["4"], description: "Set priority: low" },
    ],
  },
  {
    title: "Board Actions",
    shortcuts: [
      { keys: ["Esc"], description: "Clear selection / Close panel" },
      { keys: ["?"], description: "Show this overlay" },
    ],
  },
];

/**
 * Keyboard shortcut help overlay for the Kanban board.
 *
 * Renders as a centered modal with a semi-transparent backdrop.
 * Shortcuts are grouped into Navigation, Card Actions, and Board Actions.
 * Closes on Escape, "?" press, or clicking the backdrop.
 */
export function ShortcutOverlay({ isOpen, onClose }: ShortcutOverlayProps) {
  useEffect(() => {
    if (!isOpen) return;

    function handleKeyDown(e: globalThis.KeyboardEvent) {
      if (e.key === "Escape" || e.key === "?") {
        e.preventDefault();
        onClose();
      }
    }

    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [isOpen, onClose]);

  const handleBackdropClick = useCallback(() => {
    onClose();
  }, [onClose]);

  const handlePanelKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLDivElement>) => {
      e.stopPropagation();
    },
    [],
  );

  if (!isOpen) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center"
      role="dialog"
      aria-modal="true"
      aria-labelledby="kanban-shortcut-overlay-title"
    >
      {/* Backdrop */}
      <div
        className="absolute inset-0 bg-black/50"
        onClick={handleBackdropClick}
        aria-hidden="true"
      />

      {/* Panel */}
      <div
        className="relative z-10 w-full max-w-md rounded-lg border border-gx-border bg-gx-surface shadow-xl"
        onKeyDown={handlePanelKeyDown}
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-gx-border px-4 py-3">
          <div className="flex items-center gap-2">
            <Keyboard size={14} className="text-gx-muted" />
            <h2
              id="kanban-shortcut-overlay-title"
              className="text-sm font-medium text-gx-text"
            >
              Keyboard Shortcuts
            </h2>
          </div>
          <button
            type="button"
            className="btn-icon"
            onClick={onClose}
            aria-label="Close shortcuts overlay"
          >
            <X size={14} />
          </button>
        </div>

        {/* Content */}
        <div className="space-y-4 px-4 py-3">
          {SHORTCUT_CATEGORIES.map((category) => (
            <div key={category.title}>
              <h3 className="mb-2 text-[11px] font-medium uppercase tracking-wide text-gx-muted">
                {category.title}
              </h3>
              <div className="space-y-1.5">
                {category.shortcuts.map((shortcut) => (
                  <div
                    key={shortcut.description}
                    className="flex items-center justify-between gap-3"
                  >
                    <span className="text-xs text-gx-text/80">
                      {shortcut.description}
                    </span>
                    <div className="flex shrink-0 items-center gap-1">
                      {shortcut.keys.map((key) => (
                        <kbd key={key} className="kbd">
                          {key}
                        </kbd>
                      ))}
                    </div>
                  </div>
                ))}
              </div>
            </div>
          ))}
        </div>

        {/* Footer hint */}
        <div className="border-t border-gx-border px-4 py-2">
          <p className="text-[10px] text-gx-muted">
            Press <kbd className="kbd">?</kbd> or <kbd className="kbd">Esc</kbd> to close
          </p>
        </div>
      </div>
    </div>
  );
}
