import { useEffect, useState } from "react";
import type { CommandResult } from "./command-registry";

// --- Types ---

export interface UndoEntry {
  id: string;
  /** Human-readable description, e.g., "Deleted branch feature/foo" */
  description: string;
  /** The inverse operation to execute for undo. */
  undoAction: () => Promise<CommandResult>;
  /** Timestamp when the operation was performed. */
  timestamp: number;
  /** Whether this entry has already been undone. */
  undone: boolean;
}

// --- Constants ---

/** Time window (ms) during which undo is available. */
export const UNDO_WINDOW_MS = 10_000;

// --- Pub-sub internals ---

type Subscriber = (stack: UndoEntry[]) => void;

let nextId = 0;
let undoStack: UndoEntry[] = [];
const subscribers = new Set<Subscriber>();

function notify() {
  const snapshot = [...undoStack];
  for (const sub of subscribers) {
    sub(snapshot);
  }
}

// --- Public API ---

/**
 * Push a new undo entry onto the stack.
 * Generates a unique id, sets timestamp to now, sets undone to false.
 * Returns the generated entry id.
 */
export function pushUndo(entry: Omit<UndoEntry, "id" | "timestamp" | "undone">): string {
  const id = `undo-${++nextId}-${Date.now()}`;
  const full: UndoEntry = {
    ...entry,
    id,
    timestamp: Date.now(),
    undone: false,
  };
  undoStack = [...undoStack, full];
  notify();
  return id;
}

/**
 * Execute the undo action for the given entry id.
 * Returns the result of the undo action, or an error if the entry
 * is not found, already undone, or the undo window has expired.
 */
export async function executeUndo(id: string): Promise<CommandResult> {
  const entry = undoStack.find((e) => e.id === id);
  if (!entry) {
    return { success: false, message: "Undo entry not found" };
  }

  if (entry.undone) {
    return { success: false, message: "Already undone" };
  }

  if (Date.now() - entry.timestamp >= UNDO_WINDOW_MS) {
    return { success: false, message: "Undo window expired" };
  }

  const result = await entry.undoAction();

  // Mark the entry as undone regardless of result
  undoStack = undoStack.map((e) => (e.id === id ? { ...e, undone: true } : e));
  notify();

  return result;
}

/**
 * Returns the current undo stack.
 */
export function getUndoStack(): UndoEntry[] {
  return [...undoStack];
}

/**
 * React hook that subscribes to the undo stack.
 * Re-renders the component whenever the stack changes.
 */
export function useUndoStack(): UndoEntry[] {
  const [snapshot, setSnapshot] = useState<UndoEntry[]>(undoStack);

  useEffect(() => {
    // Sync immediately in case stack changed between render and effect.
    setSnapshot([...undoStack]);
    const sub: Subscriber = (next) => setSnapshot(next);
    subscribers.add(sub);
    return () => {
      subscribers.delete(sub);
    };
  }, []);

  return snapshot;
}
