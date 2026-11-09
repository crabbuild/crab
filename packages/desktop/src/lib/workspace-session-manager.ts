// ── Types ───────────────────────────────────────────────────────────────

export interface WorkspaceSession {
  workspaceRoot: string;
  activeRepoRelativePath: string | null;
  tabBarOrder: string[];
  sidebarWidth: number;
  savedAt: number;
}

export interface IWorkspaceSessionManager {
  save(workspaceRoot: string, session: Partial<WorkspaceSession>): void;
  restore(workspaceRoot: string): WorkspaceSession | null;
  flush(): void;
  clear(): void;
}

// ── Storage key helpers ─────────────────────────────────────────────────

const WS_SESSION_KEY_PREFIX = "crab.wsSession.v1.";
const WS_SESSION_INDEX_KEY = "crab.wsSessionIndex.v1";
const MAX_WORKSPACE_SESSIONS = 10;
const FLUSH_DEBOUNCE_MS = 250;

// Legacy single-key constants (pre-migration format)
const LEGACY_WORKSPACE_ACTIVE_REPO_KEY = "crab.workspaceActiveRepo.v1";
const LEGACY_WORKSPACE_TAB_BAR_KEY = "crab.workspaceTabBar.v1";

interface WsSessionIndexEntry {
  key: string;
  workspaceRoot: string;
  savedAt: number;
}

/**
 * Synchronous hash for key derivation. Uses the same FNV-1a-inspired
 * dual-hash approach as session-manager.ts for consistency.
 */
function hashSync(input: string): string {
  let h = 0x811c9dc5;
  for (let i = 0; i < input.length; i++) {
    h ^= input.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  const hex = (h >>> 0).toString(16).padStart(8, "0");

  let h2 = 0x01000193;
  for (let i = 0; i < input.length; i++) {
    h2 ^= input.charCodeAt(i);
    h2 = Math.imul(h2, 0x811c9dc5);
  }
  const hex2 = (h2 >>> 0).toString(16).padStart(8, "0");
  return hex + hex2;
}

function deriveStorageKey(workspaceRoot: string): string {
  return WS_SESSION_KEY_PREFIX + hashSync(workspaceRoot);
}

// ── WorkspaceSessionManager ─────────────────────────────────────────────

export class WorkspaceSessionManager implements IWorkspaceSessionManager {
  private cache: Map<string, WorkspaceSession> = new Map();
  private dirty: Set<string> = new Set();
  private flushTimer: ReturnType<typeof setTimeout> | null = null;
  private keyCache: Map<string, string> = new Map();

  /** Resolve the localStorage key for a workspace root (cached). */
  private storageKey(workspaceRoot: string): string {
    let key = this.keyCache.get(workspaceRoot);
    if (!key) {
      key = deriveStorageKey(workspaceRoot);
      this.keyCache.set(workspaceRoot, key);
    }
    return key;
  }

  /**
   * Save (or update) the workspace session. Merges the provided partial
   * into the cached session, marks dirty, and schedules a debounced flush.
   */
  save(workspaceRoot: string, session: Partial<WorkspaceSession>): void {
    const existing = this.cache.get(workspaceRoot);
    const merged: WorkspaceSession = {
      workspaceRoot,
      activeRepoRelativePath: null,
      tabBarOrder: [],
      sidebarWidth: 260,
      ...existing,
      ...session,
      savedAt: Date.now(),
    };
    this.cache.set(workspaceRoot, merged);
    this.dirty.add(workspaceRoot);
    this.scheduleFlush();
  }

  /**
   * Restore the workspace session for a given workspace root.
   * Returns from in-memory cache first, falls back to localStorage.
   * On first call, attempts to migrate legacy single-key format if no
   * new-format session exists yet.
   * Returns null if no session exists or if the stored data is corrupt.
   */
  restore(workspaceRoot: string): WorkspaceSession | null {
    if (this.cache.has(workspaceRoot)) {
      return this.cache.get(workspaceRoot)!;
    }

    const existing = this.loadFromStorage(workspaceRoot);
    if (existing) return existing;

    // No new-format session exists — attempt migration from legacy keys.
    return this.migrateFromLegacyKeys(workspaceRoot);
  }

  /**
   * Write all dirty workspace sessions to localStorage and update the index.
   * Called on the debounce timer and synchronously on before-quit.
   */
  flush(): void {
    if (this.flushTimer != null) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }

    if (this.dirty.size === 0) return;

    const index = this.loadIndex();

    for (const workspaceRoot of this.dirty) {
      const session = this.cache.get(workspaceRoot);
      if (!session) continue;

      const key = this.storageKey(workspaceRoot);
      try {
        localStorage.setItem(key, JSON.stringify(session));
      } catch {
        // Quota exceeded — attempt eviction and retry once.
        this.evict(index);
        try {
          localStorage.setItem(key, JSON.stringify(session));
        } catch {
          console.warn("[WorkspaceSessionManager] localStorage write failed after eviction for:", workspaceRoot);
          continue;
        }
      }

      // Update index entry.
      const existingIdx = index.findIndex((e) => e.workspaceRoot === workspaceRoot);
      if (existingIdx >= 0) {
        index[existingIdx] = { key, workspaceRoot, savedAt: session.savedAt };
      } else {
        index.push({ key, workspaceRoot, savedAt: session.savedAt });
      }
    }

    this.dirty.clear();

    // Evict if over cap.
    if (index.length > MAX_WORKSPACE_SESSIONS) {
      this.evict(index);
    }

    this.saveIndex(index);
  }

  /**
   * Purge all workspace session keys from localStorage and reset in-memory state.
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
      localStorage.removeItem(WS_SESSION_INDEX_KEY);
    } catch {
      // best-effort
    }

    this.cache.clear();
    this.dirty.clear();
    this.keyCache.clear();
  }

  // ── Private helpers ─────────────────────────────────────────────────

  /**
   * Migrate from the legacy single-key format (`crab.workspaceActiveRepo.v1`
   * and `crab.workspaceTabBar.v1`) to the new per-workspace keyed format.
   *
   * Idempotent: only runs if the old keys exist AND no new-format session
   * has been written for this workspace root. Deletes old keys after
   * successful migration.
   */
  private migrateFromLegacyKeys(workspaceRoot: string): WorkspaceSession | null {
    try {
      const legacyActiveRepo = localStorage.getItem(LEGACY_WORKSPACE_ACTIVE_REPO_KEY);
      const legacyTabBar = localStorage.getItem(LEGACY_WORKSPACE_TAB_BAR_KEY);

      // Nothing to migrate if neither legacy key exists.
      if (!legacyActiveRepo && !legacyTabBar) return null;

      // Parse tab bar order from legacy key (JSON array of relative paths).
      let tabBarOrder: string[] = [];
      if (legacyTabBar) {
        try {
          const parsed = JSON.parse(legacyTabBar);
          if (Array.isArray(parsed)) {
            tabBarOrder = parsed.filter((item): item is string => typeof item === "string");
          }
        } catch {
          // Corrupted tab bar data — use empty array.
        }
      }

      // Build the new workspace session from legacy data.
      const session: WorkspaceSession = {
        workspaceRoot,
        activeRepoRelativePath: legacyActiveRepo ?? null,
        tabBarOrder,
        sidebarWidth: 260,
        savedAt: Date.now(),
      };

      // Persist the migrated session immediately.
      this.cache.set(workspaceRoot, session);
      this.dirty.add(workspaceRoot);
      this.flush();

      // Remove legacy keys now that migration is complete.
      try {
        localStorage.removeItem(LEGACY_WORKSPACE_ACTIVE_REPO_KEY);
      } catch {
        // best-effort
      }
      try {
        localStorage.removeItem(LEGACY_WORKSPACE_TAB_BAR_KEY);
      } catch {
        // best-effort
      }

      return session;
    } catch {
      // Migration failed — not critical, return null and let the app start fresh.
      return null;
    }
  }

  private scheduleFlush(): void {
    if (this.flushTimer != null) return;
    this.flushTimer = setTimeout(() => {
      this.flushTimer = null;
      this.flush();
    }, FLUSH_DEBOUNCE_MS);
  }

  private loadFromStorage(workspaceRoot: string): WorkspaceSession | null {
    const key = this.storageKey(workspaceRoot);
    try {
      const raw = localStorage.getItem(key);
      if (!raw) return null;
      const parsed = JSON.parse(raw) as WorkspaceSession;
      // Basic validation: ensure it has the expected shape.
      if (
        !parsed ||
        typeof parsed.workspaceRoot !== "string" ||
        typeof parsed.savedAt !== "number" ||
        !Array.isArray(parsed.tabBarOrder)
      ) {
        console.warn("[WorkspaceSessionManager] Corrupted session data for:", workspaceRoot);
        localStorage.removeItem(key);
        return null;
      }
      this.cache.set(workspaceRoot, parsed);
      return parsed;
    } catch {
      console.warn("[WorkspaceSessionManager] Failed to parse session for:", workspaceRoot);
      try {
        localStorage.removeItem(this.storageKey(workspaceRoot));
      } catch {
        // best-effort cleanup
      }
      return null;
    }
  }

  private loadIndex(): WsSessionIndexEntry[] {
    try {
      const raw = localStorage.getItem(WS_SESSION_INDEX_KEY);
      if (!raw) return [];
      const parsed = JSON.parse(raw);
      if (!Array.isArray(parsed)) return [];
      return parsed as WsSessionIndexEntry[];
    } catch {
      return [];
    }
  }

  private saveIndex(index: WsSessionIndexEntry[]): void {
    try {
      localStorage.setItem(WS_SESSION_INDEX_KEY, JSON.stringify(index));
    } catch {
      console.warn("[WorkspaceSessionManager] Failed to save session index");
    }
  }

  /**
   * LRU eviction: sort by savedAt ascending, remove oldest entries
   * until we're at or below MAX_WORKSPACE_SESSIONS.
   */
  private evict(index: WsSessionIndexEntry[]): void {
    index.sort((a, b) => a.savedAt - b.savedAt);

    while (index.length > MAX_WORKSPACE_SESSIONS) {
      const oldest = index.shift();
      if (!oldest) break;
      try {
        localStorage.removeItem(oldest.key);
      } catch {
        // best-effort
      }
      this.cache.delete(oldest.workspaceRoot);
      this.keyCache.delete(oldest.workspaceRoot);
    }

    this.saveIndex(index);
  }
}
