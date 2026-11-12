import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
/**
 * Keyboard Shortcuts Dialog — displays all chat-scoped keyboard shortcuts
 * in a popover triggered from the chat header toolbar.
 *
 * Lists the six chat shortcuts defined in the design's keyboard shortcut
 * registry with their key combinations and descriptions.
 */

import { Keyboard, X } from "lucide-react";
import { useRef, useState } from "react";

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

  return (
    <div className="relative" ref={panelRef}>
      <Popover open={open} onOpenChange={setOpen}>
        <PopoverTrigger asChild>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            onClick={() => setOpen((o) => !o)}
            className="flex items-center gap-1 px-2 py-2 text-xs text-muted-foreground hover:text-foreground transition"
            title="Keyboard shortcuts"
            aria-label="Keyboard shortcuts"
            aria-haspopup="dialog"
            aria-expanded={open}
          >
            <Keyboard size={12} />
          </Button>
        </PopoverTrigger>

        {open && (
          <PopoverContent
            side="bottom"
            align="end"
            className=" w-72 rounded-lg border border-border bg-card "
            role="dialog"
            aria-label="Chat keyboard shortcuts"
          >
            {/* Header */}
            <div className="flex items-center justify-between border-b border-border px-3 py-2">
              <span className="text-xs font-medium text-foreground">
                Chat Keyboard Shortcuts
              </span>
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={() => setOpen(false)}
                className="flex h-5 w-5 items-center justify-center rounded text-muted-foreground hover:text-foreground hover:bg-accent transition"
                aria-label="Close"
              >
                <X size={11} />
              </Button>
            </div>

            {/* Shortcut list */}
            <div className="px-3 py-2 space-y-1.5">
              {CHAT_SHORTCUTS.map((shortcut) => (
                <div
                key={shortcut.keys}
                className="flex items-center justify-between gap-3 py-1"
              >
                  <span className="text-[11px] text-muted-foreground">
                    {shortcut.description}
                  </span>
                  <kbd className="shrink-0 rounded border border-border bg-background px-1.5 py-0.5 font-mono text-[10px] text-foreground">
                    {shortcut.keys}
                  </kbd>
                </div>
              ))}
            </div>

            {/* Footer hint */}
            <div className="border-t border-border/60 px-3 py-2">
              <p className="text-[10px] text-muted-foreground">
                Vim shortcuts (j/k) require Vim navigation to be enabled.
              </p>
            </div>
          </PopoverContent>
        )}
      </Popover>
    </div>
  );
}
