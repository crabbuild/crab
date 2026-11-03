/**
 * PendingReviewContext — manages local draft comments for a PR review.
 *
 * Comments accumulate in-memory and are persisted to localStorage so they
 * survive tab switches and navigation. The full review is submitted as a
 * single atomic operation via the forge backend.
 *
 * Usage:
 *   <PendingReviewProvider repoId={repo.repo_id} prNumber={prNumber}>
 *     …
 *   </PendingReviewProvider>
 *
 *   const { comments, count, addComment, submitReview } = usePendingReview();
 */

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";

import type { PendingComment, Verdict, ForgeInlineComment } from "./types";
import { rpc } from "../ipc";
import { publishToast } from "../lib/toasts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface PendingReviewState {
  comments: PendingComment[];
  count: number;
  loading: boolean;
}

export interface PendingReviewActions {
  addComment(comment: Omit<PendingComment, "id" | "createdAt" | "outdated">): void;
  updateComment(id: string, body: string): void;
  removeComment(id: string): void;
  clearAll(): void;
  submitReview(body: string, verdict: Verdict): Promise<void>;
  markOutdated(commentIds: string[]): void;
}

export type PendingReviewContextValue = PendingReviewState & PendingReviewActions;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function storageKey(repoId: string, prNumber: number): string {
  return `crab.pendingReview.${repoId}.${prNumber}`;
}

interface StoredPayload {
  version: number;
  comments: PendingComment[];
  lastUpdated: string;
}

function loadFromStorage(repoId: string, prNumber: number): PendingComment[] {
  try {
    const raw = localStorage.getItem(storageKey(repoId, prNumber));
    if (!raw) return [];
    const parsed: StoredPayload = JSON.parse(raw);
    if (parsed.version !== 1 || !Array.isArray(parsed.comments)) return [];
    return parsed.comments;
  } catch {
    return [];
  }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

const PendingReviewContext = createContext<PendingReviewContextValue | null>(null);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

interface PendingReviewProviderProps {
  repoId: string;
  prNumber: number;
  children: ReactNode;
}

export function PendingReviewProvider({ repoId, prNumber, children }: PendingReviewProviderProps) {
  const [comments, setComments] = useState<PendingComment[]>(() =>
    loadFromStorage(repoId, prNumber),
  );
  const [loading, setLoading] = useState(false);

  // Keep a ref to the latest comments for the debounced persist callback.
  const commentsRef = useRef(comments);
  commentsRef.current = comments;

  // Debounced localStorage persistence (implemented in task 2.3).
  const persistTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const schedulePersist = useCallback(() => {
    if (persistTimer.current) clearTimeout(persistTimer.current);
    persistTimer.current = setTimeout(() => {
      try {
        const payload: StoredPayload = {
          version: 1,
          comments: commentsRef.current,
          lastUpdated: new Date().toISOString(),
        };
        localStorage.setItem(storageKey(repoId, prNumber), JSON.stringify(payload));
      } catch (err: unknown) {
        // In-memory state is retained — the user can still submit their review.
        // Surface the error so they know drafts may not survive navigation.
        const isQuota =
          err instanceof DOMException &&
          (err.name === "QuotaExceededError" || err.code === 22);

        publishToast({
          kind: "error",
          title: "Failed to save pending review",
          body: isQuota
            ? "Local storage is full. Your comments are safe in memory but may be lost if you navigate away."
            : "Could not persist pending comments to local storage.",
        });
      }
    }, 300);
  }, [repoId, prNumber]);

  // Cleanup timer on unmount.
  useEffect(() => {
    return () => {
      if (persistTimer.current) clearTimeout(persistTimer.current);
    };
  }, []);

  // Re-load from storage if repoId/prNumber changes (provider re-used with different PR).
  useEffect(() => {
    setComments(loadFromStorage(repoId, prNumber));
  }, [repoId, prNumber]);

  // --- Actions (stubs for tasks 2.2–2.4, full interface shape exposed now) ---

  const addComment = useCallback(
    (comment: Omit<PendingComment, "id" | "createdAt" | "outdated">) => {
      // Full implementation in task 2.2.
      const newComment: PendingComment = {
        ...comment,
        id: crypto.randomUUID(),
        createdAt: new Date().toISOString(),
        outdated: false,
      };
      setComments((prev) => [...prev, newComment]);
      schedulePersist();
    },
    [schedulePersist],
  );

  const updateComment = useCallback(
    (id: string, body: string) => {
      // Full implementation in task 2.2.
      setComments((prev) => prev.map((c) => (c.id === id ? { ...c, body } : c)));
      schedulePersist();
    },
    [schedulePersist],
  );

  const removeComment = useCallback(
    (id: string) => {
      // Full implementation in task 2.2.
      setComments((prev) => prev.filter((c) => c.id !== id));
      schedulePersist();
    },
    [schedulePersist],
  );

  const clearAll = useCallback(() => {
    // Full implementation in task 2.2.
    setComments([]);
    schedulePersist();
  }, [schedulePersist]);

  const markOutdated = useCallback(
    (commentIds: string[]) => {
      // Full implementation in task 2.2.
      const idSet = new Set(commentIds);
      setComments((prev) =>
        prev.map((c) => (idSet.has(c.id) ? { ...c, outdated: true } : c)),
      );
      schedulePersist();
    },
    [schedulePersist],
  );

  const submitReview = useCallback(
    async (body: string, verdict: Verdict): Promise<void> => {
      // Full implementation in task 2.4.
      setLoading(true);
      try {
        const forgeComments: ForgeInlineComment[] = commentsRef.current.map((c) => ({
          path: c.path,
          line: c.endLine != null ? c.endLine : c.line,
          side: c.side,
          body: c.body,
          ...(c.endLine != null ? { start_line: c.line } : {}),
        }));

        await rpc.forgeSubmitReviewWithComments(repoId, prNumber, {
          body,
          event: verdict,
          comments: forgeComments,
        });

        // On success: clear all pending comments and remove localStorage key.
        setComments([]);
        try {
          localStorage.removeItem(storageKey(repoId, prNumber));
        } catch {
          // Non-critical — in-memory state is already cleared.
        }
      } finally {
        setLoading(false);
      }
    },
    [repoId, prNumber],
  );

  // --- Context value ---

  const value: PendingReviewContextValue = {
    comments,
    count: comments.length,
    loading,
    addComment,
    updateComment,
    removeComment,
    clearAll,
    submitReview,
    markOutdated,
  };

  return (
    <PendingReviewContext.Provider value={value}>{children}</PendingReviewContext.Provider>
  );
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

/**
 * Returns the pending review state and actions for the current PR.
 *
 * Must be called within a `<PendingReviewProvider>`.
 */
export function usePendingReview(): PendingReviewContextValue {
  const ctx = useContext(PendingReviewContext);
  if (!ctx) {
    throw new Error("usePendingReview must be used within a PendingReviewProvider");
  }
  return ctx;
}
