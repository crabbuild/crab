import type { NavEntry } from "../hooks/useNavHistory";

// ── Types ───────────────────────────────────────────────────────────────

export interface RepoSession {
  repoPath: string;
  activeView: string;
  activeFile?: string;
  navHistory: NavEntry[];
  navIndex: number;
  scrollPositions: Record<string, number>;
  viewState: Record<string, unknown>;
  savedAt: number;
}

export interface ISessionManager {
  deactivate(repoPath: string, currentState: Partial<RepoSession>): void;
  activate(repoPath: string): RepoSession | null;
  update(repoPath: string, partial: Partial<RepoSession>): void;
  getScrollPosition(repoPath: string, viewId: string): number | null;
  updateScrollPosition(repoPath: string, viewId: string, scrollTop: number): void;
  flush(): void;
  clear(): void;
  cleanupStaleWorktrees(knownRepoPaths: string[]): void;
  /** Enable or disable session persistence. When disabled, deactivate is a no-op and activate returns null. */
  setEnabled(enabled: boolean): void;
}

// ── Storage key helpers ─────────────────────────────────────────────────

const SESSION_KEY_PREFIX = "crab.session.v1.";
const SESSION_INDEX_KEY = "crab.sessionIndex.v1";
const SESSION_CLEANUP_QUEUE_KEY = "crab.sessionCleanupQueue.v1";
const MAX_SESSIONS = 50;
const FLUSH_DEBOUNCE_MS = 250;
const CLEANUP_GRACE_PERIOD_MS = 24 * 60 * 60 * 1000; // 24 hours

interface SessionIndexEntry {
  key: string;
  repoPath: string;
  savedAt: number;
}

interface CleanupQueueEntry {
  repoPath: string;
  key: string;
  queuedAt: number;
}

/**
 * Derive a stable localStorage key from a repo path using SHA-256.
 * Uses SubtleCrypto when available with a sync fallback (simple hash)
 * for environments where SubtleCrypto is unavailable or for the
 * synchronous code path.
 */
export function hashSync(input: string): string {
  // Simple FNV-1a-inspired hash as a sync fallback.
  // We use this because SubtleCrypto.digest is async and we need
  // synchronous key derivation for localStorage reads in state initializers.
  let h = 0x811c9dc5;
  for (let i = 0; i < input.length; i++) {
    h ^= input.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  // Convert to unsigned 32-bit then hex, pad to 8 chars, repeat to get 16 chars.
  const hex = (h >>> 0).toString(16).padStart(8, "0");
  // Generate a second hash with a different seed for more uniqueness.
  let h2 = 0x01000193;
  for (let i = 0; i < input.length; i++) {
    h2 ^= input.charCodeAt(i);
    h2 = Math.imul(h2, 0x811c9dc5);
  }
  const hex2 = (h2 >>> 0).toString(16).padStart(8, "0");
  return hex + hex2;
}

/**
 * Async SHA-256 hash (used for pre-computing keys when possible).
 * Falls back to the sync hash if SubtleCrypto is unavailable.
 */
export async function sha256Hash(input: string): Promise<string> {
  try {
    const encoder = new TextEncoder();
    const data = encoder.encode(input);
    const buffer = await crypto.subtle.digest("SHA-256", data);
    const hashArray = Array.from(new Uint8Array(buffer));
    return hashArray.map((b) => b.toString(16).padStart(2, "0")).join("").slice(0, 16);
  } catch {
    return hashSync(input);
  }
}

/**
 * Synchronous key derivation. Uses the sync fallback hash.
 * The design calls for sha256, but since SubtleCrypto is async-only
 * and we need synchronous reads in React state initializers, we use
 * a deterministic sync hash that provides sufficient uniqueness for
 * localStorage key derivation (collision probability is negligible
 * for typical repo path counts < 1000).
 */
function deriveStorageKey(repoPath: string): string {
  return SESSION_KEY_PREFIX + hashSync(repoPath);
}

// ── SessionManager ──────────────────────────────────────────────────────

export class SessionManager implements ISessionManager {
  private cache: Map<string, RepoSession> = new Map();
  private dirty: Set<string> = new Set();
  private flushTimer: ReturnType<typeof setTimeout> | null = null;
  private keyCache: Map<string, string> = new Map();
  private enabled = true;

  /** Enable or disable session persistence. When disabled, deactivate is a no-op and activate returns null. */
  setEnabled(enabled: boolean): void {
    this.enabled = enabled;
  }

  /** Resolve the localStorage key for a repo path (cached). */
  private storageKey(repoPath: string): string {
    let key = this.keyCache.get(repoPath);
    if (!key) {
      key = deriveStorageKey(repoPath);
      this.keyCache.set(repoPath, key);
    }
    return key;
  }

  /**
   * Snapshot the current repo's session before switching away.
   * Merges the provided partial state into the cached session,
   * marks it dirty, and schedules a debounced flush.
   * When persistence is disabled, this is a no-op.
   */
  deactivate(repoPath: string, currentState: Partial<RepoSession>): void {
    if (!this.enabled) return;

    const existing = this.cache.get(repoPath);
    const session: RepoSession = {
      repoPath,
      activeView: "explorer",
      navHistory: [],
      navIndex: 0,
      scrollPositions: {},
      viewState: {},
      ...existing,
      ...currentState,
      savedAt: Date.now(),
    };
    this.cache.set(repoPath, session);
    this.dirty.add(repoPath);
    this.scheduleFlush();
  }

  /**
   * Load and return the session for a repo being activated.
   * Returns from in-memory cache first, falls back to localStorage.
   * When persistence is disabled, returns null.
   */
  activate(repoPath: string): RepoSession | null {
    if (!this.enabled) return null;

    if (this.cache.has(repoPath)) {
      return this.cache.get(repoPath)!;
    }
    return this.loadFromStorage(repoPath);
  }

  /**
   * Incremental update for scroll/view changes on the active repo.
   * Merges the partial into the cached session and schedules flush.
   */
  update(repoPath: string, partial: Partial<RepoSession>): void {
    const existing = this.cache.get(repoPath);
    if (!existing) return;
    this.cache.set(repoPath, { ...existing, ...partial, savedAt: Date.now() });
    this.dirty.add(repoPath);
    this.scheduleFlush();
  }

  /** Get a saved scroll position for a specific view within a repo. */
  getScrollPosition(repoPath: string, viewId: string): number | null {
    const session = this.cache.get(repoPath);
    if (!session) return null;
    const pos = session.scrollPositions[viewId];
    return pos != null ? pos : null;
  }

  /** Update the scroll position for a specific view within a repo. */
  updateScrollPosition(repoPath: string, viewId: string, scrollTop: number): void {
    const session = this.cache.get(repoPath);
    if (!session) return;
    session.scrollPositions = { ...session.scrollPositions, [viewId]: scrollTop };
    session.savedAt = Date.now();
    this.dirty.add(repoPath);
    this.scheduleFlush();
  }

  /**
   * Write all dirty sessions to localStorage and update the session index.
   * Called on the debounce timer and synchronously on before-quit.
   */
  flush(): void {
    if (this.flushTimer != null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }

    if (this.dirty.size === 0) return;

    const index = this.loadIndex();

    for (const repoPath of this.dirty) {
      const session = this.cache.get(repoPath);
      if (!session) continue;

      const key = this.storageKey(repoPath);
      try {
        localStorage.setItem(key, JSON.stringify(session));
      } catch {
        // Quota exceeded — attempt eviction and retry once.
        this.evict(index);
        try {
          localStorage.setItem(key, JSON.stringify(session));
        } catch {
          console.warn("[SessionManager] localStorage write failed after eviction for:", repoPath);
          continue;
        }
      }

      // Update index entry.
      const existingIdx = index.findIndex((e) => e.repoPath === repoPath);
      if (existingIdx >= 0) {
        index[existingIdx] = { key, repoPath, savedAt: session.savedAt };
      } else {
        index.push({ key, repoPath, savedAt: session.savedAt });
      }
    }

    this.dirty.clear();

    // Evict if over cap.
    if (index.length > MAX_SESSIONS) {
      this.evict(index);
    }

    this.saveIndex(index);
  }

  /**
   * Purge all session keys from localStorage and reset in-memory state.
   */
  clear(): void {
    if (this.flushTimer != null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }

    const index = this.loadIndex();
    for (const entry of index) {
      try {
        localStorage.removeItem(entry.key);
      } catch {
        // best-effort
      }
    }

    try {
      localStorage.removeItem(SESSION_INDEX_KEY);
    } catch {
      // best-effort
    }

    this.cache.clear();
    this.dirty.clear();
    this.keyCache.clear();
  }

  /**
   * Clean up sessions for worktrees (or repos) that no longer exist.
   *
   * On app start, reads the session index and cross-references against
   * the set of known repo paths (currently open repos from the store).
   * Any session whose repoPath is not in the known set is added to a
   * cleanup queue. Queue entries older than 24 hours are purged — this
   * grace period gives the user time to recreate a deleted worktree
   * before its session data is lost.
   */
  cleanupStaleWorktrees(knownRepoPaths: string[]): void {
    const now = Date.now();
    const knownSet = new Set(knownRepoPaths);
    const index = this.loadIndex();
    const queue = this.loadCleanupQueue();

    // Phase 1: Purge queue entries older than 24 hours.
    const expiredEntries: CleanupQueueEntry[] = [];
    const survivingQueue: CleanupQueueEntry[] = [];

    for (const entry of queue) {
      if (now - entry.queuedAt >= CLEANUP_GRACE_PERIOD_MS) {
        expiredEntries.push(entry);
      } else {
        survivingQueue.push(entry);
      }
    }

    // Remove expired entries from localStorage and the session index.
    for (const expired of expiredEntries) {
      try {
        localStorage.removeItem(expired.key);
      } catch {
        // best-effort
      }
      this.cache.delete(expired.repoPath);
      this.keyCache.delete(expired.repoPath);

      const idx = index.findIndex((e) => e.repoPath === expired.repoPath);
      if (idx >= 0) {
        index.splice(idx, 1);
      }
    }

    // Phase 2: Add new stale entries to the queue.
    // A session is stale if its repoPath is not in the known set AND
    // it's not already queued.
    const queuedPaths = new Set(survivingQueue.map((e) => e.repoPath));

    for (const entry of index) {
      if (!knownSet.has(entry.repoPath) && !queuedPaths.has(entry.repoPath)) {
        survivingQueue.push({
          repoPath: entry.repoPath,
          key: entry.key,
          queuedAt: now,
        });
      }
    }

    // Phase 3: Remove entries from the queue if they reappeared in the
    // known set (user re-opened the worktree before the grace period expired).
    const finalQueue = survivingQueue.filter((e) => !knownSet.has(e.repoPath));

    // Persist updated state.
    if (expiredEntries.length > 0) {
      this.saveIndex(index);
    }
    this.saveCleanupQueue(finalQueue);

    if (expiredEntries.length > 0) {
      console.info(
        `[SessionManager] Purged ${expiredEntries.length} stale session(s) after 24h grace period.`,
      );
    }
  }

  // ── Private helpers ─────────────────────────────────────────────────

  private scheduleFlush(): void {
    if (this.flushTimer != null) return;
    this.flushTimer = setTimeout(() => {
      this.flushTimer = null;
      this.flush();
    }, FLUSH_DEBOUNCE_MS);
  }

  private loadFromStorage(repoPath: string): RepoSession | null {
    const key = this.storageKey(repoPath);
    try {
      const raw = localStorage.getItem(key);
      if (!raw) return null;
      const parsed = JSON.parse(raw) as RepoSession;
      // Basic validation: ensure it has the expected shape.
      if (!parsed || typeof parsed.repoPath !== "string" || typeof parsed.savedAt !== "number") {
        console.warn("[SessionManager] Corrupted session data for:", repoPath);
        localStorage.removeItem(key);
        return null;
      }
      this.cache.set(repoPath, parsed);
      return parsed;
    } catch {
      console.warn("[SessionManager] Failed to parse session for:", repoPath);
      try {
        localStorage.removeItem(this.storageKey(repoPath));
      } catch {
        // best-effort cleanup
      }
      return null;
    }
  }

  private loadIndex(): SessionIndexEntry[] {
    try {
      const raw = localStorage.getItem(SESSION_INDEX_KEY);
      if (!raw) return [];
      const parsed = JSON.parse(raw);
      if (!Array.isArray(parsed)) return [];
      return parsed as SessionIndexEntry[];
    } catch {
      return [];
    }
  }

  private saveIndex(index: SessionIndexEntry[]): void {
    try {
      localStorage.setItem(SESSION_INDEX_KEY, JSON.stringify(index));
    } catch {
      console.warn("[SessionManager] Failed to save session index");
    }
  }

  private loadCleanupQueue(): CleanupQueueEntry[] {
    try {
      const raw = localStorage.getItem(SESSION_CLEANUP_QUEUE_KEY);
      if (!raw) return [];
      const parsed = JSON.parse(raw);
      if (!Array.isArray(parsed)) return [];
      return parsed as CleanupQueueEntry[];
    } catch {
      return [];
    }
  }

  private saveCleanupQueue(queue: CleanupQueueEntry[]): void {
    try {
      if (queue.length === 0) {
        localStorage.removeItem(SESSION_CLEANUP_QUEUE_KEY);
      } else {
        localStorage.setItem(SESSION_CLEANUP_QUEUE_KEY, JSON.stringify(queue));
      }
    } catch {
      console.warn("[SessionManager] Failed to save cleanup queue");
    }
  }

  /**
   * LRU eviction: sort by savedAt ascending, remove oldest entries
   * until we're at or below MAX_SESSIONS.
   */
  private evict(index: SessionIndexEntry[]): void {
    // Sort oldest first.
    index.sort((a, b) => a.savedAt - b.savedAt);

    while (index.length > MAX_SESSIONS) {
      const oldest = index.shift();
      if (!oldest) break;
      try {
        localStorage.removeItem(oldest.key);
      } catch {
        // best-effort
      }
      this.cache.delete(oldest.repoPath);
      this.keyCache.delete(oldest.repoPath);
    }

    this.saveIndex(index);
  }
}
