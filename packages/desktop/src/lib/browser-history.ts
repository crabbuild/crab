// ─── Browser History ───────────────────────────────────────────────────────
// Persisted navigation history with visit counts and recency tracking.

export interface HistoryEntry {
  url: string;
  title: string;
  lastVisitedAt: number;
  visitCount: number;
}

const HIST_KEY = "crab.browserHistory.v1";
const MAX_ENTRIES = 500;

export function loadHistory(): HistoryEntry[] {
  try {
    const raw = localStorage.getItem(HIST_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (e): e is HistoryEntry =>
        typeof e.url === "string" &&
        typeof e.title === "string" &&
        typeof e.lastVisitedAt === "number" &&
        typeof e.visitCount === "number"
    );
  } catch {
    return [];
  }
}

function saveHistory(entries: HistoryEntry[]): void {
  try {
    localStorage.setItem(HIST_KEY, JSON.stringify(entries.slice(0, MAX_ENTRIES)));
  } catch {
    // best-effort
  }
}

export function recordVisit(url: string, title: string): HistoryEntry[] {
  if (!url || url.startsWith("about:") || url.startsWith("data:")) {
    return loadHistory();
  }
  const entries = loadHistory();
  const existing = entries.findIndex((e) => e.url === url);
  const now = Date.now();
  if (existing >= 0) {
    entries[existing] = {
      ...entries[existing],
      lastVisitedAt: now,
      visitCount: entries[existing].visitCount + 1,
      title: title || entries[existing].title,
    };
  } else {
    entries.unshift({ url, title: title || url, lastVisitedAt: now, visitCount: 1 });
  }
  saveHistory(entries);
  return entries;
}

export function clearHistory(): void {
  try {
    localStorage.removeItem(HIST_KEY);
  } catch {
    // best-effort
  }
}

export function removeHistoryEntry(url: string): HistoryEntry[] {
  const entries = loadHistory().filter((e) => e.url !== url);
  saveHistory(entries);
  return entries;
}
