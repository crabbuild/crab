// ─── Browser Bookmarks ─────────────────────────────────────────────────────
// Persisted in localStorage, keyed by URL for fast lookups.

export interface BookmarkEntry {
  url: string;
  title: string;
  addedAt: number;
}

const BM_KEY = "crab.browserBookmarks.v1";

export function loadBookmarks(): BookmarkEntry[] {
  try {
    const raw = localStorage.getItem(BM_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(
      (e): e is BookmarkEntry =>
        typeof e.url === "string" &&
        typeof e.title === "string" &&
        typeof e.addedAt === "number"
    );
  } catch {
    return [];
  }
}

function saveBookmarks(bookmarks: BookmarkEntry[]): void {
  try {
    localStorage.setItem(BM_KEY, JSON.stringify(bookmarks.slice(0, 500)));
  } catch {
    // best-effort
  }
}

export function isBookmarked(url: string, bookmarks: readonly BookmarkEntry[]): boolean {
  return bookmarks.some((b) => b.url === url);
}

export function toggleBookmark(
  url: string,
  title: string,
  bookmarks: BookmarkEntry[]
): BookmarkEntry[] {
  const existing = bookmarks.findIndex((b) => b.url === url);
  if (existing >= 0) {
    const next = [...bookmarks];
    next.splice(existing, 1);
    saveBookmarks(next);
    return next;
  }
  const next = [{ url, title: title || url, addedAt: Date.now() }, ...bookmarks];
  saveBookmarks(next);
  return next;
}
