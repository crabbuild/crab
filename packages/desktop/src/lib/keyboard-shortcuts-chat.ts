/**
 * Chat-scoped keyboard shortcut handler.
 *
 * Shortcuts are attached via React `onKeyDown` on the `<ChatTranscript>`
 * root (not `window.addEventListener`) so other surfaces' shortcuts
 * pre-empt for free.
 *
 * The handler gates on `document.activeElement` being inside the chat
 * column — if focus is in a textarea (e.g. ChatInput), shortcuts that
 * would conflict with text editing do NOT fire.
 */

import type { CardId } from "./chat-ui-state";

// ─── Focus context ───────────────────────────────────────────────────────────

export interface ChatFocusContext {
  /** The Card_Id of the currently focused tool card, if any. */
  cardId: CardId | null;
  /** The Group_Id of the currently focused tool group, if any. */
  groupId: string | null;
  /** The turn index of the currently focused turn. */
  turnIndex: number;
  /** The task ID for the current chat session. */
  taskId: string;
}

// ─── Shortcut definition ─────────────────────────────────────────────────────

export type ShortcutId =
  | "expand"
  | "collapse"
  | "expand-turn"
  | "collapse-turn"
  | "vim-down"
  | "vim-up";

export interface ChatShortcut {
  /** Mac: metaKey; other: ctrlKey. "none" means no modifier required. */
  modifier: "mod" | "mod+shift" | "none";
  key: string;
  /** Friendly id; used in the help menu. */
  id: ShortcutId;
  /** Shown in the Help menu. */
  description: string;
}

// ─── Shortcut registry ───────────────────────────────────────────────────────

export const chatShortcuts: ChatShortcut[] = [
  {
    modifier: "mod",
    key: "]",
    id: "expand",
    description: "Expand focused card or group",
  },
  {
    modifier: "mod",
    key: "[",
    id: "collapse",
    description: "Collapse focused card or group",
  },
  {
    modifier: "mod",
    key: "e",
    id: "expand-turn",
    description: "Expand all in current turn",
  },
  {
    modifier: "mod+shift",
    key: "E",
    id: "collapse-turn",
    description: "Collapse all in current turn",
  },
  {
    modifier: "none",
    key: "j",
    id: "vim-down",
    description: "Move focus to next tool card (Vim mode)",
  },
  {
    modifier: "none",
    key: "k",
    id: "vim-up",
    description: "Move focus to previous tool card (Vim mode)",
  },
];

// ─── Focus context extraction ────────────────────────────────────────────────

/**
 * Extract a ChatFocusContext from a DOM element inside the chat transcript.
 * Walks up the DOM tree looking for `data-card-id`, `data-group-id`, and
 * `data-turn-index` attributes set by the Turn, ToolGroup, and tool card
 * components.
 *
 * Intended to be called on `focusin` events on the chat root to keep the
 * focus context up to date.
 */
export function extractFocusFromElement(
  target: Element | null,
  taskId: string,
): ChatFocusContext {
  const ctx: ChatFocusContext = {
    cardId: null,
    groupId: null,
    turnIndex: 0,
    taskId,
  };

  let el: Element | null = target;
  while (el) {
    if (!ctx.cardId) {
      const cardId = el.getAttribute("data-card-id");
      if (cardId) ctx.cardId = cardId;
    }
    if (!ctx.groupId) {
      const groupId = el.getAttribute("data-group-id");
      if (groupId) ctx.groupId = groupId;
    }
    const turnAttr = el.getAttribute("data-turn-index");
    if (turnAttr !== null && ctx.turnIndex === 0) {
      const parsed = parseInt(turnAttr, 10);
      if (!isNaN(parsed)) ctx.turnIndex = parsed;
    }
    el = el.parentElement;
  }

  return ctx;
}

// ─── Modifier matching ───────────────────────────────────────────────────────

/**
 * Detect whether the current platform uses Meta (Cmd) as the primary
 * modifier key. Exported for testability.
 */
export function detectIsMac(): boolean {
  return (
    typeof navigator !== "undefined" &&
    /Mac|iPhone|iPad/.test(navigator.platform ?? "")
  );
}

/**
 * Check if the modifier requirement is satisfied by the keyboard event.
 *
 * On Mac, "mod" maps to metaKey (Cmd). On other platforms, "mod" maps
 * to ctrlKey. Pass `isMac` explicitly for testability; defaults to
 * platform detection.
 */
export function modifierMatches(
  modifier: ChatShortcut["modifier"],
  event: { metaKey: boolean; ctrlKey: boolean; shiftKey: boolean },
  isMac: boolean = detectIsMac(),
): boolean {
  const modKey = isMac ? event.metaKey : event.ctrlKey;

  switch (modifier) {
    case "mod":
      return modKey && !event.shiftKey;
    case "mod+shift":
      return modKey && event.shiftKey;
    case "none":
      return !event.metaKey && !event.ctrlKey && !event.shiftKey;
  }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

export interface ChatShortcutActions {
  expandCard: (taskId: string, cardId: CardId) => void;
  collapseCard: (taskId: string, cardId: CardId) => void;
  expandGroup: (taskId: string, groupId: string) => void;
  collapseGroup: (taskId: string, groupId: string) => void;
  expandTurn: (taskId: string, turnIndex: number) => void;
  collapseTurn: (taskId: string, turnIndex: number) => void;
  moveFocusDown: () => void;
  moveFocusUp: () => void;
}

/**
 * Determine whether the active element is a text input (textarea, input,
 * or contenteditable) that should suppress chat shortcuts.
 */
export function isTextInputFocused(activeElement: Element | null): boolean {
  if (!activeElement) return false;

  const tag = activeElement.tagName.toLowerCase();
  if (tag === "textarea") return true;
  if (tag === "input") {
    const type = (activeElement as HTMLInputElement).type?.toLowerCase();
    // Only text-like inputs suppress shortcuts
    return !type || ["text", "search", "url", "email", "password", "tel"].includes(type);
  }
  if (activeElement.getAttribute("contenteditable") === "true") return true;

  return false;
}

/**
 * Match a keyboard event against the shortcut registry and return the
 * matching shortcut, or null if no match.
 *
 * Vim shortcuts (`j`/`k`) only match when `vimNavigationEnabled` is true.
 */
export function matchShortcut(
  event: { key: string; metaKey: boolean; ctrlKey: boolean; shiftKey: boolean },
  vimNavigationEnabled: boolean,
  isMac: boolean = detectIsMac(),
): ChatShortcut | null {
  for (const shortcut of chatShortcuts) {
    // Skip vim shortcuts when vim navigation is off
    if (
      (shortcut.id === "vim-down" || shortcut.id === "vim-up") &&
      !vimNavigationEnabled
    ) {
      continue;
    }

    if (shortcut.key === event.key && modifierMatches(shortcut.modifier, event, isMac)) {
      return shortcut;
    }
  }
  return null;
}

/**
 * Handle a matched shortcut by dispatching to the appropriate action.
 *
 * Returns true if the shortcut was handled (caller should preventDefault),
 * false otherwise (fall through to default browser behaviour).
 */
export function handleChatShortcut(
  shortcut: ChatShortcut,
  focused: ChatFocusContext,
  actions: ChatShortcutActions,
): boolean {
  switch (shortcut.id) {
    case "expand":
      if (focused.cardId) {
        actions.expandCard(focused.taskId, focused.cardId);
        return true;
      }
      if (focused.groupId) {
        actions.expandGroup(focused.taskId, focused.groupId);
        return true;
      }
      return false;

    case "collapse":
      if (focused.cardId) {
        actions.collapseCard(focused.taskId, focused.cardId);
        return true;
      }
      if (focused.groupId) {
        actions.collapseGroup(focused.taskId, focused.groupId);
        return true;
      }
      return false;

    case "expand-turn":
      actions.expandTurn(focused.taskId, focused.turnIndex);
      return true;

    case "collapse-turn":
      actions.collapseTurn(focused.taskId, focused.turnIndex);
      return true;

    case "vim-down":
      actions.moveFocusDown();
      return true;

    case "vim-up":
      actions.moveFocusUp();
      return true;

    default:
      return false;
  }
}

/**
 * Top-level keyboard event handler for the chat surface.
 *
 * Intended to be used as the `onKeyDown` handler on the ChatTranscript root.
 * Returns true if the event was handled (caller should call preventDefault).
 */
export function handleChatKeyDown(
  event: {
    key: string;
    metaKey: boolean;
    ctrlKey: boolean;
    shiftKey: boolean;
  },
  activeElement: Element | null,
  focused: ChatFocusContext,
  actions: ChatShortcutActions,
  vimNavigationEnabled: boolean,
  isMac: boolean = detectIsMac(),
): boolean {
  // If focus is in a text input, do not intercept shortcuts
  if (isTextInputFocused(activeElement)) {
    return false;
  }

  const shortcut = matchShortcut(event, vimNavigationEnabled, isMac);
  if (!shortcut) return false;

  return handleChatShortcut(shortcut, focused, actions);
}


// ─── Display helpers ─────────────────────────────────────────────────────────

/**
 * Format a shortcut for display in the help menu.
 * Returns a human-readable string like "⌘]" or "Ctrl+]".
 */
export function formatShortcut(shortcut: ChatShortcut, mac: boolean = detectIsMac()): string {
  const modPrefix = mac
    ? shortcut.modifier === "mod"
      ? "⌘"
      : shortcut.modifier === "mod+shift"
        ? "⌘⇧"
        : ""
    : shortcut.modifier === "mod"
      ? "Ctrl+"
      : shortcut.modifier === "mod+shift"
        ? "Ctrl+Shift+"
        : "";

  const keyDisplay = shortcut.key.length === 1
    ? shortcut.key.toUpperCase()
    : shortcut.key;

  return `${modPrefix}${keyDisplay}`;
}
