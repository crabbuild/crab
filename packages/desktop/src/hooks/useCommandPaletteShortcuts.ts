import { useEffect, useRef } from "react";
import { executeCommand, getCommand } from "../lib/command-registry";
import type { CommandContext } from "../lib/command-registry";

export interface CommandPaletteShortcutOptions {
  /** Current command context for checking isEnabled and executing commands. */
  context: CommandContext;
  /** Callback to open the command palette overlay. */
  onOpenPalette: () => void;
  /** Whether the Changes view is currently focused/active. */
  isChangesViewFocused: boolean;
}

/**
 * Registers global keyboard shortcuts for the command palette and
 * direct command execution. Shortcuts check `isEnabled` before
 * executing; if disabled, the shortcut is a silent no-op (no toast).
 *
 * Bindings:
 * - Cmd+Shift+P / Ctrl+Shift+P → open CommandPalette
 * - Cmd+Shift+K / Ctrl+Shift+K → execute "Git: Push" directly
 * - Cmd+Shift+L / Ctrl+Shift+L → execute "Git: Pull" directly
 * - Cmd+Enter / Ctrl+Enter     → execute "Git: Commit" (only when Changes view is focused)
 */
export function useCommandPaletteShortcuts({
  context,
  onOpenPalette,
  isChangesViewFocused,
}: CommandPaletteShortcutOptions) {
  // Use refs to avoid re-registering the listener on every context/callback change.
  const contextRef = useRef(context);
  contextRef.current = context;

  const onOpenPaletteRef = useRef(onOpenPalette);
  onOpenPaletteRef.current = onOpenPalette;

  const isChangesViewFocusedRef = useRef(isChangesViewFocused);
  isChangesViewFocusedRef.current = isChangesViewFocused;

  useEffect(() => {
    function openPalette(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && !e.altKey && e.key.toLowerCase() === "p") {
        e.preventDefault();
        onOpenPaletteRef.current();
      }
    }

    function handleKeyDown(e: KeyboardEvent) {
      const meta = e.metaKey || e.ctrlKey;
      if (!meta) return;

      // Cmd+Shift+K / Ctrl+Shift+K → Git: Push
      if (e.shiftKey && !e.altKey && e.key.toLowerCase() === "k") {
        e.preventDefault();
        const cmd = getCommand("git.push");
        if (cmd && cmd.isEnabled(contextRef.current)) {
          void executeCommand("git.push", contextRef.current);
        }
        return;
      }

      // Cmd+Shift+L / Ctrl+Shift+L → Git: Pull
      if (e.shiftKey && !e.altKey && e.key.toLowerCase() === "l") {
        e.preventDefault();
        const cmd = getCommand("git.pull");
        if (cmd && cmd.isEnabled(contextRef.current)) {
          void executeCommand("git.pull", contextRef.current);
        }
        return;
      }

      // Cmd+Enter / Ctrl+Enter → Git: Commit (only when Changes view is focused)
      if (!e.shiftKey && !e.altKey && e.key === "Enter" && isChangesViewFocusedRef.current) {
        e.preventDefault();
        const cmd = getCommand("git.commit");
        if (cmd && cmd.isEnabled(contextRef.current)) {
          void executeCommand("git.commit", contextRef.current);
        }
        return;
      }
    }

    // Controls scope bubbling keys for their own navigation. Opening the global
    // palette must work from those controls; Git mutations keep that boundary.
    window.addEventListener("keydown", openPalette, true);
    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.removeEventListener("keydown", openPalette, true);
      window.removeEventListener("keydown", handleKeyDown);
    };
  }, []);
}
