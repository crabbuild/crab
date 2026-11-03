/**
 * Tests for chat keyboard shortcut handler logic.
 *
 * Validates: Requirements CHAT10.1–10.6
 *
 * Tests the shortcut handler functions directly rather than mounting
 * React components. Store mutations are mocked via the actions interface.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  chatShortcuts,
  matchShortcut,
  handleChatShortcut,
  handleChatKeyDown,
  isTextInputFocused,
  modifierMatches,
  type ChatFocusContext,
  type ChatShortcutActions,
} from "../src/lib/keyboard-shortcuts-chat";

// ─── Test helpers ────────────────────────────────────────────────────────────

function makeFocusContext(overrides: Partial<ChatFocusContext> = {}): ChatFocusContext {
  return {
    cardId: "card-1",
    groupId: "g:card-1",
    turnIndex: 0,
    taskId: "task-abc",
    ...overrides,
  };
}

function makeActions(): ChatShortcutActions {
  return {
    expandCard: vi.fn(),
    collapseCard: vi.fn(),
    expandGroup: vi.fn(),
    collapseGroup: vi.fn(),
    expandTurn: vi.fn(),
    collapseTurn: vi.fn(),
    moveFocusDown: vi.fn(),
    moveFocusUp: vi.fn(),
  };
}

function makeKeyEvent(overrides: Partial<{
  key: string;
  metaKey: boolean;
  ctrlKey: boolean;
  shiftKey: boolean;
}> = {}) {
  return {
    key: "",
    metaKey: false,
    ctrlKey: false,
    shiftKey: false,
    ...overrides,
  };
}

// ─── Registry tests ──────────────────────────────────────────────────────────

describe("chatShortcuts registry", () => {
  it("contains exactly six shortcuts", () => {
    expect(chatShortcuts).toHaveLength(6);
  });

  it("has unique ids", () => {
    const ids = chatShortcuts.map((s) => s.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("defines expand as Mod+]", () => {
    const expand = chatShortcuts.find((s) => s.id === "expand");
    expect(expand).toBeDefined();
    expect(expand!.modifier).toBe("mod");
    expect(expand!.key).toBe("]");
  });

  it("defines collapse as Mod+[", () => {
    const collapse = chatShortcuts.find((s) => s.id === "collapse");
    expect(collapse).toBeDefined();
    expect(collapse!.modifier).toBe("mod");
    expect(collapse!.key).toBe("[");
  });

  it("defines expand-turn as Mod+E", () => {
    const expandTurn = chatShortcuts.find((s) => s.id === "expand-turn");
    expect(expandTurn).toBeDefined();
    expect(expandTurn!.modifier).toBe("mod");
    expect(expandTurn!.key).toBe("e");
  });

  it("defines collapse-turn as Mod+Shift+E", () => {
    const collapseTurn = chatShortcuts.find((s) => s.id === "collapse-turn");
    expect(collapseTurn).toBeDefined();
    expect(collapseTurn!.modifier).toBe("mod+shift");
    expect(collapseTurn!.key).toBe("E");
  });

  it("defines vim-down as j (no modifier)", () => {
    const vimDown = chatShortcuts.find((s) => s.id === "vim-down");
    expect(vimDown).toBeDefined();
    expect(vimDown!.modifier).toBe("none");
    expect(vimDown!.key).toBe("j");
  });

  it("defines vim-up as k (no modifier)", () => {
    const vimUp = chatShortcuts.find((s) => s.id === "vim-up");
    expect(vimUp).toBeDefined();
    expect(vimUp!.modifier).toBe("none");
    expect(vimUp!.key).toBe("k");
  });
});

// ─── modifierMatches tests ───────────────────────────────────────────────────

describe("modifierMatches", () => {
  describe("non-Mac (ctrlKey is the mod key)", () => {
    const isMac = false;

    it("matches 'mod' when ctrlKey is pressed", () => {
      expect(
        modifierMatches("mod", { metaKey: false, ctrlKey: true, shiftKey: false }, isMac),
      ).toBe(true);
    });

    it("does not match 'mod' when shiftKey is also pressed", () => {
      expect(
        modifierMatches("mod", { metaKey: false, ctrlKey: true, shiftKey: true }, isMac),
      ).toBe(false);
    });

    it("matches 'mod+shift' when ctrlKey and shiftKey are pressed", () => {
      expect(
        modifierMatches("mod+shift", { metaKey: false, ctrlKey: true, shiftKey: true }, isMac),
      ).toBe(true);
    });

    it("does not match 'mod+shift' without shiftKey", () => {
      expect(
        modifierMatches("mod+shift", { metaKey: false, ctrlKey: true, shiftKey: false }, isMac),
      ).toBe(false);
    });
  });

  describe("Mac (metaKey is the mod key)", () => {
    const isMac = true;

    it("matches 'mod' when metaKey is pressed", () => {
      expect(
        modifierMatches("mod", { metaKey: true, ctrlKey: false, shiftKey: false }, isMac),
      ).toBe(true);
    });

    it("does not match 'mod' when shiftKey is also pressed", () => {
      expect(
        modifierMatches("mod", { metaKey: true, ctrlKey: false, shiftKey: true }, isMac),
      ).toBe(false);
    });

    it("matches 'mod+shift' when metaKey and shiftKey are pressed", () => {
      expect(
        modifierMatches("mod+shift", { metaKey: true, ctrlKey: false, shiftKey: true }, isMac),
      ).toBe(true);
    });

    it("does not match 'mod+shift' without shiftKey", () => {
      expect(
        modifierMatches("mod+shift", { metaKey: true, ctrlKey: false, shiftKey: false }, isMac),
      ).toBe(false);
    });
  });

  it("matches 'none' when no modifiers are pressed", () => {
    expect(
      modifierMatches("none", { metaKey: false, ctrlKey: false, shiftKey: false }),
    ).toBe(true);
  });

  it("does not match 'none' when ctrlKey is pressed", () => {
    expect(
      modifierMatches("none", { metaKey: false, ctrlKey: true, shiftKey: false }),
    ).toBe(false);
  });
});

// ─── matchShortcut tests ─────────────────────────────────────────────────────

describe("matchShortcut", () => {
  // Explicitly pass isMac=false so tests are platform-independent
  const isMac = false;

  it("matches Ctrl+] to expand (non-Mac)", () => {
    const result = matchShortcut(
      makeKeyEvent({ key: "]", ctrlKey: true }),
      false,
      isMac,
    );
    expect(result).not.toBeNull();
    expect(result!.id).toBe("expand");
  });

  it("matches Ctrl+[ to collapse (non-Mac)", () => {
    const result = matchShortcut(
      makeKeyEvent({ key: "[", ctrlKey: true }),
      false,
      isMac,
    );
    expect(result).not.toBeNull();
    expect(result!.id).toBe("collapse");
  });

  it("matches Ctrl+e to expand-turn (non-Mac)", () => {
    const result = matchShortcut(
      makeKeyEvent({ key: "e", ctrlKey: true }),
      false,
      isMac,
    );
    expect(result).not.toBeNull();
    expect(result!.id).toBe("expand-turn");
  });

  it("matches Ctrl+Shift+E to collapse-turn (non-Mac)", () => {
    const result = matchShortcut(
      makeKeyEvent({ key: "E", ctrlKey: true, shiftKey: true }),
      false,
      isMac,
    );
    expect(result).not.toBeNull();
    expect(result!.id).toBe("collapse-turn");
  });

  it("matches Cmd+] to expand (Mac)", () => {
    const result = matchShortcut(
      makeKeyEvent({ key: "]", metaKey: true }),
      false,
      true,
    );
    expect(result).not.toBeNull();
    expect(result!.id).toBe("expand");
  });

  it("matches Cmd+[ to collapse (Mac)", () => {
    const result = matchShortcut(
      makeKeyEvent({ key: "[", metaKey: true }),
      false,
      true,
    );
    expect(result).not.toBeNull();
    expect(result!.id).toBe("collapse");
  });

  it("returns null for unrecognised key", () => {
    const result = matchShortcut(
      makeKeyEvent({ key: "x", ctrlKey: true }),
      false,
      isMac,
    );
    expect(result).toBeNull();
  });

  it("returns null for key without required modifier", () => {
    const result = matchShortcut(
      makeKeyEvent({ key: "]" }), // no ctrlKey or metaKey
      false,
      isMac,
    );
    expect(result).toBeNull();
  });
});

// ─── Vim navigation gating ───────────────────────────────────────────────────

describe("matchShortcut — Vim navigation gating", () => {
  it("does NOT match j when vimNavigation is off", () => {
    const result = matchShortcut(makeKeyEvent({ key: "j" }), false);
    expect(result).toBeNull();
  });

  it("does NOT match k when vimNavigation is off", () => {
    const result = matchShortcut(makeKeyEvent({ key: "k" }), false);
    expect(result).toBeNull();
  });

  it("matches j when vimNavigation is on", () => {
    const result = matchShortcut(makeKeyEvent({ key: "j" }), true);
    expect(result).not.toBeNull();
    expect(result!.id).toBe("vim-down");
  });

  it("matches k when vimNavigation is on", () => {
    const result = matchShortcut(makeKeyEvent({ key: "k" }), true);
    expect(result).not.toBeNull();
    expect(result!.id).toBe("vim-up");
  });

  it("does not match j with a modifier even when vim is on", () => {
    const result = matchShortcut(
      makeKeyEvent({ key: "j", ctrlKey: true }),
      true,
    );
    // j with ctrl doesn't match "none" modifier requirement
    expect(result).toBeNull();
  });
});

// ─── handleChatShortcut tests ────────────────────────────────────────────────

describe("handleChatShortcut", () => {
  let actions: ChatShortcutActions;
  let focused: ChatFocusContext;

  beforeEach(() => {
    actions = makeActions();
    focused = makeFocusContext();
  });

  it("expand shortcut calls expandCard when cardId is focused", () => {
    const shortcut = chatShortcuts.find((s) => s.id === "expand")!;
    const handled = handleChatShortcut(shortcut, focused, actions);

    expect(handled).toBe(true);
    expect(actions.expandCard).toHaveBeenCalledWith("task-abc", "card-1");
  });

  it("expand shortcut calls expandGroup when only groupId is focused", () => {
    focused = makeFocusContext({ cardId: null, groupId: "g:card-1" });
    const shortcut = chatShortcuts.find((s) => s.id === "expand")!;
    const handled = handleChatShortcut(shortcut, focused, actions);

    expect(handled).toBe(true);
    expect(actions.expandGroup).toHaveBeenCalledWith("task-abc", "g:card-1");
    expect(actions.expandCard).not.toHaveBeenCalled();
  });

  it("expand shortcut returns false when nothing is focused", () => {
    focused = makeFocusContext({ cardId: null, groupId: null });
    const shortcut = chatShortcuts.find((s) => s.id === "expand")!;
    const handled = handleChatShortcut(shortcut, focused, actions);

    expect(handled).toBe(false);
    expect(actions.expandCard).not.toHaveBeenCalled();
    expect(actions.expandGroup).not.toHaveBeenCalled();
  });

  it("collapse shortcut calls collapseCard when cardId is focused", () => {
    const shortcut = chatShortcuts.find((s) => s.id === "collapse")!;
    const handled = handleChatShortcut(shortcut, focused, actions);

    expect(handled).toBe(true);
    expect(actions.collapseCard).toHaveBeenCalledWith("task-abc", "card-1");
  });

  it("collapse shortcut calls collapseGroup when only groupId is focused", () => {
    focused = makeFocusContext({ cardId: null, groupId: "g:card-1" });
    const shortcut = chatShortcuts.find((s) => s.id === "collapse")!;
    const handled = handleChatShortcut(shortcut, focused, actions);

    expect(handled).toBe(true);
    expect(actions.collapseGroup).toHaveBeenCalledWith("task-abc", "g:card-1");
  });

  it("expand-turn shortcut calls expandTurn with the focused turnIndex", () => {
    focused = makeFocusContext({ turnIndex: 3 });
    const shortcut = chatShortcuts.find((s) => s.id === "expand-turn")!;
    const handled = handleChatShortcut(shortcut, focused, actions);

    expect(handled).toBe(true);
    expect(actions.expandTurn).toHaveBeenCalledWith("task-abc", 3);
  });

  it("collapse-turn shortcut calls collapseTurn with the focused turnIndex", () => {
    focused = makeFocusContext({ turnIndex: 2 });
    const shortcut = chatShortcuts.find((s) => s.id === "collapse-turn")!;
    const handled = handleChatShortcut(shortcut, focused, actions);

    expect(handled).toBe(true);
    expect(actions.collapseTurn).toHaveBeenCalledWith("task-abc", 2);
  });

  it("vim-down shortcut calls moveFocusDown", () => {
    const shortcut = chatShortcuts.find((s) => s.id === "vim-down")!;
    const handled = handleChatShortcut(shortcut, focused, actions);

    expect(handled).toBe(true);
    expect(actions.moveFocusDown).toHaveBeenCalled();
  });

  it("vim-up shortcut calls moveFocusUp", () => {
    const shortcut = chatShortcuts.find((s) => s.id === "vim-up")!;
    const handled = handleChatShortcut(shortcut, focused, actions);

    expect(handled).toBe(true);
    expect(actions.moveFocusUp).toHaveBeenCalled();
  });
});

// ─── isTextInputFocused tests ────────────────────────────────────────────────

describe("isTextInputFocused", () => {
  it("returns true for a textarea element", () => {
    const el = { tagName: "TEXTAREA", getAttribute: () => null } as unknown as Element;
    expect(isTextInputFocused(el)).toBe(true);
  });

  it("returns true for a text input element", () => {
    const el = {
      tagName: "INPUT",
      type: "text",
      getAttribute: () => null,
    } as unknown as Element;
    expect(isTextInputFocused(el)).toBe(true);
  });

  it("returns true for an input with no type (defaults to text)", () => {
    const el = {
      tagName: "INPUT",
      type: "",
      getAttribute: () => null,
    } as unknown as Element;
    expect(isTextInputFocused(el)).toBe(true);
  });

  it("returns true for a contenteditable element", () => {
    const el = {
      tagName: "DIV",
      getAttribute: (attr: string) => (attr === "contenteditable" ? "true" : null),
    } as unknown as Element;
    expect(isTextInputFocused(el)).toBe(true);
  });

  it("returns false for a regular div", () => {
    const el = {
      tagName: "DIV",
      getAttribute: () => null,
    } as unknown as Element;
    expect(isTextInputFocused(el)).toBe(false);
  });

  it("returns false for a button", () => {
    const el = {
      tagName: "BUTTON",
      getAttribute: () => null,
    } as unknown as Element;
    expect(isTextInputFocused(el)).toBe(false);
  });

  it("returns false for a checkbox input", () => {
    const el = {
      tagName: "INPUT",
      type: "checkbox",
      getAttribute: () => null,
    } as unknown as Element;
    expect(isTextInputFocused(el)).toBe(false);
  });

  it("returns false for null", () => {
    expect(isTextInputFocused(null)).toBe(false);
  });
});

// ─── handleChatKeyDown integration tests ─────────────────────────────────────

describe("handleChatKeyDown — no-collision invariant", () => {
  let actions: ChatShortcutActions;
  let focused: ChatFocusContext;

  beforeEach(() => {
    actions = makeActions();
    focused = makeFocusContext();
  });

  it("does NOT fire when focused element is a textarea", () => {
    const textarea = {
      tagName: "TEXTAREA",
      getAttribute: () => null,
    } as unknown as Element;

    const handled = handleChatKeyDown(
      makeKeyEvent({ key: "]", ctrlKey: true }),
      textarea,
      focused,
      actions,
      false,
    );

    expect(handled).toBe(false);
    expect(actions.expandCard).not.toHaveBeenCalled();
  });

  it("does NOT fire when focused element is a text input", () => {
    const input = {
      tagName: "INPUT",
      type: "text",
      getAttribute: () => null,
    } as unknown as Element;

    const handled = handleChatKeyDown(
      makeKeyEvent({ key: "[", ctrlKey: true }),
      input,
      focused,
      actions,
      false,
    );

    expect(handled).toBe(false);
    expect(actions.collapseCard).not.toHaveBeenCalled();
  });

  it("does NOT fire when focused element is contenteditable", () => {
    const editable = {
      tagName: "DIV",
      getAttribute: (attr: string) => (attr === "contenteditable" ? "true" : null),
    } as unknown as Element;

    const handled = handleChatKeyDown(
      makeKeyEvent({ key: "e", ctrlKey: true }),
      editable,
      focused,
      actions,
      false,
    );

    expect(handled).toBe(false);
    expect(actions.expandTurn).not.toHaveBeenCalled();
  });

  it("fires when focused element is a regular div (chat column)", () => {
    const div = {
      tagName: "DIV",
      getAttribute: () => null,
    } as unknown as Element;

    const handled = handleChatKeyDown(
      makeKeyEvent({ key: "]", ctrlKey: true }),
      div,
      focused,
      actions,
      false,
      false, // isMac = false so ctrlKey is the mod key
    );

    expect(handled).toBe(true);
    expect(actions.expandCard).toHaveBeenCalledWith("task-abc", "card-1");
  });

  it("fires when activeElement is null (focus on the chat root itself)", () => {
    const handled = handleChatKeyDown(
      makeKeyEvent({ key: "[", ctrlKey: true }),
      null,
      focused,
      actions,
      false,
      false, // isMac = false so ctrlKey is the mod key
    );

    expect(handled).toBe(true);
    expect(actions.collapseCard).toHaveBeenCalledWith("task-abc", "card-1");
  });
});

describe("handleChatKeyDown — Vim shortcuts gating", () => {
  let actions: ChatShortcutActions;
  let focused: ChatFocusContext;
  const div = { tagName: "DIV", getAttribute: () => null } as unknown as Element;

  beforeEach(() => {
    actions = makeActions();
    focused = makeFocusContext();
  });

  it("j does NOT fire when vimNavigation is off", () => {
    const handled = handleChatKeyDown(
      makeKeyEvent({ key: "j" }),
      div,
      focused,
      actions,
      false,
    );

    expect(handled).toBe(false);
    expect(actions.moveFocusDown).not.toHaveBeenCalled();
  });

  it("k does NOT fire when vimNavigation is off", () => {
    const handled = handleChatKeyDown(
      makeKeyEvent({ key: "k" }),
      div,
      focused,
      actions,
      false,
    );

    expect(handled).toBe(false);
    expect(actions.moveFocusUp).not.toHaveBeenCalled();
  });

  it("j fires when vimNavigation is on", () => {
    const handled = handleChatKeyDown(
      makeKeyEvent({ key: "j" }),
      div,
      focused,
      actions,
      true,
    );

    expect(handled).toBe(true);
    expect(actions.moveFocusDown).toHaveBeenCalled();
  });

  it("k fires when vimNavigation is on", () => {
    const handled = handleChatKeyDown(
      makeKeyEvent({ key: "k" }),
      div,
      focused,
      actions,
      true,
    );

    expect(handled).toBe(true);
    expect(actions.moveFocusUp).toHaveBeenCalled();
  });

  it("j does NOT fire in a textarea even when vimNavigation is on", () => {
    const textarea = {
      tagName: "TEXTAREA",
      getAttribute: () => null,
    } as unknown as Element;

    const handled = handleChatKeyDown(
      makeKeyEvent({ key: "j" }),
      textarea,
      focused,
      actions,
      true,
    );

    expect(handled).toBe(false);
    expect(actions.moveFocusDown).not.toHaveBeenCalled();
  });
});
