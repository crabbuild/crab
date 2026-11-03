import { describe, it, expect } from "vitest";
import {
  calculateOverlayHeight,
  matchesHotkey,
  DEFAULT_QUICK_TERMINAL_CONFIG,
} from "../src/components/QuickTerminal";

describe("QuickTerminal", () => {
  describe("calculateOverlayHeight", () => {
    it("calculates height as floor(windowHeight * percent / 100)", () => {
      expect(calculateOverlayHeight(40, 1000)).toBe(400);
      expect(calculateOverlayHeight(40, 800)).toBe(320);
      expect(calculateOverlayHeight(50, 900)).toBe(450);
    });

    it("handles minimum percentage (20%)", () => {
      expect(calculateOverlayHeight(20, 1000)).toBe(200);
    });

    it("handles maximum percentage (80%)", () => {
      expect(calculateOverlayHeight(80, 1000)).toBe(800);
    });

    it("clamps percentage below 20 to 20", () => {
      expect(calculateOverlayHeight(10, 1000)).toBe(200);
      expect(calculateOverlayHeight(0, 1000)).toBe(200);
    });

    it("clamps percentage above 80 to 80", () => {
      expect(calculateOverlayHeight(90, 1000)).toBe(800);
      expect(calculateOverlayHeight(100, 1000)).toBe(800);
    });

    it("floors the result for non-integer heights", () => {
      // 33% of 1000 = 330, but 33% of 999 = 329.67 → 329
      expect(calculateOverlayHeight(33, 999)).toBe(329);
    });

    it("handles odd window heights", () => {
      expect(calculateOverlayHeight(40, 1001)).toBe(400);
      expect(calculateOverlayHeight(40, 1003)).toBe(401);
    });

    it("default config uses 40%", () => {
      expect(DEFAULT_QUICK_TERMINAL_CONFIG.heightPercent).toBe(40);
    });
  });

  describe("matchesHotkey", () => {
    function createKeyEvent(overrides: Partial<KeyboardEvent>): KeyboardEvent {
      return {
        key: "",
        code: "",
        ctrlKey: false,
        shiftKey: false,
        altKey: false,
        metaKey: false,
        ...overrides,
      } as KeyboardEvent;
    }

    it("matches Ctrl+` hotkey", () => {
      const event = createKeyEvent({ key: "`", code: "Backquote", ctrlKey: true });
      expect(matchesHotkey(event, "Ctrl+`")).toBe(true);
    });

    it("does not match without Ctrl modifier", () => {
      const event = createKeyEvent({ key: "`", code: "Backquote", ctrlKey: false });
      expect(matchesHotkey(event, "Ctrl+`")).toBe(false);
    });

    it("matches CmdOrCtrl with either Ctrl or Meta", () => {
      const ctrlEvent = createKeyEvent({ key: "k", code: "KeyK", ctrlKey: true });
      const metaEvent = createKeyEvent({ key: "k", code: "KeyK", metaKey: true });
      expect(matchesHotkey(ctrlEvent, "CmdOrCtrl+k")).toBe(true);
      expect(matchesHotkey(metaEvent, "CmdOrCtrl+k")).toBe(true);
    });

    it("does not match wrong key", () => {
      const event = createKeyEvent({ key: "a", code: "KeyA", ctrlKey: true });
      expect(matchesHotkey(event, "Ctrl+`")).toBe(false);
    });

    it("default hotkey is Ctrl+`", () => {
      expect(DEFAULT_QUICK_TERMINAL_CONFIG.hotkey).toBe("Ctrl+`");
    });

    it("default animation duration is 150ms", () => {
      expect(DEFAULT_QUICK_TERMINAL_CONFIG.animationDurationMs).toBe(150);
    });
  });
});
