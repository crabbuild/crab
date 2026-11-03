import { useCallback, useEffect, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { Keyboard, X } from "lucide-react";

// ── Types ────────────────────────────────────────────────────────────

export interface ShortcutOverlayProps {
  visible: boolean;
  onClose(): void;
}

// ── Shortcut data ────────────────────────────────────────────────────

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
      { keys: ["j", "k"], description: "Move focus to next / previous diff line" },
      { keys: ["n", "p"], description: "Jump to next / previous unresolved thread" },
      { keys: ["]", "["], description: "Jump to next / previous unviewed file" },
      { keys: ["?"], description: "Toggle this shortcut overlay" },
    ],
  },
  {
    title: "Commenting",
    shortcuts: [
      { keys: ["c"], description: "Open comment composer on focused line" },
    ],
  },
  {
    title: "Submission",
    shortcuts: [
      {
        keys: [navigator.platform?.includes("Mac") ? "⌘+Enter" : "Ctrl+Enter"],
        description: "Submit current composer / review dialog",
      },
      { keys: ["Escape"], description: "Close current composer / dialog" },
    ],
  },
];

// ── Component ────────────────────────────────────────────────────────

/**
 * Keyboard shortcut help overlay toggled via the `?` key.
 *
 * Renders as a centered modal with a backdrop. Shortcuts are grouped
 * into Navigation, Commenting, and Submission categories. Closes on
 * `?` press, Escape, or clicking the backdrop.
 */
export function ShortcutOverlay({ visible, onClose }: ShortcutOverlayProps) {
  // Close on `?` or Escape pressed anywhere in the document.
  useEffect(() => {
    if (!visible) return;

    function handleKeyDown(e: globalThis.KeyboardEvent) {
      if (e.key === "?" || e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    }

    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [visible, onClose]);

  const handleBackdropClick = useCallback(() => {
    onClose();
  }, [onClose]);

  const handlePanelKeyDown = useCallback(
    (e: ReactKeyboardEvent<HTMLDivElement>) => {
      // Prevent keydown from propagating to the navigator context.
      e.stopPropagation();
    },
    [],
  );

  if (!visible) return null;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center"
      role="dialog"
      aria-modal="true"
      aria-labelledby="shortcut-overlay-title"
    >
      {/* Backdrop */}
      <div
        className="absolute inset-0 bg-black/60"
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
            <h2 id="shortcut-overlay-title" className="text-sm font-semibold text-gx-text">
              Keyboard Shortcuts
            </h2>
          </div>
          <button
            type="button"
            className="rounded p-1 text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
            onClick={onClose}
            aria-label="Close shortcuts overlay"
          >
            <X size={14} />
          </button>
        </div>

        {/* Content */}
        <div className="px-4 py-3 space-y-4">
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
                    <span className="text-[11px] text-gx-text/80">
                      {shortcut.description}
                    </span>
                    <div className="flex shrink-0 items-center gap-1">
                      {shortcut.keys.map((key, idx) => (
                        <span key={idx}>
                          {idx > 0 && (
                            <span className="mx-0.5 text-[10px] text-gx-muted">/</span>
                          )}
                          <kbd className="inline-block min-w-[20px] rounded border border-gx-border bg-gx-bg-sub px-1.5 py-0.5 text-center font-mono text-[10px] text-gx-text">
                            {key}
                          </kbd>
                        </span>
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
          <p className="text-[9px] text-gx-muted">
            Press <kbd className="rounded border border-gx-border bg-gx-bg-sub px-1 py-0.5 font-mono text-[9px]">?</kbd> or <kbd className="rounded border border-gx-border bg-gx-bg-sub px-1 py-0.5 font-mono text-[9px]">Esc</kbd> to close
          </p>
        </div>
      </div>
    </div>
  );
}
