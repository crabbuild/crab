import { useEffect } from "react";

interface ShortcutHandlers {
  /** Cmd+Shift+A — open the crab add dialog. */
  onAddShortcut?: () => void;
  /** Cmd+Shift+P — trigger push when crab remote detected. */
  onPushShortcut?: () => void;
}

/**
 * Register global keyboard shortcuts for the crab flow.
 * These are app-level shortcuts that work regardless of which
 * view is focused.
 */
export function useCrabShortcuts(handlers: ShortcutHandlers) {
  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      const meta = e.metaKey || e.ctrlKey;
      if (!meta || !e.shiftKey) return;

      switch (e.key.toLowerCase()) {
        case "a":
          if (handlers.onAddShortcut) {
            e.preventDefault();
            handlers.onAddShortcut();
          }
          break;
        case "p":
          // Only intercept Cmd+Shift+P for push when we have a handler.
          // Note: this conflicts with the command palette shortcut in
          // some editors. We only register it when the handler is provided
          // and the app has a crab remote configured.
          if (handlers.onPushShortcut) {
            e.preventDefault();
            handlers.onPushShortcut();
          }
          break;
      }
    }

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [handlers.onAddShortcut, handlers.onPushShortcut]);
}
