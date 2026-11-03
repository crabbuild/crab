import { describe, it, expect, beforeEach } from "vitest";
import {
  createKeybindingRegistry,
  detectConflicts,
  normalizeKeybinding,
  DEFAULT_BINDINGS,
  ALL_TERMINAL_ACTIONS,
  ACTION_LABELS,
  LS_KEYBINDINGS_KEY,
  type TerminalAction,
  type TerminalKeybinding,
} from "../src/lib/terminal-keybindings";

// Mock localStorage
const store: Record<string, string> = {};
const localStorageMock = {
  getItem: (key: string) => store[key] ?? null,
  setItem: (key: string, value: string) => { store[key] = value; },
  removeItem: (key: string) => { delete store[key]; },
  clear: () => { for (const k of Object.keys(store)) delete store[k]; },
  get length() { return Object.keys(store).length; },
  key: (i: number) => Object.keys(store)[i] ?? null,
};
Object.defineProperty(globalThis, "localStorage", { value: localStorageMock });

describe("terminal-keybindings", () => {
  beforeEach(() => {
    localStorageMock.clear();
  });

  describe("normalizeKeybinding", () => {
    it("lowercases and sorts modifiers", () => {
      expect(normalizeKeybinding("Ctrl+Shift+C")).toBe("ctrl+shift+c");
      expect(normalizeKeybinding("Shift+Ctrl+C")).toBe("ctrl+shift+c");
    });

    it("handles CmdOrCtrl modifier", () => {
      expect(normalizeKeybinding("CmdOrCtrl+K")).toBe("cmdorctrl+k");
    });

    it("handles single key without modifiers", () => {
      expect(normalizeKeybinding("F5")).toBe("f5");
    });
  });

  describe("detectConflicts", () => {
    it("returns empty array when no conflicts exist", () => {
      const existing: TerminalKeybinding[] = [
        { action: "copy", keys: "CmdOrCtrl+Shift+C", isDefault: true },
        { action: "paste", keys: "CmdOrCtrl+Shift+V", isDefault: true },
      ];
      const conflicts = detectConflicts("CmdOrCtrl+K", existing, []);
      expect(conflicts).toHaveLength(0);
    });

    it("detects conflict with existing terminal binding", () => {
      const existing: TerminalKeybinding[] = [
        { action: "copy", keys: "CmdOrCtrl+Shift+C", isDefault: true },
      ];
      const conflicts = detectConflicts("CmdOrCtrl+Shift+C", existing, []);
      expect(conflicts).toHaveLength(1);
      expect(conflicts[0].scope).toBe("terminal");
      expect(conflicts[0].action).toBe("copy");
    });

    it("detects conflict with application binding", () => {
      const existing: TerminalKeybinding[] = [];
      const appBindings = ["CmdOrCtrl+Q", "CmdOrCtrl+W"];
      const conflicts = detectConflicts("CmdOrCtrl+Q", existing, appBindings);
      expect(conflicts).toHaveLength(1);
      expect(conflicts[0].scope).toBe("application");
    });

    it("detects conflicts case-insensitively", () => {
      const existing: TerminalKeybinding[] = [
        { action: "search", keys: "CmdOrCtrl+F", isDefault: true },
      ];
      const conflicts = detectConflicts("cmdorctrl+f", existing, []);
      expect(conflicts).toHaveLength(1);
    });

    it("detects conflicts regardless of modifier order", () => {
      const existing: TerminalKeybinding[] = [
        { action: "copy", keys: "Ctrl+Shift+C", isDefault: true },
      ];
      const conflicts = detectConflicts("Shift+Ctrl+C", existing, []);
      expect(conflicts).toHaveLength(1);
    });

    it("returns multiple conflicts when binding matches both terminal and app", () => {
      const existing: TerminalKeybinding[] = [
        { action: "close-tab", keys: "CmdOrCtrl+W", isDefault: true },
      ];
      const appBindings = ["CmdOrCtrl+W"];
      const conflicts = detectConflicts("CmdOrCtrl+W", existing, appBindings);
      expect(conflicts).toHaveLength(2);
    });
  });

  describe("KeybindingRegistry", () => {
    it("returns all actions with default bindings initially", () => {
      const registry = createKeybindingRegistry();
      const all = registry.getAll();
      expect(all).toHaveLength(ALL_TERMINAL_ACTIONS.length);
      for (const binding of all) {
        expect(binding.isDefault).toBe(true);
        expect(binding.keys).toBe(DEFAULT_BINDINGS[binding.action]);
      }
    });

    it("getBinding returns the current binding for an action", () => {
      const registry = createKeybindingRegistry();
      expect(registry.getBinding("copy")).toBe("CmdOrCtrl+Shift+C");
    });

    it("setBinding updates a binding and persists", () => {
      const registry = createKeybindingRegistry();
      registry.setBinding("copy", "Ctrl+C");
      expect(registry.getBinding("copy")).toBe("Ctrl+C");
      expect(registry.isDefault("copy")).toBe(false);

      // Verify persistence
      const raw = localStorage.getItem(LS_KEYBINDINGS_KEY);
      expect(raw).not.toBeNull();
      const parsed = JSON.parse(raw!);
      expect(parsed.bindings.copy).toBe("Ctrl+C");
    });

    it("resetToDefaults restores all bindings", () => {
      const registry = createKeybindingRegistry();
      registry.setBinding("copy", "Ctrl+C");
      registry.setBinding("paste", "Ctrl+V");
      registry.resetToDefaults();
      expect(registry.getBinding("copy")).toBe(DEFAULT_BINDINGS.copy);
      expect(registry.getBinding("paste")).toBe(DEFAULT_BINDINGS.paste);
      expect(registry.isDefault("copy")).toBe(true);
    });

    it("resetAction restores a single action to default", () => {
      const registry = createKeybindingRegistry();
      registry.setBinding("copy", "Ctrl+C");
      registry.setBinding("paste", "Ctrl+V");
      registry.resetAction("copy");
      expect(registry.getBinding("copy")).toBe(DEFAULT_BINDINGS.copy);
      expect(registry.getBinding("paste")).toBe("Ctrl+V"); // unchanged
    });

    it("isDefault returns false for modified bindings", () => {
      const registry = createKeybindingRegistry();
      expect(registry.isDefault("copy")).toBe(true);
      registry.setBinding("copy", "Alt+C");
      expect(registry.isDefault("copy")).toBe(false);
    });

    it("serialize produces a valid storage object", () => {
      const registry = createKeybindingRegistry();
      registry.setBinding("search", "Ctrl+G");
      const serialized = registry.serialize();
      expect(serialized.version).toBe(1);
      expect(serialized.bindings.search).toBe("Ctrl+G");
      expect(serialized.bindings.copy).toBe(DEFAULT_BINDINGS.copy);
    });

    it("persists and restores across instances", () => {
      const reg1 = createKeybindingRegistry();
      reg1.setBinding("clear", "Ctrl+L");

      const reg2 = createKeybindingRegistry();
      expect(reg2.getBinding("clear")).toBe("Ctrl+L");
      expect(reg2.isDefault("clear")).toBe(false);
    });
  });

  describe("ALL_TERMINAL_ACTIONS", () => {
    it("covers all expected actions", () => {
      expect(ALL_TERMINAL_ACTIONS).toContain("copy");
      expect(ALL_TERMINAL_ACTIONS).toContain("paste");
      expect(ALL_TERMINAL_ACTIONS).toContain("clear");
      expect(ALL_TERMINAL_ACTIONS).toContain("split-horizontal");
      expect(ALL_TERMINAL_ACTIONS).toContain("split-vertical");
      expect(ALL_TERMINAL_ACTIONS).toContain("new-tab");
      expect(ALL_TERMINAL_ACTIONS).toContain("close-tab");
      expect(ALL_TERMINAL_ACTIONS).toContain("next-tab");
      expect(ALL_TERMINAL_ACTIONS).toContain("prev-tab");
      expect(ALL_TERMINAL_ACTIONS).toContain("jump-prev-prompt");
      expect(ALL_TERMINAL_ACTIONS).toContain("jump-next-prompt");
      expect(ALL_TERMINAL_ACTIONS).toContain("search");
      expect(ALL_TERMINAL_ACTIONS).toContain("toggle-quick-terminal");
      expect(ALL_TERMINAL_ACTIONS).toContain("select-command-output");
    });

    it("has labels for all actions", () => {
      for (const action of ALL_TERMINAL_ACTIONS) {
        expect(ACTION_LABELS[action]).toBeDefined();
        expect(ACTION_LABELS[action].length).toBeGreaterThan(0);
      }
    });

    it("has default bindings for all actions", () => {
      for (const action of ALL_TERMINAL_ACTIONS) {
        expect(DEFAULT_BINDINGS[action]).toBeDefined();
        expect(DEFAULT_BINDINGS[action].length).toBeGreaterThan(0);
      }
    });
  });
});
