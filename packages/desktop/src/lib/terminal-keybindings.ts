/**
 * Terminal keybinding manager.
 *
 * Defines all terminal actions, provides a registry with default bindings,
 * detects conflicts between bindings, and persists custom bindings to
 * localStorage.
 */

// --- Types ---

export type TerminalAction =
  | "copy"
  | "paste"
  | "clear"
  | "split-horizontal"
  | "split-vertical"
  | "new-tab"
  | "close-tab"
  | "next-tab"
  | "prev-tab"
  | "jump-prev-prompt"
  | "jump-next-prompt"
  | "search"
  | "toggle-quick-terminal"
  | "select-command-output";

export interface TerminalKeybinding {
  action: TerminalAction;
  keys: string; // Electron accelerator format, e.g. "CmdOrCtrl+Shift+C"
  isDefault: boolean;
}

export interface KeybindingConflict {
  action: TerminalAction;
  conflictsWith: string; // description of conflicting binding
  scope: "terminal" | "application";
}

// --- Constants ---

export const LS_KEYBINDINGS_KEY = "crab.termKeybindings";

export const ALL_TERMINAL_ACTIONS: TerminalAction[] = [
  "copy",
  "paste",
  "clear",
  "split-horizontal",
  "split-vertical",
  "new-tab",
  "close-tab",
  "next-tab",
  "prev-tab",
  "jump-prev-prompt",
  "jump-next-prompt",
  "search",
  "toggle-quick-terminal",
  "select-command-output",
];

export const ACTION_LABELS: Record<TerminalAction, string> = {
  copy: "Copy",
  paste: "Paste",
  clear: "Clear Terminal",
  "split-horizontal": "Split Horizontal",
  "split-vertical": "Split Vertical",
  "new-tab": "New Tab",
  "close-tab": "Close Tab",
  "next-tab": "Next Tab",
  "prev-tab": "Previous Tab",
  "jump-prev-prompt": "Jump to Previous Prompt",
  "jump-next-prompt": "Jump to Next Prompt",
  search: "Search",
  "toggle-quick-terminal": "Toggle Quick Terminal",
  "select-command-output": "Select Command Output",
};

export const DEFAULT_BINDINGS: Record<TerminalAction, string> = {
  copy: "CmdOrCtrl+Shift+C",
  paste: "CmdOrCtrl+Shift+V",
  clear: "CmdOrCtrl+K",
  "split-horizontal": "CmdOrCtrl+\\",
  "split-vertical": "CmdOrCtrl+Shift+|",
  "new-tab": "CmdOrCtrl+Shift+`",
  "close-tab": "CmdOrCtrl+W",
  "next-tab": "CmdOrCtrl+Shift+]",
  "prev-tab": "CmdOrCtrl+Shift+[",
  "jump-prev-prompt": "CmdOrCtrl+Up",
  "jump-next-prompt": "CmdOrCtrl+Down",
  search: "CmdOrCtrl+F",
  "toggle-quick-terminal": "Ctrl+`",
  "select-command-output": "CmdOrCtrl+Shift+A",
};

// --- Storage ---

interface KeybindingsStorage {
  version: number;
  bindings: Record<string, string>; // action → key combo
}

function loadBindings(): Record<TerminalAction, string> {
  try {
    const raw = localStorage.getItem(LS_KEYBINDINGS_KEY);
    if (raw) {
      const parsed: KeybindingsStorage = JSON.parse(raw);
      if (parsed.version === 1 && parsed.bindings) {
        // Merge with defaults (in case new actions were added)
        const merged = { ...DEFAULT_BINDINGS };
        for (const [action, keys] of Object.entries(parsed.bindings)) {
          if (ALL_TERMINAL_ACTIONS.includes(action as TerminalAction)) {
            merged[action as TerminalAction] = keys;
          }
        }
        return merged;
      }
    }
  } catch {
    // Fall through to defaults
  }
  return { ...DEFAULT_BINDINGS };
}

function saveBindings(bindings: Record<TerminalAction, string>): void {
  const storage: KeybindingsStorage = {
    version: 1,
    bindings: { ...bindings },
  };
  localStorage.setItem(LS_KEYBINDINGS_KEY, JSON.stringify(storage));
}

// --- Normalization ---

/**
 * Normalize a key combination string for comparison.
 * Sorts modifiers alphabetically and lowercases everything.
 */
export function normalizeKeybinding(keys: string): string {
  const parts = keys.split("+").map((p) => p.trim().toLowerCase());
  // Separate modifiers from the key
  const modifiers: string[] = [];
  const keyParts: string[] = [];
  const modifierSet = new Set(["ctrl", "cmd", "alt", "shift", "meta", "cmdorctrl", "super"]);
  for (const part of parts) {
    if (modifierSet.has(part)) {
      modifiers.push(part);
    } else {
      keyParts.push(part);
    }
  }
  modifiers.sort();
  return [...modifiers, ...keyParts].join("+");
}

// --- Conflict Detection ---

/**
 * Detect conflicts between a proposed binding and existing terminal/app bindings.
 *
 * Returns a conflict if the normalized key combination exactly matches
 * an existing terminal binding or application-level shortcut.
 */
export function detectConflicts(
  binding: string,
  existingBindings: TerminalKeybinding[],
  appBindings: string[],
): KeybindingConflict[] {
  const conflicts: KeybindingConflict[] = [];
  const normalizedNew = normalizeKeybinding(binding);

  // Check against existing terminal bindings
  for (const existing of existingBindings) {
    if (normalizeKeybinding(existing.keys) === normalizedNew) {
      conflicts.push({
        action: existing.action,
        conflictsWith: `Terminal: ${ACTION_LABELS[existing.action]} (${existing.keys})`,
        scope: "terminal",
      });
    }
  }

  // Check against application-level bindings
  for (const appBinding of appBindings) {
    if (normalizeKeybinding(appBinding) === normalizedNew) {
      conflicts.push({
        action: "copy", // placeholder — we don't know the app action
        conflictsWith: `Application: ${appBinding}`,
        scope: "application",
      });
    }
  }

  return conflicts;
}

// --- Keybinding Registry ---

export interface KeybindingRegistry {
  getAll(): TerminalKeybinding[];
  getBinding(action: TerminalAction): string;
  setBinding(action: TerminalAction, keys: string): void;
  resetToDefaults(): void;
  resetAction(action: TerminalAction): void;
  isDefault(action: TerminalAction): boolean;
  serialize(): KeybindingsStorage;
}

export function createKeybindingRegistry(): KeybindingRegistry {
  let bindings = loadBindings();

  return {
    getAll(): TerminalKeybinding[] {
      return ALL_TERMINAL_ACTIONS.map((action) => ({
        action,
        keys: bindings[action],
        isDefault: bindings[action] === DEFAULT_BINDINGS[action],
      }));
    },

    getBinding(action: TerminalAction): string {
      return bindings[action];
    },

    setBinding(action: TerminalAction, keys: string): void {
      bindings[action] = keys;
      saveBindings(bindings);
    },

    resetToDefaults(): void {
      bindings = { ...DEFAULT_BINDINGS };
      saveBindings(bindings);
    },

    resetAction(action: TerminalAction): void {
      bindings[action] = DEFAULT_BINDINGS[action];
      saveBindings(bindings);
    },

    isDefault(action: TerminalAction): boolean {
      return bindings[action] === DEFAULT_BINDINGS[action];
    },

    serialize(): KeybindingsStorage {
      return { version: 1, bindings: { ...bindings } };
    },
  };
}
