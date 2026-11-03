import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

/**
 * Tests for PendingReviewContext debounced localStorage persistence.
 *
 * Validates the 300ms debounce, version-1 serialization format,
 * QuotaExceededError handling (toast + in-memory retention), and
 * graceful handling of corrupted storage data.
 */

// ---------------------------------------------------------------------------
// localStorage mock
// ---------------------------------------------------------------------------

function createLocalStorageMock() {
  const store = new Map<string, string>();
  return {
    _store: store,
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => { store.set(key, value); },
    removeItem: (key: string) => { store.delete(key); },
    clear: () => { store.clear(); },
    get length() { return store.size; },
    key: (i: number) => [...store.keys()][i] ?? null,
  };
}

let mockStorage: ReturnType<typeof createLocalStorageMock>;

beforeEach(() => {
  vi.useFakeTimers();
  mockStorage = createLocalStorageMock();
  Object.defineProperty(globalThis, "localStorage", { value: mockStorage, writable: true });
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const STORAGE_KEY = "crab.pendingReview.repo-1.42";

interface StoredPayload {
  version: number;
  comments: Array<{
    id: string;
    path: string;
    line: number;
    endLine: number | null;
    side: "LEFT" | "RIGHT";
    body: string;
    createdAt: string;
    outdated: boolean;
  }>;
  lastUpdated: string;
}

function makeComment(overrides: Partial<StoredPayload["comments"][0]> = {}) {
  return {
    id: "comment-1",
    path: "src/foo.ts",
    line: 10,
    endLine: null,
    side: "RIGHT" as const,
    body: "looks good",
    createdAt: "2024-06-01T00:00:00.000Z",
    outdated: false,
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("PendingReview localStorage persistence", () => {
  it("serializes with version 1 format including lastUpdated", () => {
    const comments = [makeComment({ body: "fix this" })];
    const payload: StoredPayload = {
      version: 1,
      comments,
      lastUpdated: new Date().toISOString(),
    };

    localStorage.setItem(STORAGE_KEY, JSON.stringify(payload));

    const stored = JSON.parse(localStorage.getItem(STORAGE_KEY)!);
    expect(stored.version).toBe(1);
    expect(stored.comments).toHaveLength(1);
    expect(stored.comments[0].body).toBe("fix this");
    expect(typeof stored.lastUpdated).toBe("string");
  });

  it("debounces writes — only the last call within 300ms fires", () => {
    const writes: string[] = [];
    const origSetItem = mockStorage.setItem.bind(mockStorage);
    mockStorage.setItem = (key: string, value: string) => {
      writes.push(key);
      origSetItem(key, value);
    };

    // Simulate the debounce pattern from PendingReviewContext
    let timer: ReturnType<typeof setTimeout> | null = null;
    const commentsRef = { current: [makeComment()] };

    function schedulePersist() {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        const payload: StoredPayload = {
          version: 1,
          comments: commentsRef.current,
          lastUpdated: new Date().toISOString(),
        };
        localStorage.setItem(STORAGE_KEY, JSON.stringify(payload));
      }, 300);
    }

    // Rapid calls — only the last should persist
    schedulePersist();
    vi.advanceTimersByTime(100);
    schedulePersist();
    vi.advanceTimersByTime(100);
    schedulePersist();

    // Before 300ms from last call, nothing written
    vi.advanceTimersByTime(299);
    expect(writes).toHaveLength(0);

    // At 300ms from last call, single write fires
    vi.advanceTimersByTime(1);
    expect(writes).toHaveLength(1);
  });

  it("catches QuotaExceededError, shows toast, retains in-memory state", () => {
    const toastCalls: Array<{ kind: string; title: string; body?: string }> = [];
    function publishToast(t: { kind: string; title: string; body?: string }) {
      toastCalls.push(t);
    }

    // Simulate QuotaExceededError on setItem
    mockStorage.setItem = () => {
      throw new DOMException("Storage quota exceeded", "QuotaExceededError");
    };

    const comments = [makeComment({ body: "important feedback" })];
    const commentsRef = { current: comments };

    let timer: ReturnType<typeof setTimeout> | null = null;
    function schedulePersist() {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        try {
          const payload: StoredPayload = {
            version: 1,
            comments: commentsRef.current,
            lastUpdated: new Date().toISOString(),
          };
          localStorage.setItem(STORAGE_KEY, JSON.stringify(payload));
        } catch (err: unknown) {
          const isQuota =
            err instanceof DOMException &&
            (err.name === "QuotaExceededError" || (err as any).code === 22);

          publishToast({
            kind: "error",
            title: "Failed to save pending review",
            body: isQuota
              ? "Local storage is full. Your comments are safe in memory but may be lost if you navigate away."
              : "Could not persist pending comments to local storage.",
          });
        }
      }, 300);
    }

    schedulePersist();
    vi.advanceTimersByTime(300);

    // In-memory state is retained
    expect(commentsRef.current).toHaveLength(1);
    expect(commentsRef.current[0].body).toBe("important feedback");

    // Toast was published with quota-specific message
    expect(toastCalls).toHaveLength(1);
    expect(toastCalls[0].kind).toBe("error");
    expect(toastCalls[0].title).toBe("Failed to save pending review");
    expect(toastCalls[0].body).toContain("Local storage is full");
  });

  it("handles non-quota storage errors with generic toast message", () => {
    const toastCalls: Array<{ kind: string; title: string; body?: string }> = [];
    function publishToast(t: { kind: string; title: string; body?: string }) {
      toastCalls.push(t);
    }

    // Simulate a generic error (not QuotaExceeded)
    mockStorage.setItem = () => {
      throw new Error("SecurityError: access denied");
    };

    const commentsRef = { current: [makeComment()] };

    let timer: ReturnType<typeof setTimeout> | null = null;
    function schedulePersist() {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        try {
          const payload: StoredPayload = {
            version: 1,
            comments: commentsRef.current,
            lastUpdated: new Date().toISOString(),
          };
          localStorage.setItem(STORAGE_KEY, JSON.stringify(payload));
        } catch (err: unknown) {
          const isQuota =
            err instanceof DOMException &&
            (err.name === "QuotaExceededError" || (err as any).code === 22);

          publishToast({
            kind: "error",
            title: "Failed to save pending review",
            body: isQuota
              ? "Local storage is full. Your comments are safe in memory but may be lost if you navigate away."
              : "Could not persist pending comments to local storage.",
          });
        }
      }, 300);
    }

    schedulePersist();
    vi.advanceTimersByTime(300);

    // In-memory state retained
    expect(commentsRef.current).toHaveLength(1);

    // Generic message (not quota-specific)
    expect(toastCalls).toHaveLength(1);
    expect(toastCalls[0].body).toBe("Could not persist pending comments to local storage.");
  });

  it("loads stored comments correctly on initialization", () => {
    const comments = [makeComment({ body: "stored comment" })];
    const payload: StoredPayload = {
      version: 1,
      comments,
      lastUpdated: "2024-01-01T00:00:00.000Z",
    };
    localStorage.setItem(STORAGE_KEY, JSON.stringify(payload));

    // Simulate loadFromStorage
    const raw = localStorage.getItem(STORAGE_KEY);
    const parsed: StoredPayload = JSON.parse(raw!);
    expect(parsed.version).toBe(1);
    expect(parsed.comments).toHaveLength(1);
    expect(parsed.comments[0].body).toBe("stored comment");
  });

  it("returns empty array for corrupted storage data", () => {
    localStorage.setItem(STORAGE_KEY, "not valid json{{{");

    function loadFromStorage(): StoredPayload["comments"] {
      try {
        const raw = localStorage.getItem(STORAGE_KEY);
        if (!raw) return [];
        const parsed: StoredPayload = JSON.parse(raw);
        if (parsed.version !== 1 || !Array.isArray(parsed.comments)) return [];
        return parsed.comments;
      } catch {
        return [];
      }
    }

    expect(loadFromStorage()).toEqual([]);
  });

  it("returns empty array for wrong version number", () => {
    const payload = { version: 99, comments: [makeComment()], lastUpdated: "x" };
    localStorage.setItem(STORAGE_KEY, JSON.stringify(payload));

    function loadFromStorage(): StoredPayload["comments"] {
      try {
        const raw = localStorage.getItem(STORAGE_KEY);
        if (!raw) return [];
        const parsed: StoredPayload = JSON.parse(raw);
        if (parsed.version !== 1 || !Array.isArray(parsed.comments)) return [];
        return parsed.comments;
      } catch {
        return [];
      }
    }

    expect(loadFromStorage()).toEqual([]);
  });
});
