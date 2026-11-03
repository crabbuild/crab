/**
 * OPFS-backed per-tab state eviction.
 *
 * When the heap watcher signals "over cap", the eviction manager
 * serializes the least-recently-focused tab's state to an Origin
 * Private File System file. When the tab regains focus, the state is
 * restored from OPFS and the file is removed.
 *
 * Key format: `tab-state-{tabId}.json`
 *
 * Falls back gracefully (logs a warning) if OPFS is unavailable.
 */

/** Serializable tab state — kept intentionally loose so any view can
 *  stash whatever it needs. */
export type TabState = Record<string, unknown>;

/** Tracks which tabs have been evicted and their LRU order. */
interface TabEntry {
  tabId: string;
  lastFocusedAt: number;
}

function opfsAvailable(): boolean {
  return (
    typeof navigator !== "undefined" &&
    typeof navigator.storage !== "undefined" &&
    typeof navigator.storage.getDirectory === "function"
  );
}

export class OpfsEvictionManager {
  private _available: boolean;
  private _tabOrder: TabEntry[] = [];
  private _evictedTabs = new Set<string>();

  constructor() {
    this._available = opfsAvailable();
    if (!this._available) {
      console.warn("[opfs-eviction] OPFS unavailable — eviction disabled.");
    }
  }

  /** Whether OPFS is available in this environment. */
  get available(): boolean {
    return this._available;
  }

  /** Set of tab IDs currently evicted to OPFS. */
  get evictedTabs(): ReadonlySet<string> {
    return this._evictedTabs;
  }

  /** Record a tab focus event. Call this whenever a tab gains focus. */
  recordFocus(tabId: string): void {
    const now = Date.now();
    const existing = this._tabOrder.find((e) => e.tabId === tabId);
    if (existing) {
      existing.lastFocusedAt = now;
    } else {
      this._tabOrder.push({ tabId, lastFocusedAt: now });
    }
  }

  /** Remove a tab from tracking (e.g. when closed). Also cleans up
   *  any OPFS file if the tab was evicted. */
  async removeTab(tabId: string): Promise<void> {
    this._tabOrder = this._tabOrder.filter((e) => e.tabId !== tabId);
    if (this._evictedTabs.has(tabId)) {
      await this.deleteFile(tabId);
      this._evictedTabs.delete(tabId);
    }
  }

  /** Returns the tab ID of the least-recently-focused tab that hasn't
   *  already been evicted, or null if none are eligible. */
  getLeastRecentlyFocused(): string | null {
    const candidates = this._tabOrder
      .filter((e) => !this._evictedTabs.has(e.tabId))
      .sort((a, b) => a.lastFocusedAt - b.lastFocusedAt);
    return candidates.length > 0 ? candidates[0].tabId : null;
  }

  /** Evict a tab's state to OPFS. */
  async evict(tabId: string, state: TabState): Promise<boolean> {
    if (!this._available) {
      console.warn("[opfs-eviction] Cannot evict — OPFS unavailable.");
      return false;
    }
    try {
      const root = await navigator.storage.getDirectory();
      const fileName = `tab-state-${tabId}.json`;
      const fileHandle = await root.getFileHandle(fileName, { create: true });
      const writable = await fileHandle.createWritable();
      await writable.write(JSON.stringify(state));
      await writable.close();
      this._evictedTabs.add(tabId);
      return true;
    } catch (err) {
      console.warn("[opfs-eviction] Eviction failed:", err);
      return false;
    }
  }

  /** Restore a tab's state from OPFS. Returns null if not found or
   *  if OPFS is unavailable. Removes the file after successful read. */
  async restore(tabId: string): Promise<TabState | null> {
    if (!this._available) {
      console.warn("[opfs-eviction] Cannot restore — OPFS unavailable.");
      return null;
    }
    try {
      const root = await navigator.storage.getDirectory();
      const fileName = `tab-state-${tabId}.json`;
      const fileHandle = await root.getFileHandle(fileName);
      const file = await fileHandle.getFile();
      const text = await file.text();
      const state = JSON.parse(text) as TabState;
      // Clean up the OPFS file after restoring.
      await this.deleteFile(tabId);
      this._evictedTabs.delete(tabId);
      return state;
    } catch (err) {
      // File not found or parse error — not critical.
      console.warn("[opfs-eviction] Restore failed:", err);
      this._evictedTabs.delete(tabId);
      return null;
    }
  }

  /** Check if a specific tab has been evicted. */
  isEvicted(tabId: string): boolean {
    return this._evictedTabs.has(tabId);
  }

  private async deleteFile(tabId: string): Promise<void> {
    try {
      const root = await navigator.storage.getDirectory();
      await root.removeEntry(`tab-state-${tabId}.json`);
    } catch {
      // Ignore — file may already be gone.
    }
  }
}

/** Singleton instance used across the app. */
export const evictionManager = new OpfsEvictionManager();
