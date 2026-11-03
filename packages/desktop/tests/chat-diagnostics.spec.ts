import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  incrementChatDiagnostic,
  trackCardCollapsed,
  trackCardExpanded,
  trackGroupCollapsed,
  trackGroupExpanded,
  trackCompactToggleFlip,
  trackTimelineRailScroll,
  trackKeyboardShortcutFire,
  trackDupTextCollision,
  getChatDiagnostics,
  resetChatDiagnostics,
} from "../src/lib/chat-diagnostics";

// localStorage mock
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

describe("chat-diagnostics", () => {
  beforeEach(() => {
    resetChatDiagnostics();
    localStorageMock.clear();
  });

  it("starts with all counters at zero", () => {
    const diag = getChatDiagnostics();
    expect(diag.cardsCollapsed).toBe(0);
    expect(diag.cardsExpanded).toBe(0);
    expect(diag.groupsCollapsed).toBe(0);
    expect(diag.groupsExpanded).toBe(0);
    expect(diag.compactToggleFlips).toBe(0);
    expect(diag.timelineRailScrolls).toBe(0);
    expect(diag.keyboardShortcutFires).toBe(0);
    expect(diag.dupTextCollisions).toBe(0);
  });

  it("incrementChatDiagnostic increments the named counter", () => {
    incrementChatDiagnostic("cardsCollapsed");
    incrementChatDiagnostic("cardsCollapsed");
    incrementChatDiagnostic("groupsExpanded");
    const diag = getChatDiagnostics();
    expect(diag.cardsCollapsed).toBe(2);
    expect(diag.groupsExpanded).toBe(1);
  });

  it("convenience functions increment the correct counters", () => {
    trackCardCollapsed();
    trackCardExpanded();
    trackGroupCollapsed();
    trackGroupExpanded();
    trackCompactToggleFlip();
    trackTimelineRailScroll();
    trackKeyboardShortcutFire();
    trackDupTextCollision();

    const diag = getChatDiagnostics();
    expect(diag.cardsCollapsed).toBe(1);
    expect(diag.cardsExpanded).toBe(1);
    expect(diag.groupsCollapsed).toBe(1);
    expect(diag.groupsExpanded).toBe(1);
    expect(diag.compactToggleFlips).toBe(1);
    expect(diag.timelineRailScrolls).toBe(1);
    expect(diag.keyboardShortcutFires).toBe(1);
    expect(diag.dupTextCollisions).toBe(1);
  });

  it("resetChatDiagnostics resets all counters to zero", () => {
    trackCardCollapsed();
    trackCardCollapsed();
    trackDupTextCollision();
    resetChatDiagnostics();
    const diag = getChatDiagnostics();
    expect(diag.cardsCollapsed).toBe(0);
    expect(diag.dupTextCollisions).toBe(0);
  });

  it("getChatDiagnostics returns a snapshot (not a live reference)", () => {
    const before = getChatDiagnostics();
    trackCardCollapsed();
    const after = getChatDiagnostics();
    expect(before.cardsCollapsed).toBe(0);
    expect(after.cardsCollapsed).toBe(1);
  });

  it("logs to console.debug when CRAB_CHAT_DEBUG=1 in localStorage", () => {
    localStorage.setItem("CRAB_CHAT_DEBUG", "1");
    const spy = vi.spyOn(console, "debug").mockImplementation(() => {});
    trackCardCollapsed();
    expect(spy).toHaveBeenCalledWith(
      "[chat-diagnostics] cardsCollapsed: 1",
    );
    spy.mockRestore();
  });

  it("does not log to console.debug when CRAB_CHAT_DEBUG is not set", () => {
    const spy = vi.spyOn(console, "debug").mockImplementation(() => {});
    trackCardCollapsed();
    expect(spy).not.toHaveBeenCalled();
    spy.mockRestore();
  });

  it("does not log when CRAB_CHAT_DEBUG is set to something other than 1", () => {
    localStorage.setItem("CRAB_CHAT_DEBUG", "true");
    const spy = vi.spyOn(console, "debug").mockImplementation(() => {});
    trackCardCollapsed();
    expect(spy).not.toHaveBeenCalled();
    spy.mockRestore();
  });
});
