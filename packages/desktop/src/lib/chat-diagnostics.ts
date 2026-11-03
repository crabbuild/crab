/**
 * Renderer-local observability counters for the chat surface.
 *
 * Tracks interaction metrics (collapse/expand, keyboard shortcuts, etc.)
 * purely in-process. Never sent off-machine. Surfaced via `console.debug`
 * when `CRAB_CHAT_DEBUG` is set to "1" in localStorage.
 */

// ─── Counter names ───────────────────────────────────────────────────────────

export type ChatDiagnosticCounter =
  | "cardsCollapsed"
  | "cardsExpanded"
  | "groupsCollapsed"
  | "groupsExpanded"
  | "compactToggleFlips"
  | "timelineRailScrolls"
  | "keyboardShortcutFires"
  | "dupTextCollisions";

// ─── Internal state ──────────────────────────────────────────────────────────

const counters: Record<ChatDiagnosticCounter, number> = {
  cardsCollapsed: 0,
  cardsExpanded: 0,
  groupsCollapsed: 0,
  groupsExpanded: 0,
  compactToggleFlips: 0,
  timelineRailScrolls: 0,
  keyboardShortcutFires: 0,
  dupTextCollisions: 0,
};

function isDebugEnabled(): boolean {
  try {
    return localStorage.getItem("CRAB_CHAT_DEBUG") === "1";
  } catch {
    return false;
  }
}

function debugLog(counter: ChatDiagnosticCounter): void {
  if (isDebugEnabled()) {
    // eslint-disable-next-line no-console
    console.debug(
      `[chat-diagnostics] ${counter}: ${counters[counter]}`,
    );
  }
}

// ─── Public API ──────────────────────────────────────────────────────────────

/** Increment a single counter by 1 and log when debug is enabled. */
export function incrementChatDiagnostic(counter: ChatDiagnosticCounter): void {
  counters[counter] += 1;
  debugLog(counter);
}

/** Convenience: increment `cardsCollapsed`. */
export function trackCardCollapsed(): void {
  incrementChatDiagnostic("cardsCollapsed");
}

/** Convenience: increment `cardsExpanded`. */
export function trackCardExpanded(): void {
  incrementChatDiagnostic("cardsExpanded");
}

/** Convenience: increment `groupsCollapsed`. */
export function trackGroupCollapsed(): void {
  incrementChatDiagnostic("groupsCollapsed");
}

/** Convenience: increment `groupsExpanded`. */
export function trackGroupExpanded(): void {
  incrementChatDiagnostic("groupsExpanded");
}

/** Convenience: increment `compactToggleFlips`. */
export function trackCompactToggleFlip(): void {
  incrementChatDiagnostic("compactToggleFlips");
}

/** Convenience: increment `timelineRailScrolls`. */
export function trackTimelineRailScroll(): void {
  incrementChatDiagnostic("timelineRailScrolls");
}

/** Convenience: increment `keyboardShortcutFires`. */
export function trackKeyboardShortcutFire(): void {
  incrementChatDiagnostic("keyboardShortcutFires");
}

/** Convenience: increment `dupTextCollisions`. */
export function trackDupTextCollision(): void {
  incrementChatDiagnostic("dupTextCollisions");
}

/** Returns a snapshot of all current counter values. */
export function getChatDiagnostics(): Readonly<Record<ChatDiagnosticCounter, number>> {
  return { ...counters };
}

/** Resets all counters to zero. Useful for testing. */
export function resetChatDiagnostics(): void {
  for (const key of Object.keys(counters) as ChatDiagnosticCounter[]) {
    counters[key] = 0;
  }
}
