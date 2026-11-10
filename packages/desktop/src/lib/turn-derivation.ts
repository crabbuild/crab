/**
 * Pure derivation helpers that turn a flat AgentEvent[] stream into
 * the structured Turn[] and ToolGroup[] primitives the chat surface
 * renders against.
 *
 * These functions are renderer-only — no IPC, no side effects, no
 * mutation of the input arrays.
 */

import type { AgentEvent } from "../../electron/orchestrator-adapters";

// ─── Public types ────────────────────────────────────────────────────────────

export type CardId = string;

export interface ToolGroup {
  /** Group identity, derived from the first member's Card_Id. */
  groupId: string;
  /** Indices into the Turn's events array (not the global events array). */
  eventIndices: number[];
  /** True iff every tool_use in this group has a matching tool_result. */
  complete: boolean;
  /** Timestamp of the first tool_use event in the group. */
  startedAt: number;
  /** Timestamp of the last tool_result (or last tool_use if incomplete). */
  endedAt: number | null;
}

export interface Turn {
  /** Zero-based index of this turn within the task. */
  turnIndex: number;
  /** The user prompt that opened this turn, if any. The first turn
   *  has no user message because the task is started by the
   *  orchestrator's initial prompt, not a user turn. */
  userMessage: string | null;
  /** All AgentEvents that belong to this turn, in arrival order. */
  events: AgentEvent[];
  /** ToolGroups derived from `events`. Every tool_use is in exactly
   *  one ToolGroup; non-tool events are not. */
  toolGroups: ToolGroup[];
  /** Count of distinct file paths edited in this turn. */
  fileEditCount: number;
  /** Distinct file paths edited in this turn. */
  fileEditPaths: string[];
  /** Sum of added lines across all file_edit events in this turn. */
  totalAdded: number;
  /** Sum of removed lines across all file_edit events in this turn. */
  totalRemoved: number;
  /** True iff a `done` event has arrived for this turn. */
  complete: boolean;
  /** Wall-clock duration in ms; null while running. */
  durationMs: number | null;
  outcome?: AgentEvent["outcome"];
}

// ─── Internal helpers ────────────────────────────────────────────────────────

/**
 * Parse +X / -Y line counts from a unified diff string.
 * Counts lines starting with `+` (excluding `+++`) as added,
 * and lines starting with `-` (excluding `---`) as removed.
 */
function parseDiffStats(patch: string): { added: number; removed: number } {
  if (!patch) return { added: 0, removed: 0 };

  let added = 0;
  let removed = 0;

  const lines = patch.split("\n");
  for (const line of lines) {
    if (line.startsWith("+++")) continue;
    if (line.startsWith("---")) continue;
    if (line.startsWith("+")) added++;
    else if (line.startsWith("-")) removed++;
  }

  return { added, removed };
}

// ─── Card_Id derivation ──────────────────────────────────────────────────────

/**
 * Stable identity for a tool card across re-renders, scroll, and
 * persistence reloads.
 *
 * Strategy:
 *   1. If the AgentEvent has an itemId, use itemId verbatim.
 *   2. Otherwise, fall back to a deterministic synthesis from
 *      (taskId, turnIndex, eventIndexWithinTurn).
 *
 * Either form is stable for the lifetime of the task.
 */
export function deriveCardId(
  taskId: string,
  turnIndex: number,
  eventIndexInTurn: number,
  event: AgentEvent,
): CardId {
  if (event.itemId) return event.itemId;
  return `${taskId}:${turnIndex}:${eventIndexInTurn}`;
}

// ─── Group_Id derivation ─────────────────────────────────────────────────────

/**
 * Derive a stable group identity from the first member's CardId.
 * The first member never changes once the group exists (events are
 * append-only), so this is stable for the lifetime of the task.
 */
export function deriveGroupId(
  toolGroup: ToolGroup,
  turnEvents: AgentEvent[],
  taskId: string,
  turnIndex: number,
): string {
  const firstIdx = toolGroup.eventIndices[0];
  const firstEvent = turnEvents[firstIdx];
  const firstCardId = deriveCardId(taskId, turnIndex, firstIdx, firstEvent);
  return `g:${firstCardId}`;
}

// ─── ToolGroup derivation ────────────────────────────────────────────────────

/**
 * Returns true if the event is a tool-type event that should be grouped.
 */
function isToolEvent(event: AgentEvent): boolean {
  return event.type === "tool_use" || event.type === "file_edit" ||
         event.type === "file_read" || event.type === "command" ||
         event.type === "command_run" || event.type === "search" ||
         event.type === "web" || event.type === "task" ||
         event.type === "generic" || event.type === "approval_request" || event.type === "input_request";
}

/**
 * Derive ToolGroups from a turn's events array.
 *
 * Contiguity rule: a maximal run of tool-type events with no
 * intervening `text` event whose `content` is non-empty forms one
 * group. `tool_result` events are NOT group members — they update
 * the group's `complete` and `endedAt` fields.
 *
 * A `done` event ends the current group (turn boundary).
 *
 * If `turnComplete` is true, all groups are marked complete regardless
 * of whether individual tool_result events were matched.
 */
export function deriveToolGroups(events: AgentEvent[], turnComplete?: boolean): ToolGroup[] {
  const groups: ToolGroup[] = [];
  let currentGroup: ToolGroup | null = null;

  for (let i = 0; i < events.length; i++) {
    const event = events[i];

    if (isToolEvent(event)) {
      if (!currentGroup) {
        currentGroup = {
          groupId: "", // placeholder — caller assigns via deriveGroupId
          eventIndices: [i],
          complete: false,
          startedAt: event.timestamp,
          endedAt: null,
        };
      } else {
        currentGroup.eventIndices.push(i);
      }
    } else if (event.type === "text" && event.content) {
      // Non-empty text event breaks the current group
      if (currentGroup) {
        groups.push(currentGroup);
        currentGroup = null;
      }
    } else if (event.type === "tool_result") {
      // tool_result updates the most recent group containing a
      // matching tool_use. It is NOT a group member itself.
      const target = findGroupForResult(event, groups, currentGroup, events);
      if (target) {
        target.endedAt = event.timestamp;
        target.complete = isGroupComplete(target, events, i);
      }
    } else if (event.type === "done") {
      // Turn boundary ends the current group
      if (currentGroup) {
        groups.push(currentGroup);
        currentGroup = null;
      }
    }
    // Other event types (thinking, error, etc.) do NOT
    // break a tool group — only non-empty `text` events do.
  }

  // Flush any trailing open group
  if (currentGroup) {
    groups.push(currentGroup);
  }

  // If the turn is complete, mark all groups as complete
  if (turnComplete) {
    for (const group of groups) {
      group.complete = true;
      if (group.endedAt === null && group.eventIndices.length > 0) {
        const lastIdx = group.eventIndices[group.eventIndices.length - 1];
        group.endedAt = events[lastIdx].timestamp;
      }
    }
  }

  return groups;
}

/**
 * Find the group that a tool_result belongs to by matching itemId.
 */
function findGroupForResult(
  resultEvent: AgentEvent,
  finishedGroups: ToolGroup[],
  currentGroup: ToolGroup | null,
  events: AgentEvent[],
): ToolGroup | null {
  if (resultEvent.itemId) {
    // Check current group first (most likely match)
    if (currentGroup) {
      for (const idx of currentGroup.eventIndices) {
        if (events[idx].itemId === resultEvent.itemId) {
          return currentGroup;
        }
      }
    }
    // Check finished groups in reverse order
    for (let g = finishedGroups.length - 1; g >= 0; g--) {
      for (const idx of finishedGroups[g].eventIndices) {
        if (events[idx].itemId === resultEvent.itemId) {
          return finishedGroups[g];
        }
      }
    }
  }

  // Fallback: associate with the current group or the most recent one
  return currentGroup ?? finishedGroups[finishedGroups.length - 1] ?? null;
}

/**
 * Check if all tool_use members in a group have a matching tool_result
 * in the events seen so far (up to index `upToIdx`).
 */
function isGroupComplete(
  group: ToolGroup,
  events: AgentEvent[],
  upToIdx: number,
): boolean {
  for (const memberIdx of group.eventIndices) {
    const toolUseEvent = events[memberIdx];
    if (!toolUseEvent.itemId) {
      // Without itemId we can't definitively pair — use heuristic:
      // if endedAt is set, consider the group complete for single-member
      // groups. For multi-member groups without itemIds, we can't be sure.
      if (group.eventIndices.length === 1) {
        return group.endedAt !== null;
      }
      continue;
    }
    // Check if a tool_result with the same itemId exists up to upToIdx
    let found = false;
    for (let j = 0; j <= upToIdx; j++) {
      if (events[j].type === "tool_result" && events[j].itemId === toolUseEvent.itemId) {
        found = true;
        break;
      }
    }
    if (!found) return false;
  }
  return true;
}

// ─── Turn derivation ─────────────────────────────────────────────────────────

/**
 * Pure function. Given the raw event stream and the user-feedback
 * array (from `Task.feedback`), produces the turn-by-turn breakdown.
 *
 * Boundary rule: a turn ends at every `done` event. The next turn
 * begins at the next event after `done`. The first turn starts at
 * event 0.
 *
 * The feedback array provides user messages that opened each
 * subsequent turn. Turn 0 has no user message (task started by the
 * orchestrator's initial prompt). Turn 1 corresponds to feedback[0],
 * turn 2 to feedback[1], etc.
 */
export function deriveTurns(
  events: AgentEvent[],
  feedback: string[],
): Turn[] {
  if (events.length === 0) return [];

  const turns: Turn[] = [];
  let turnStart = 0;

  for (let i = 0; i < events.length; i++) {
    if (events[i].type === "done") {
      const turnEvents = events.slice(turnStart, i + 1);
      turns.push(buildTurn(turns.length, turnEvents, feedback));
      turnStart = i + 1;
    }
  }

  // Handle trailing events after the last `done` (turn still running)
  if (turnStart < events.length) {
    const turnEvents = events.slice(turnStart);
    turns.push(buildTurn(turns.length, turnEvents, feedback));
  }

  return turns;
}

/**
 * Build a single Turn from its slice of events.
 */
function buildTurn(
  turnIndex: number,
  turnEvents: AgentEvent[],
  feedback: string[],
): Turn {
  const openingMessage = turnEvents[0]?.type === "user_message" ? turnEvents[0] : undefined;
  if (openingMessage) turnEvents = turnEvents.slice(1);
  const textIndices = new Map<string, number>();
  const normalized: AgentEvent[] = [];
  for (const event of turnEvents) {
    if ((event.type === "text" || event.type === "thinking") && event.itemId) {
      const key = `${event.type}:${event.itemId}`;
      const index = textIndices.get(key);
      if (index !== undefined) {
        const previous = normalized[index];
        normalized[index] = { ...previous, content: event.textMode === "replace" ? event.content : (previous.content ?? "") + (event.content ?? "") };
        continue;
      }
      textIndices.set(key, normalized.length);
    }
    normalized.push(event);
  }
  turnEvents = normalized;

  // Determine completion first so we can pass it to deriveToolGroups
  const lastEvent = turnEvents[turnEvents.length - 1];
  const complete = lastEvent?.type === "done";
  const outcome = complete ? lastEvent.outcome : undefined;

  const toolGroups = deriveToolGroups(turnEvents, complete);

  // Assign stable groupIds to each group
  for (const group of toolGroups) {
    group.groupId = deriveGroupId(group, turnEvents, `turn-${turnIndex}`, turnIndex);
  }

  // Turn 0 has no user message. Turn N (N > 0) uses feedback[N - 1].
  const userMessage = openingMessage?.content ?? (turnIndex > 0 ? feedback[turnIndex - 1] ?? null : null);

  // Aggregate file edit stats
  const pathSet = new Set<string>();
  let totalAdded = 0;
  let totalRemoved = 0;

  for (const event of turnEvents) {
    if (event.type === "file_edit" && event.path) {
      pathSet.add(event.path);
      if (event.diff) {
        const stats = parseDiffStats(event.diff);
        totalAdded += stats.added;
        totalRemoved += stats.removed;
      }
    }
  }

  const fileEditPaths = [...pathSet];

  // Duration
  const firstTimestamp = turnEvents[0]?.timestamp ?? 0;
  const lastTimestamp = lastEvent?.timestamp ?? 0;
  const durationMs = complete ? lastTimestamp - firstTimestamp : null;

  return {
    turnIndex,
    userMessage,
    events: turnEvents,
    toolGroups,
    fileEditCount: fileEditPaths.length,
    fileEditPaths,
    totalAdded,
    totalRemoved,
    complete,
    outcome,
    durationMs,
  };
}
