import { onAgentEvent, rpc, type HydrationEntry, type HydrationSummary } from "../ipc";

interface Snapshot {
  summary: HydrationSummary | null;
  byPath: Map<string, HydrationEntry>;
  loading: boolean;
  error: string | null;
}

interface Entry {
  repoId: string;
  listeners: Set<() => void>;
  snapshot: Snapshot;
  version: number;
  pending: boolean;
}

const EMPTY: Snapshot = { summary: null, byPath: new Map(), loading: false, error: null };
const entries = new Map<string, Entry>();
let disconnect: (() => void) | undefined;

function publish(entry: Entry, snapshot: Snapshot) {
  entry.snapshot = snapshot;
  for (const listener of entry.listeners) listener();
}

function current(entry: Entry) { return entries.get(entry.repoId) === entry; }

function read(entry: Entry) {
  if (entry.pending) return;
  entry.pending = true;
  const version = entry.version;
  void rpc.hydrationStatus(entry.repoId).then(data => {
    if (!current(entry) || version !== entry.version) return;
    publish(entry, { summary: data.summary, byPath: new Map(data.items.map(item => [item.path, item])), loading: false, error: null });
  }).catch((error: unknown) => {
    if (!current(entry) || version !== entry.version) return;
    publish(entry, { ...EMPTY, error: error instanceof Error ? error.message : String(error) });
  }).finally(() => {
    entry.pending = false;
    // A completed mutation invalidates even a read already in flight. Drain it
    // before fetching again, and never publish its earlier filesystem view.
    if (current(entry) && version !== entry.version) read(entry);
  });
}

function refresh(repoId: string) {
  const entry = entries.get(repoId);
  if (!entry) return;
  entry.version++;
  publish(entry, { ...EMPTY, loading: true });
  read(entry);
}

function subscribe(repoId: string, listener: () => void) {
  let entry = entries.get(repoId);
  const first = !entry;
  if (!entry) {
    entry = { repoId, listeners: new Set(), snapshot: EMPTY, version: 0, pending: false };
    entries.set(repoId, entry);
  }
  const subscribed = entry;
  const notify = () => listener();
  entry.listeners.add(notify);
  if (!disconnect) disconnect = onAgentEvent(event => {
    if (event.method !== "progress/complete" || !event.params || typeof event.params !== "object") return;
    const { op } = event.params as { op?: unknown };
    if (op !== "hydrate" && op !== "dehydrate") return;
    // CLI completion frames have a job ID but no repository. Each subscribed
    // snapshot refreshes through its own repository route, including failures
    // that may have materialized only part of the requested inventory.
    for (const repoId of [...entries.keys()]) refresh(repoId);
  });
  if (first) refresh(repoId);
  return () => {
    if (!subscribed.listeners.delete(notify)) return;
    if (subscribed.listeners.size > 0) return;
    entries.delete(repoId);
    if (entries.size === 0) { disconnect?.(); disconnect = undefined; }
  };
}

export const hydrationStatusStore = {
  subscribe,
  refresh,
  snapshot: (repoId?: string): Snapshot => repoId === undefined ? EMPTY : entries.get(repoId)?.snapshot ?? EMPTY,
};
