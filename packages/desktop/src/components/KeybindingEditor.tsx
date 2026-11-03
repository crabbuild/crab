/**
 * Keybinding editor UI component.
 *
 * Lists all terminal actions with their current bindings, allows reassignment
 * with conflict detection and warning, applies changes immediately to all
 * open sessions, and provides a "Reset to Defaults" action.
 */

import { useCallback,useEffect,useRef,useState } from "react";
import { RotateCcw,AlertTriangle,Keyboard } from "lucide-react";
import clsx from "clsx";
import {
createKeybindingRegistry,
detectConflicts,ACTION_LABELS,
DEFAULT_BINDINGS,
type TerminalAction,
type TerminalKeybinding,
type KeybindingConflict
} from "../lib/terminal-keybindings";

// --- Types ---

interface KeybindingEditorProps {
  /** Application-level keybindings to check for conflicts. */
  appBindings?: string[];
  /** Callback when bindings change (to apply to open sessions). */
  onBindingsChange?: (bindings: TerminalKeybinding[]) => void;
  /** Whether the editor is visible. */
  visible?: boolean;
}

// --- Helpers ---

/**
 * Capture a key combination from a keyboard event.
 * Returns an Electron accelerator-format string.
 */
function captureKeyCombination(e: KeyboardEvent): string | null {
  // Ignore standalone modifier keys
  if (["Control", "Shift", "Alt", "Meta"].includes(e.key)) {
    return null;
  }

  const parts: string[] = [];
  if (e.ctrlKey || e.metaKey) parts.push("CmdOrCtrl");
  if (e.shiftKey) parts.push("Shift");
  if (e.altKey) parts.push("Alt");

  // Normalize key name
  let key = e.key;
  if (key === " ") key = "Space";
  if (key.length === 1) key = key.toUpperCase();
  if (e.code === "Backquote") key = "`";

  parts.push(key);
  return parts.join("+");
}

// --- Component ---

export function KeybindingEditor({
  appBindings = [],
  onBindingsChange,
  visible = true,
}: KeybindingEditorProps) {
  const [registry] = useState(() => createKeybindingRegistry());
  const [bindings, setBindings] = useState<TerminalKeybinding[]>(() => registry.getAll());
  const [editingAction, setEditingAction] = useState<TerminalAction | null>(null);
  const [conflicts, setConflicts] = useState<KeybindingConflict[]>([]);
  const [pendingKeys, setPendingKeys] = useState<string | null>(null);
  const captureRef = useRef<HTMLDivElement>(null);

  // Refresh bindings from registry
  const refreshBindings = useCallback(() => {
    const updated = registry.getAll();
    setBindings(updated);
    onBindingsChange?.(updated);
  }, [registry, onBindingsChange]);

  // Key capture for editing
  useEffect(() => {
    if (!editingAction) return;

    function handleKeyDown(e: KeyboardEvent) {
      e.preventDefault();
      e.stopPropagation();

      // Escape cancels editing
      if (e.key === "Escape") {
        setEditingAction(null);
        setPendingKeys(null);
        setConflicts([]);
        return;
      }

      const combo = captureKeyCombination(e);
      if (!combo) return;

      setPendingKeys(combo);

      // Check for conflicts (exclude the action being edited)
      const otherBindings = bindings.filter((b) => b.action !== editingAction);
      const detected = detectConflicts(combo, otherBindings, appBindings);
      setConflicts(detected);
    }

    window.addEventListener("keydown", handleKeyDown, true);
    return () => window.removeEventListener("keydown", handleKeyDown, true);
  }, [editingAction, bindings, appBindings]);

  function startEditing(action: TerminalAction) {
    setEditingAction(action);
    setPendingKeys(null);
    setConflicts([]);
  }

  function confirmBinding() {
    if (!editingAction || !pendingKeys) return;
    registry.setBinding(editingAction, pendingKeys);
    setEditingAction(null);
    setPendingKeys(null);
    setConflicts([]);
    refreshBindings();
  }

  function cancelEditing() {
    setEditingAction(null);
    setPendingKeys(null);
    setConflicts([]);
  }

  function resetAll() {
    registry.resetToDefaults();
    refreshBindings();
  }

  function resetAction(action: TerminalAction) {
    registry.resetAction(action);
    refreshBindings();
  }

  if (!visible) return null;

  return (
    <div className="flex flex-col gap-3 p-4" data-testid="keybinding-editor">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <Keyboard className="w-4 h-4 text-[var(--color-text-secondary)]" />
          <h3 className="text-sm font-medium text-[var(--color-text-primary)]">
            Terminal Keybindings
          </h3>
        </div>
        <button
          onClick={resetAll}
          className="flex items-center gap-1 px-2 py-1 text-xs rounded hover:bg-[var(--color-bg-tertiary)] text-[var(--color-text-secondary)]"
          title="Reset all keybindings to defaults"
        >
          <RotateCcw className="w-3 h-3" />
          Reset to Defaults
        </button>
      </div>

      {/* Binding list */}
      <div className="flex flex-col gap-1">
        {bindings.map((binding) => (
          <div
            key={binding.action}
            className={clsx(
              "flex items-center justify-between px-3 py-2 rounded",
              editingAction === binding.action
                ? "bg-[var(--color-bg-accent)] ring-1 ring-[var(--color-border-accent)]"
                : "hover:bg-[var(--color-bg-secondary)]",
            )}
          >
            {/* Action label */}
            <span className="text-sm text-[var(--color-text-primary)] min-w-[180px]">
              {ACTION_LABELS[binding.action]}
            </span>

            {/* Key binding display / editor */}
            <div className="flex items-center gap-2">
              {editingAction === binding.action ? (
                <div className="flex items-center gap-2" ref={captureRef}>
                  <kbd className="px-2 py-1 text-xs font-mono bg-[var(--color-bg-tertiary)] border border-[var(--color-border)] rounded min-w-[120px] text-center">
                    {pendingKeys || "Press keys..."}
                  </kbd>
                  {pendingKeys && (
                    <>
                      <button
                        onClick={confirmBinding}
                        className="px-2 py-1 text-xs bg-[var(--color-bg-accent)] text-[var(--color-text-accent)] rounded hover:opacity-80"
                      >
                        Save
                      </button>
                      <button
                        onClick={cancelEditing}
                        className="px-2 py-1 text-xs text-[var(--color-text-secondary)] hover:text-[var(--color-text-primary)]"
                      >
                        Cancel
                      </button>
                    </>
                  )}
                </div>
              ) : (
                <>
                  <button
                    onClick={() => startEditing(binding.action)}
                    className="px-2 py-1 text-xs font-mono bg-[var(--color-bg-tertiary)] border border-[var(--color-border)] rounded hover:border-[var(--color-border-accent)] min-w-[120px] text-center"
                  >
                    {binding.keys}
                  </button>
                  {!binding.isDefault && (
                    <button
                      onClick={() => resetAction(binding.action)}
                      className="p-1 text-[var(--color-text-tertiary)] hover:text-[var(--color-text-secondary)]"
                      title={`Reset to default (${DEFAULT_BINDINGS[binding.action]})`}
                    >
                      <RotateCcw className="w-3 h-3" />
                    </button>
                  )}
                </>
              )}
            </div>
          </div>
        ))}
      </div>

      {/* Conflict warnings */}
      {conflicts.length > 0 && (
        <div className="flex flex-col gap-1 p-2 rounded bg-yellow-500/10 border border-yellow-500/30">
          <div className="flex items-center gap-1 text-xs font-medium text-yellow-600 dark:text-yellow-400">
            <AlertTriangle className="w-3 h-3" />
            Conflicts detected
          </div>
          {conflicts.map((conflict, i) => (
            <span key={i} className="text-xs text-yellow-600 dark:text-yellow-400 pl-4">
              {conflict.conflictsWith}
            </span>
          ))}
        </div>
      )}
    </div>
  );
}
