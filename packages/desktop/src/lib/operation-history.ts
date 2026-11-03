import { useEffect, useState } from "react";

// --- Types ---

export interface OperationRecord {
  id: string;
  commandId: string;
  label: string;
  timestamp: number;
  durationMs: number;
  status: "success" | "error" | "conflict";
  message?: string;
  undoId?: string;
}

// --- Constants ---

/** Max entries retained. Oldest are evicted. */
export const MAX_HISTORY = 200;

// --- Pub-sub internals ---

type Subscriber = (history: OperationRecord[]) => void;

let nextId = 0;
let history: OperationRecord[] = [];
const subscribers = new Set<Subscriber>();

function notify() {
  const snapshot = [...history];
  for (const sub of subscribers) {
    sub(snapshot);
  }
}

// --- Public API ---

/**
 * Record a new operation. Generates a unique id, adds to history,
 * evicts oldest entries when over MAX_HISTORY, and notifies subscribers.
 * Returns the generated record id.
 */
export function recordOperation(record: Omit<OperationRecord, "id">): string {
  const id = `op-${++nextId}-${Date.now()}`;
  const full: OperationRecord = { ...record, id };
  history = [...history, full];

  if (history.length > MAX_HISTORY) {
    history = history.slice(history.length - MAX_HISTORY);
  }

  notify();
  return id;
}

/**
 * Returns the current operation history snapshot.
 */
export function getHistory(): OperationRecord[] {
  return [...history];
}

/**
 * React hook that subscribes to operation history changes.
 * Re-renders the component whenever history changes.
 */
export function useOperationHistory(): OperationRecord[] {
  const [snapshot, setSnapshot] = useState<OperationRecord[]>(history);

  useEffect(() => {
    setSnapshot([...history]);
    const sub: Subscriber = (next) => setSnapshot(next);
    subscribers.add(sub);
    return () => {
      subscribers.delete(sub);
    };
  }, []);

  return snapshot;
}

/**
 * Clears all operation history and notifies subscribers.
 */
export function clearHistory(): void {
  history = [];
  notify();
}
