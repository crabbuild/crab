import { useCallback, useEffect, useState } from "react";

/**
 * Hook that manages the ⌘K shortcut for opening the fuzzy repo switcher.
 *
 * Only activates when a workspace is open. Avoids firing when the user
 * is focused inside a terminal (which uses ⌘K for "Clear Terminal") or
 * inside an editor (which uses ⌘K as a chord prefix).
 */
export function useFuzzySwitcherShortcut(workspaceActive: boolean) {
  const [open, setOpen] = useState(false);

  const openSwitcher = useCallback(() => setOpen(true), []);
  const closeSwitcher = useCallback(() => setOpen(false), []);

  useEffect(() => {
    if (!workspaceActive) return;

    const handler = (e: KeyboardEvent) => {
      const meta = e.metaKey || e.ctrlKey;
      if (!meta || e.shiftKey || e.altKey) return;
      if (e.key !== "k") return;

      // Don't intercept when focus is inside a terminal or editor element
      // that uses ⌘K for its own purposes (chord prefix, clear terminal).
      const target = e.target as HTMLElement | null;
      if (target) {
        const tag = target.tagName;
        // xterm terminals use a canvas or textarea inside .xterm container
        if (target.closest(".xterm")) return;
        // Monaco editor uses textarea inside .monaco-editor
        if (target.closest(".monaco-editor")) return;
        // Skip if inside an orchestrator view that has its own ⌘K handler
        if (target.closest("[data-orchestrator-palette]")) return;
        // Allow if it's a regular input/textarea not inside the above
        if (tag === "TEXTAREA" && !target.closest("[data-fuzzy-switcher]")) return;
      }

      e.preventDefault();
      setOpen(true);
    };

    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [workspaceActive]);

  return { open, openSwitcher, closeSwitcher };
}
