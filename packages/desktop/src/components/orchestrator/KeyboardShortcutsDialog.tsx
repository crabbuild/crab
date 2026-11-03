/**
 * Keyboard Shortcuts Dialog — displays all chat-scoped keyboard shortcuts
 * in a popover triggered from the chat header toolbar.
 *
 * Lists the six chat shortcuts defined in the design's keyboard shortcut
 * registry with their key combinations and descriptions.
 */

import { useEffect, useRef, useState } from "react";
import { Keyboard, X } from "lucide-react";

// ── Shortcut definitions ────────────────────────────────────────────

interface ShortcutEntry {
  keys: string;
  description: string;
}

const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.userAgent);
const mod = isMac ? "⌘" : "Ctrl";

const CHAT_SHORTCUTS: ShortcutEntry[] = [
  { keys: `${mod}]`, description: "Expand focused card or group" },
  { keys: `${mod}[`, description: "Collapse focused card or group" },
  { keys: `${mod}E`, description: "Expand all in current turn" },
  { keys: `${mod}⇧E`, description: "Collapse all in current turn" },
  { keys: "j", description: "Next card (Vim navigation)" },
  { keys: "k", description: "Previous card (Vim navigation)" },
];

// ── Component ───────────────────────────────────────────────────────

export function KeyboardShortcutsButton() {
  const [open, setOpen] = useState(false);
  const panelRef = useRef<HTMLDivElement>(null);

  // Close on outside click or Escape.
  useEffect(() => {
    if (!open) return;
    const handleClick = (e: MouseEvent) => {
      if (panelRef.current && !panelRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    const handleKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", handleClick);
    document.addEventListener("keydown", handleKey);
    return () => {
      document.removeEventListener("mousedown", handleClick);
      document.removeEventListener("keydown", handleKey);
    };
  }, [open]);

  return (
    <div className="relative" ref={panelRef}>
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="flex items-center gap-1 px-2 py-2 text-xs text-gx-muted hover:text-gx-text transition"
        title="Keyboard shortcuts"
        aria-label="Keyboard shortcuts"
        aria-haspopup="dialog"
        aria-expanded={open}
      >
        <Keyboard size={12} />
      </button>

      {open && (
        <div
          className="absolute right-0 top-full z-50 mt-1 w-72 rounded-lg border border-gx-border bg-gx-surface shadow-xl"
          role="dialog"
          aria-label="Chat keyboard shortcuts"
        >
          {/* Header */}
          <div className="flex items-center justify-between border-b border-gx-border px-3 py-2">
            <span className="text-xs font-medium text-gx-text">Chat Keyboard Shortcuts</span>
            <button
              type="button"
              onClick={() => setOpen(false)}
              className="flex h-5 w-5 items-center justify-center rounded text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover transition"
              aria-label="Close"
            >
              <X size={11} />
            </button>
          </div>

          {/* Shortcut list */}
          <div className="px-3 py-2 space-y-1.5">
            {CHAT_SHORTCUTS.map((shortcut) => (
              <div
                key={shortcut.keys}
                className="flex items-center justify-between gap-3 py-1"
              >
                <span className="text-[11px] text-gx-muted">{shortcut.description}</span>
                <kbd className="shrink-0 rounded border border-gx-border bg-gx-bg-sub px-1.5 py-0.5 font-mono text-[10px] text-gx-text">
                  {shortcut.keys}
                </kbd>
              </div>
            ))}
          </div>

          {/* Footer hint */}
          <div className="border-t border-gx-border/60 px-3 py-2">
            <p className="text-[10px] text-gx-muted">
              Vim shortcuts (j/k) require Vim navigation to be enabled.
            </p>
          </div>
        </div>
      )}
    </div>
  );
}
