/**
 * Per-task chat UI state store.
 *
 * Hand-rolled external store consumed via React's `useSyncExternalStore`.
 * Each mutator schedules a debounced (300 ms) flush to the main process
 * via `crab.orchestrator.setTaskUIState`. On flush, if the serialised
 * state exceeds 64 KiB, the oldest `cardCollapse` entries (by
 * `lastToggledMs`) are evicted until the budget is met.
 */

import { useSyncExternalStore } from "react";

// ─── Types ───────────────────────────────────────────────────────────────────

export type CardId = string;

export interface CardCollapseEntry {
  collapsed: boolean;
  /** Wall-clock ms when the user last toggled this entry. Used by
   *  the LRU eviction — if the per-task collapse map exceeds the
   *  size budget, we drop the oldest entries first. */
  lastToggledMs: number;
}

export interface ChatUIStatePerTask {
  /** Per-card overrides. Absent means "follow the default rule":
   *  collapsed when the underlying tool call is complete, expanded
   *  otherwise. */
  cardCollapse: Record<CardId, CardCollapseEntry>;
  /** Per-group overrides. Same default-rule semantics. */
  groupCollapse: Record<string, CardCollapseEntry>;
  /** Per-turn overrides. */
  turnCollapse: Record<number, CardCollapseEntry>;
  /** When false, every card and group renders expanded regardless
   *  of the per-card overrides above. */
  compactView: boolean;
  /** Density of the timeline rail. `off` hides it. */
  timelineRail: "compact" | "off";
  /** Vim-style j/k navigation on the chat. Default false. */
  vimNavigation: boolean;
}

export interface ChatUIStateStore {
  /** Map<taskId, ChatUIStatePerTask>. */
  byTask: Map<string, ChatUIStatePerTask>;

  setCardCollapsed(taskId: string, cardId: CardId, collapsed: boolean): void;
  setGroupCollapsed(taskId: string, groupId: string, collapsed: boolean): void;
  setTurnCollapsed(taskId: string, turnIndex: number, collapsed: boolean): void;
  setCompactView(taskId: string, on: boolean): void;
  setTimelineRail(taskId: string, density: "compact" | "off"): void;
  setVimNavigation(taskId: string, on: boolean): void;

  /** Idempotent: drop a task's UI state and IPC the deletion. */
  forgetTask(taskId: string): void;

  /** Called once at app startup with the persisted state from disk. */
  hydrate(byTask: Map<string, ChatUIStatePerTask>): void;
}

// ─── Constants ───────────────────────────────────────────────────────────────

const FLUSH_DEBOUNCE_MS = 300;
const MAX_SERIALISED_BYTES = 64 * 1024; // 64 KiB

// ─── Internal state ──────────────────────────────────────────────────────────

let store: ChatUIStateStore = buildStore(new Map());
let storeSnapshot: ChatUIStateStore = store;
const listeners = new Set<() => void>();
const flushTimers = new Map<string, ReturnType<typeof setTimeout>>();

function notify(): void {
  // Create a new snapshot reference so useSyncExternalStore detects the change.
  storeSnapshot = { ...store };
  for (const listener of listeners) {
    listener();
  }
}

// ─── Default state factory ───────────────────────────────────────────────────

export function defaultPerTaskState(): ChatUIStatePerTask {
  return {
    cardCollapse: {},
    groupCollapse: {},
    turnCollapse: {},
    compactView: true,
    timelineRail: "off",
    vimNavigation: false,
  };
}

function ensureTask(taskId: string): ChatUIStatePerTask {
  let state = store.byTask.get(taskId);
  if (!state) {
    state = defaultPerTaskState();
    store.byTask.set(taskId, state);
  }
  return state;
}

// ─── Flush (debounced IPC + LRU eviction) ────────────────────────────────────

function scheduleFlush(taskId: string): void {
  const existing = flushTimers.get(taskId);
  if (existing !== undefined) {
    clearTimeout(existing);
  }
  flushTimers.set(
    taskId,
    setTimeout(() => {
      flushTimers.delete(taskId);
      flushTask(taskId);
    }, FLUSH_DEBOUNCE_MS),
  );
}

function flushTask(taskId: string): void {
  const state = store.byTask.get(taskId);
  if (!state) return;

  // LRU eviction: if serialised state exceeds 64 KiB, drop the oldest
  // cardCollapse entries by lastToggledMs until under budget.
  const evicted = applyLruEviction(state);

  // Persist via IPC (fire-and-forget). The IPC may not exist yet if
  // task 1.4 hasn't shipped; guard gracefully.
  try {
    const orchestrator = (window as any)?.crab?.orchestrator;
    if (orchestrator?.setTaskUIState) {
      orchestrator.setTaskUIState(taskId, toSerializable(evicted)).catch(() => {
        // Swallow — persistence is best-effort from the renderer's POV.
      });
    }
  } catch {
    // No IPC available (e.g. running in tests or IPC not wired yet).
  }
}

/** Evict oldest cardCollapse entries until serialised size ≤ 64 KiB. */
export function applyLruEviction(state: ChatUIStatePerTask): ChatUIStatePerTask {
  let serialised = JSON.stringify(toSerializable(state));
  if (serialised.length <= MAX_SERIALISED_BYTES) return state;

  // Clone cardCollapse so we can mutate.
  const cardCollapse = { ...state.cardCollapse };

  // Sort entries by lastToggledMs ascending (oldest first).
  const entries = Object.entries(cardCollapse).sort(
    (a, b) => a[1].lastToggledMs - b[1].lastToggledMs,
  );

  let idx = 0;
  while (serialised.length > MAX_SERIALISED_BYTES && idx < entries.length) {
    delete cardCollapse[entries[idx][0]];
    idx++;
    // Re-serialise to check size.
    const candidate = { ...state, cardCollapse };
    serialised = JSON.stringify(toSerializable(candidate));
  }

  const result = { ...state, cardCollapse };
  // Update the in-memory store with the evicted state.
  store.byTask.set(
    // Find the taskId for this state — we need to look it up.
    findTaskIdForState(state) ?? "",
    result,
  );
  return result;
}

function findTaskIdForState(state: ChatUIStatePerTask): string | undefined {
  for (const [taskId, s] of store.byTask) {
    if (s === state) return taskId;
  }
  return undefined;
}

// ─── Serialisation helpers ───────────────────────────────────────────────────

/** Convert to a plain object suitable for JSON / IPC. */
function toSerializable(state: ChatUIStatePerTask): Record<string, unknown> {
  return {
    cardCollapse: state.cardCollapse,
    groupCollapse: state.groupCollapse,
    turnCollapse: state.turnCollapse,
    compactView: state.compactView,
    timelineRail: state.timelineRail,
    vimNavigation: state.vimNavigation,
  };
}

// ─── Store builder ───────────────────────────────────────────────────────────

function buildStore(byTask: Map<string, ChatUIStatePerTask>): ChatUIStateStore {
  return {
    byTask,

    setCardCollapsed(taskId: string, cardId: CardId, collapsed: boolean): void {
      const state = ensureTask(taskId);
      state.cardCollapse = {
        ...state.cardCollapse,
        [cardId]: { collapsed, lastToggledMs: Date.now() },
      };
      scheduleFlush(taskId);
      notify();
    },

    setGroupCollapsed(taskId: string, groupId: string, collapsed: boolean): void {
      const state = ensureTask(taskId);
      state.groupCollapse = {
        ...state.groupCollapse,
        [groupId]: { collapsed, lastToggledMs: Date.now() },
      };
      scheduleFlush(taskId);
      notify();
    },

    setTurnCollapsed(taskId: string, turnIndex: number, collapsed: boolean): void {
      const state = ensureTask(taskId);
      state.turnCollapse = {
        ...state.turnCollapse,
        [turnIndex]: { collapsed, lastToggledMs: Date.now() },
      };
      scheduleFlush(taskId);
      notify();
    },

    setCompactView(taskId: string, on: boolean): void {
      const state = ensureTask(taskId);
      state.compactView = on;
      scheduleFlush(taskId);
      notify();
    },

    setTimelineRail(taskId: string, density: "compact" | "off"): void {
      const state = ensureTask(taskId);
      state.timelineRail = density;
      scheduleFlush(taskId);
      notify();
    },

    setVimNavigation(taskId: string, on: boolean): void {
      const state = ensureTask(taskId);
      state.vimNavigation = on;
      scheduleFlush(taskId);
      notify();
    },

    forgetTask(taskId: string): void {
      store.byTask.delete(taskId);

      // Cancel any pending flush.
      const timer = flushTimers.get(taskId);
      if (timer !== undefined) {
        clearTimeout(timer);
        flushTimers.delete(taskId);
      }

      // IPC the deletion (fire-and-forget).
      try {
        const orchestrator = (window as any)?.crab?.orchestrator;
        if (orchestrator?.setTaskUIState) {
          orchestrator.setTaskUIState(taskId, null).catch(() => {});
        }
      } catch {
        // No IPC available.
      }

      notify();
    },

    hydrate(byTask: Map<string, ChatUIStatePerTask>): void {
      store.byTask = byTask;
      notify();
    },
  };
}

// ─── React hook ──────────────────────────────────────────────────────────────

function getSnapshot(): ChatUIStateStore {
  return storeSnapshot;
}

function getServerSnapshot(): ChatUIStateStore {
  return storeSnapshot;
}

function subscribe(onStoreChange: () => void): () => void {
  listeners.add(onStoreChange);
  return () => {
    listeners.delete(onStoreChange);
  };
}

/**
 * React hook that subscribes to the chat UI state store.
 * Re-renders the component whenever any mutation fires.
 */
export function useChatUIState(): ChatUIStateStore {
  return useSyncExternalStore(subscribe, getSnapshot, getServerSnapshot);
}

// ─── Imperative access (for non-React code, tests, hydration) ────────────────

/** Direct access to the store for imperative use outside React. */
export function getChatUIState(): ChatUIStateStore {
  return store;
}

/**
 * Reset the store (useful for tests). Clears all state and pending timers.
 */
export function resetChatUIState(): void {
  for (const timer of flushTimers.values()) {
    clearTimeout(timer);
  }
  flushTimers.clear();
  store = buildStore(new Map());
  storeSnapshot = store;
  notify();
}
