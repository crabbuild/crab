import { useCallback, useEffect, useState } from "react";

// --- Toast types ---

export interface Toast {
  id: string;
  kind: "info" | "success" | "error" | "warning";
  title: string;
  body?: string;
  duration?: number; // ms, default 5000
  action?: { label: string; onClick: () => void };
}

// --- Pub-sub internals ---

type Subscriber = (toasts: Toast[]) => void;

let nextId = 0;
let toasts: Toast[] = [];
const subscribers = new Set<Subscriber>();

function notify() {
  const snapshot = [...toasts];
  for (const sub of subscribers) {
    sub(snapshot);
  }
}

// --- Public API ---

const MAX_VISIBLE = 5;
const DEFAULT_DURATION = 5000;

/**
 * Publish a new toast. Returns the generated toast id.
 * If more than MAX_VISIBLE toasts are active, the oldest is dismissed.
 */
export function publishToast(toast: Omit<Toast, "id">): string {
  const id = `toast-${++nextId}-${Date.now()}`;
  const full: Toast = { ...toast, id, duration: toast.duration ?? DEFAULT_DURATION };
  toasts = [...toasts, full];

  // Enforce max visible — dismiss oldest when over the cap.
  if (toasts.length > MAX_VISIBLE) {
    toasts = toasts.slice(toasts.length - MAX_VISIBLE);
  }

  notify();
  return id;
}

/**
 * Dismiss a toast by id.
 */
export function dismissToast(id: string): void {
  toasts = toasts.filter((t) => t.id !== id);
  notify();
}

/**
 * React hook that subscribes to the toast list.
 * Re-renders the component whenever toasts change.
 */
export function useToasts(): Toast[] {
  const [snapshot, setSnapshot] = useState<Toast[]>(toasts);

  useEffect(() => {
    // Sync immediately in case toasts changed between render and effect.
    setSnapshot([...toasts]);
    const sub: Subscriber = (next) => setSnapshot(next);
    subscribers.add(sub);
    return () => { subscribers.delete(sub); };
  }, []);

  return snapshot;
}

/**
 * Hook that returns a stable publishToast callback (useful for memoized components).
 */
export function usePublishToast() {
  return useCallback((toast: Omit<Toast, "id">) => publishToast(toast), []);
}
