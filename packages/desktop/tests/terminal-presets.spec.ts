import { describe, it, expect, beforeEach } from "vitest";
import {
  createPresetManager,
  availableAgentPresets,
  BUILTIN_PRESET_LIST,
  LS_PRESETS_KEY,
  type StartupCommand,
} from "../src/lib/terminal-presets";

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

describe("terminal-presets", () => {
  beforeEach(() => {
    localStorageMock.clear();
  });

  it("shows installed and configured agents while preserving hidden presets", () => {
    const manager = createPresetManager();
    manager.setVisibility("builtin-claude", false);
    const agents = [
      { type: "claude-code", name: "Claude Code", binary: "claude", available: true },
      { type: "codex", name: "Codex", binary: "codex", available: true },
      { type: "aider", name: "Aider", binary: "aider", available: false },
      { type: "custom", name: "Configured CLI", binary: "custom", available: false, configured: true },
    ].map(agent => ({ path: "", version: "", ...agent }));
    expect(availableAgentPresets(manager.getAll(), agents).map(preset => preset.agentType)).toEqual(["codex", "custom"]);
  });

  describe("built-in presets", () => {
    it("includes 7 built-in agent presets", () => {
      expect(BUILTIN_PRESET_LIST).toHaveLength(7);
    });

    it("has Claude Code preset with 'claude' command", () => {
      const claude = BUILTIN_PRESET_LIST.find((p) => p.id === "builtin-claude");
      expect(claude).toBeDefined();
      expect(claude!.name).toBe("Claude Code");
      expect(claude!.command).toBe("claude");
      expect(claude!.category).toBe("builtin");
    });

    it("has OpenAI Codex preset with 'codex' command", () => {
      const codex = BUILTIN_PRESET_LIST.find((p) => p.id === "builtin-codex");
      expect(codex).toBeDefined();
      expect(codex!.command).toBe("codex");
    });

    it("has Aider preset with 'aider' command", () => {
      const aider = BUILTIN_PRESET_LIST.find((p) => p.id === "builtin-aider");
      expect(aider).toBeDefined();
      expect(aider!.command).toBe("aider");
    });

    it("has Gemini CLI preset with 'gemini' command", () => {
      const gemini = BUILTIN_PRESET_LIST.find((p) => p.id === "builtin-gemini");
      expect(gemini).toBeDefined();
      expect(gemini!.command).toBe("gemini");
    });

    it("has Amazon Q Developer CLI preset with 'q chat' command", () => {
      const q = BUILTIN_PRESET_LIST.find((p) => p.id === "builtin-q");
      expect(q).toBeDefined();
      expect(q!.command).toBe("q chat");
    });

    it("has Kiro CLI preset with 'kiro-cli' command", () => {
      const kiro = BUILTIN_PRESET_LIST.find((p) => p.id === "builtin-kiro");
      expect(kiro).toBeDefined();
      expect(kiro!.name).toBe("Kiro CLI");
      expect(kiro!.command).toBe("kiro-cli");
    });

    it("has OpenCode preset with 'opencode' command", () => {
      const opencode = BUILTIN_PRESET_LIST.find((p) => p.id === "builtin-opencode");
      expect(opencode).toBeDefined();
      expect(opencode!.name).toBe("OpenCode");
      expect(opencode!.command).toBe("opencode");
    });

    it("each built-in preset has sub-options", () => {
      for (const preset of BUILTIN_PRESET_LIST) {
        expect(preset.subOptions).toBeDefined();
        expect(preset.subOptions!.length).toBeGreaterThan(0);
      }
    });

    it("sub-options have required fields", () => {
      for (const preset of BUILTIN_PRESET_LIST) {
        for (const sub of preset.subOptions!) {
          expect(sub.id).toBeTruthy();
          expect(sub.label).toBeTruthy();
          expect(sub.command).toBeTruthy();
        }
      }
    });
  });

  describe("PresetManager.getAll", () => {
    it("returns all built-in presets when no custom commands exist", () => {
      const pm = createPresetManager();
      const all = pm.getAll();
      expect(all.length).toBe(7);
      expect(all.every((p) => p.category === "builtin")).toBe(true);
    });

    it("returns built-in and custom presets together", () => {
      const pm = createPresetManager();
      pm.addCustom({ name: "My Tool", command: "mytool", visible: true });
      const all = pm.getAll();
      expect(all.length).toBe(8);
    });
  });

  describe("PresetManager.getGrouped", () => {
    it("groups presets by category with builtin first", () => {
      const pm = createPresetManager();
      pm.addCustom({ name: "Custom1", command: "cmd1", visible: true });
      const grouped = pm.getGrouped();
      expect(grouped.builtin.length).toBe(7);
      expect(grouped.custom.length).toBe(1);
      expect(grouped.builtin.every((p) => p.category === "builtin")).toBe(true);
      expect(grouped.custom.every((p) => p.category === "custom")).toBe(true);
    });

    it("excludes hidden presets from grouped results", () => {
      const pm = createPresetManager();
      pm.setVisibility("builtin-claude", false);
      const grouped = pm.getGrouped();
      expect(grouped.builtin.length).toBe(6);
      expect(grouped.builtin.find((p) => p.id === "builtin-claude")).toBeUndefined();
    });
  });

  describe("PresetManager.addCustom", () => {
    it("adds a custom command with generated id and category", () => {
      const pm = createPresetManager();
      const cmd = pm.addCustom({ name: "Test", command: "test-cmd", visible: true });
      expect(cmd.id).toMatch(/^custom-/);
      expect(cmd.category).toBe("custom");
      expect(cmd.name).toBe("Test");
      expect(cmd.command).toBe("test-cmd");
    });

    it("persists to localStorage", () => {
      const pm = createPresetManager();
      pm.addCustom({ name: "Persisted", command: "persist-cmd", visible: true });
      const raw = localStorage.getItem(LS_PRESETS_KEY);
      expect(raw).not.toBeNull();
      const parsed = JSON.parse(raw!);
      expect(parsed.customCommands).toHaveLength(1);
      expect(parsed.customCommands[0].name).toBe("Persisted");
    });

    it("assigns incrementing order values", () => {
      const pm = createPresetManager();
      const cmd1 = pm.addCustom({ name: "First", command: "first", visible: true });
      const cmd2 = pm.addCustom({ name: "Second", command: "second", visible: true });
      expect(cmd2.order).toBeGreaterThan(cmd1.order);
    });
  });

  describe("PresetManager.updateCustom", () => {
    it("updates an existing custom command", () => {
      const pm = createPresetManager();
      const cmd = pm.addCustom({ name: "Original", command: "orig", visible: true });
      pm.updateCustom(cmd.id, { name: "Updated", command: "new-cmd" });
      const all = pm.getAll();
      const updated = all.find((c) => c.id === cmd.id);
      expect(updated!.name).toBe("Updated");
      expect(updated!.command).toBe("new-cmd");
    });

    it("does nothing for non-existent id", () => {
      const pm = createPresetManager();
      pm.updateCustom("nonexistent", { name: "Nope" });
      expect(pm.getAll().length).toBe(7); // only builtins
    });

    it("cannot override id or category", () => {
      const pm = createPresetManager();
      const cmd = pm.addCustom({ name: "Test", command: "test", visible: true });
      pm.updateCustom(cmd.id, { id: "hacked", category: "builtin" } as any);
      const found = pm.getAll().find((c) => c.id === cmd.id);
      expect(found).toBeDefined();
      expect(found!.category).toBe("custom");
    });
  });

  describe("PresetManager.removeCustom", () => {
    it("removes a custom command", () => {
      const pm = createPresetManager();
      const cmd = pm.addCustom({ name: "ToRemove", command: "rm", visible: true });
      pm.removeCustom(cmd.id);
      const all = pm.getAll();
      expect(all.find((c) => c.id === cmd.id)).toBeUndefined();
    });

    it("persists removal to localStorage", () => {
      const pm = createPresetManager();
      const cmd = pm.addCustom({ name: "ToRemove", command: "rm", visible: true });
      pm.removeCustom(cmd.id);
      const raw = localStorage.getItem(LS_PRESETS_KEY);
      const parsed = JSON.parse(raw!);
      expect(parsed.customCommands).toHaveLength(0);
    });
  });

  describe("PresetManager.reorder", () => {
    it("reorders custom commands by id list", () => {
      const pm = createPresetManager();
      const cmd1 = pm.addCustom({ name: "A", command: "a", visible: true });
      const cmd2 = pm.addCustom({ name: "B", command: "b", visible: true });
      const cmd3 = pm.addCustom({ name: "C", command: "c", visible: true });
      pm.reorder([cmd3.id, cmd1.id, cmd2.id]);
      const grouped = pm.getGrouped();
      expect(grouped.custom[0].id).toBe(cmd3.id);
      expect(grouped.custom[1].id).toBe(cmd1.id);
      expect(grouped.custom[2].id).toBe(cmd2.id);
    });

    it("preserves all elements after reorder (no loss)", () => {
      const pm = createPresetManager();
      const cmd1 = pm.addCustom({ name: "A", command: "a", visible: true });
      const cmd2 = pm.addCustom({ name: "B", command: "b", visible: true });
      pm.reorder([cmd2.id, cmd1.id]);
      const all = pm.getAll().filter((c) => c.category === "custom");
      expect(all.length).toBe(2);
      const ids = all.map((c) => c.id).sort();
      expect(ids).toContain(cmd1.id);
      expect(ids).toContain(cmd2.id);
    });
  });

  describe("PresetManager.setVisibility", () => {
    it("hides a built-in preset", () => {
      const pm = createPresetManager();
      pm.setVisibility("builtin-aider", false);
      const grouped = pm.getGrouped();
      expect(grouped.builtin.find((p) => p.id === "builtin-aider")).toBeUndefined();
    });

    it("hides a custom preset", () => {
      const pm = createPresetManager();
      const cmd = pm.addCustom({ name: "Hidden", command: "hide", visible: true });
      pm.setVisibility(cmd.id, false);
      const grouped = pm.getGrouped();
      expect(grouped.custom.find((c) => c.id === cmd.id)).toBeUndefined();
    });
  });

  describe("PresetManager.validate", () => {
    it("accepts valid name and command", () => {
      const pm = createPresetManager();
      const result = pm.validate({ name: "My Tool", command: "mytool --flag" });
      expect(result.valid).toBe(true);
      expect(result.errors).toHaveLength(0);
    });

    it("rejects empty name", () => {
      const pm = createPresetManager();
      const result = pm.validate({ name: "", command: "cmd" });
      expect(result.valid).toBe(false);
      expect(result.errors).toContain("Name is required");
    });

    it("rejects whitespace-only name", () => {
      const pm = createPresetManager();
      const result = pm.validate({ name: "   ", command: "cmd" });
      expect(result.valid).toBe(false);
      expect(result.errors).toContain("Name is required");
    });

    it("rejects empty command", () => {
      const pm = createPresetManager();
      const result = pm.validate({ name: "Tool", command: "" });
      expect(result.valid).toBe(false);
      expect(result.errors).toContain("Command is required");
    });

    it("rejects whitespace-only command", () => {
      const pm = createPresetManager();
      const result = pm.validate({ name: "Tool", command: "  \t  " });
      expect(result.valid).toBe(false);
      expect(result.errors).toContain("Command is required");
    });

    it("rejects both empty name and command", () => {
      const pm = createPresetManager();
      const result = pm.validate({ name: "", command: "" });
      expect(result.valid).toBe(false);
      expect(result.errors.length).toBe(2);
    });
  });

  describe("persistence round-trip", () => {
    it("restores custom commands from localStorage", () => {
      const pm1 = createPresetManager();
      pm1.addCustom({ name: "Saved", command: "saved-cmd", visible: true });
      pm1.setVisibility("builtin-codex", false);

      // Create a new manager (simulates app restart)
      const pm2 = createPresetManager();
      const grouped = pm2.getGrouped();
      expect(grouped.custom.length).toBe(1);
      expect(grouped.custom[0].name).toBe("Saved");
      expect(grouped.builtin.find((p) => p.id === "builtin-codex")).toBeUndefined();
    });
  });
});
