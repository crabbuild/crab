import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
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

  if (!visible) return null;

  return (
    <Dialog
      open
      onOpenChange={(open) => {
      if (!open) onClose();
      }}
    >
      {/* Backdrop */}

      {/* Panel */}
      <DialogContent
        aria-describedby={undefined}
        showCloseButton={false}
        className={
          "w-full max-w-md rounded-lg border border-border bg-card shadow-xl" +
          " p-0 gap-0"
        }
        onKeyDown={(event) => {
          if (event.key === "?") onClose();
      }}
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-border px-4 py-3">
          <div className="flex items-center gap-2">
            <Keyboard size={14} className="text-muted-foreground" />
            <DialogTitle className="text-sm font-semibold text-foreground">
              Keyboard Shortcuts
            </DialogTitle>
          </div>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={onClose}
            aria-label="Close shortcuts overlay"
          >
            <X size={14} />
          </Button>
        </div>

        {/* Content */}
        <div className="px-4 py-3 space-y-4">
          {SHORTCUT_CATEGORIES.map((category) => (
            <div key={category.title}>
              <h3 className="mb-2 text-[11px] font-medium uppercase tracking-wide text-muted-foreground">
                {category.title}
              </h3>
              <div className="space-y-1.5">
                {category.shortcuts.map((shortcut) => (
                  <div
                    key={shortcut.description}
                    className="flex items-center justify-between gap-3"
                  >
                    <span className="text-[11px] text-foreground/80">
                      {shortcut.description}
                    </span>
                    <div className="flex shrink-0 items-center gap-1">
                      {shortcut.keys.map((key, idx) => (
                        <span key={idx}>
                          {idx > 0 && (
                            <span className="mx-0.5 text-[10px] text-muted-foreground">
                              /
                            </span>
                          )}
                          <kbd className="inline-block min-w-[20px] rounded border border-border bg-background px-1.5 py-0.5 text-center font-mono text-[10px] text-foreground">
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
        <div className="border-t border-border px-4 py-2">
          <p className="text-[9px] text-muted-foreground">
            Press{" "}
            <kbd className="rounded border border-border bg-background px-1 py-0.5 font-mono text-[9px]">
              ?
            </kbd>{" "}
            or{" "}
            <kbd className="rounded border border-border bg-background px-1 py-0.5 font-mono text-[9px]">
              Esc
            </kbd>{" "}
            to close
          </p>
        </div>
      </DialogContent>
    </Dialog>
  );
}
