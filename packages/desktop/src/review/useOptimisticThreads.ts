import { useCallback, useRef, useState } from "react";
import type { ReviewThread, ThreadComment } from "./types";
import { rpc } from "../ipc";

// ── Types ────────────────────────────────────────────────────────────

/** A thread comment augmented with optimistic metadata. */
export interface OptimisticThreadComment extends ThreadComment {
  /** When true, the reply failed to publish and should show a failure indicator. */
  failed?: boolean;
  /** Error message from the failed RPC call. */
  failureMessage?: string;
  /** Whether this is a temporary optimistic comment not yet confirmed by the server. */
  pending?: boolean;
}

/** A thread with comments that may include optimistic entries. */
export interface OptimisticReviewThread extends Omit<ReviewThread, "comments"> {
  comments: OptimisticThreadComment[];
  /** Inline error message from a failed resolve/unresolve operation. */
  resolveError?: string;
}

/** Tracks an in-flight optimistic resolve/unresolve override for a thread. */
interface OptimisticResolveEntry {
  /** The optimistic resolved value applied to the UI. */
  resolved: boolean;
  /** Error message if the RPC failed (triggers revert + inline error display). */
  error?: string;
}

/** Tracks an optimistic edit applied to a comment. */
interface OptimisticEditEntry {
  /** The new body after editing. */
  body: string;
  /** Error message if the RPC failed (triggers revert + inline error display). */
  error?: string;
}

interface OptimisticReplyState {
  /** Map of threadId → list of optimistic comments (pending or failed). */
  optimisticComments: Map<string, OptimisticThreadComment[]>;
  /** Map of threadId → optimistic resolve state override. */
  optimisticResolves: Map<string, OptimisticResolveEntry>;
  /** Map of commentId → optimistic edit state. */
  optimisticEdits: Map<string, OptimisticEditEntry>;
  /** Set of commentIds that have been optimistically deleted. */
  optimisticDeletes: Set<string>;
}

interface UseOptimisticThreadsResult {
  /** Threads augmented with any optimistic comments appended. */
  threads: OptimisticReviewThread[];
  /** Submit a reply optimistically. Returns immediately after appending the temp comment. */
  replyToThread(threadId: string, body: string): void;
  /** Retry a previously failed reply. */
  retryReply(threadId: string, tempId: string): void;
  /** Dismiss a failed reply (remove it from the optimistic state). */
  dismissFailedReply(threadId: string, tempId: string): void;
  /** Resolve or unresolve a thread optimistically. */
  resolveThread(threadId: string, resolved: boolean): void;
  /** Dismiss the inline error shown after a failed resolve/unresolve. */
  dismissResolveError(threadId: string): void;
  /** Edit a comment. Returns a promise that rejects on failure (caller handles UI revert). */
  editComment(commentId: string, body: string): Promise<void>;
  /** Delete a comment. Returns a promise that rejects on failure (caller handles UI revert). */
  deleteComment(commentId: string): Promise<void>;
}

// ── Hook ─────────────────────────────────────────────────────────────

/**
 * Wraps thread data with optimistic reply management.
 *
 * On reply submit, a temp comment is immediately appended to the thread.
 * The RPC call runs in the background. On success the temp is replaced with
 * confirmed data; on failure the temp is marked with a failure indicator.
 */
export function useOptimisticThreads(
  threads: ReviewThread[],
  repoId: string,
  prNumber: number,
  currentUser: string,
): UseOptimisticThreadsResult {
  const [state, setState] = useState<OptimisticReplyState>({
    optimisticComments: new Map(),
    optimisticResolves: new Map(),
    optimisticEdits: new Map(),
    optimisticDeletes: new Set(),
  });

  // Keep a ref to the latest threads so the async callback always sees fresh data.
  const threadsRef = useRef(threads);
  threadsRef.current = threads;

  const replyToThread = useCallback(
    (threadId: string, body: string) => {
      const tempId = `temp-${crypto.randomUUID()}`;
      const tempComment: OptimisticThreadComment = {
        id: tempId,
        author: currentUser,
        body,
        createdAt: new Date().toISOString(),
        edited: false,
        isMine: true,
        pending: true,
      };

      // Immediately append the temp comment.
      setState((prev) => {
        const next = new Map(prev.optimisticComments);
        const existing = next.get(threadId) ?? [];
        next.set(threadId, [...existing, tempComment]);
        return { ...prev, optimisticComments: next };
      });

      // Fire the RPC in the background.
      rpc
        .forgePrThreadReply(repoId, prNumber, threadId, body)
        .then((result) => {
          // On success: replace the temp comment with confirmed data.
          setState((prev) => {
            const next = new Map(prev.optimisticComments);
            const list = next.get(threadId) ?? [];
            const updated = list.map((c) =>
              c.id === tempId
                ? {
                    ...c,
                    id: result.id,
                    createdAt: result.created_at,
                    pending: false,
                    failed: false,
                    failureMessage: undefined,
                  }
                : c,
            );
            next.set(threadId, updated);
            return { ...prev, optimisticComments: next };
          });
        })
        .catch((err: unknown) => {
          // On failure: mark the temp comment as failed.
          const message =
            err instanceof Error ? err.message : "Failed to post reply";
          setState((prev) => {
            const next = new Map(prev.optimisticComments);
            const list = next.get(threadId) ?? [];
            const updated = list.map((c) =>
              c.id === tempId
                ? { ...c, pending: false, failed: true, failureMessage: message }
                : c,
            );
            next.set(threadId, updated);
            return { ...prev, optimisticComments: next };
          });
        });
    },
    [repoId, prNumber, currentUser],
  );

  const retryReply = useCallback(
    (threadId: string, tempId: string) => {
      // Find the failed comment to get its body.
      const list = state.optimisticComments.get(threadId) ?? [];
      const failedComment = list.find((c) => c.id === tempId);
      if (!failedComment) return;

      const body = failedComment.body;

      // Mark as pending again.
      setState((prev) => {
        const next = new Map(prev.optimisticComments);
        const comments = next.get(threadId) ?? [];
        const updated = comments.map((c) =>
          c.id === tempId
            ? { ...c, pending: true, failed: false, failureMessage: undefined }
            : c,
        );
        next.set(threadId, updated);
        return { ...prev, optimisticComments: next };
      });

      // Retry the RPC.
      rpc
        .forgePrThreadReply(repoId, prNumber, threadId, body)
        .then((result) => {
          setState((prev) => {
            const next = new Map(prev.optimisticComments);
            const comments = next.get(threadId) ?? [];
            const updated = comments.map((c) =>
              c.id === tempId
                ? {
                    ...c,
                    id: result.id,
                    createdAt: result.created_at,
                    pending: false,
                    failed: false,
                    failureMessage: undefined,
                  }
                : c,
            );
            next.set(threadId, updated);
            return { ...prev, optimisticComments: next };
          });
        })
        .catch((err: unknown) => {
          const message =
            err instanceof Error ? err.message : "Failed to post reply";
          setState((prev) => {
            const next = new Map(prev.optimisticComments);
            const comments = next.get(threadId) ?? [];
            const updated = comments.map((c) =>
              c.id === tempId
                ? { ...c, pending: false, failed: true, failureMessage: message }
                : c,
            );
            next.set(threadId, updated);
            return { ...prev, optimisticComments: next };
          });
        });
    },
    [repoId, prNumber, state.optimisticComments],
  );

  const dismissFailedReply = useCallback(
    (threadId: string, tempId: string) => {
      setState((prev) => {
        const next = new Map(prev.optimisticComments);
        const list = next.get(threadId) ?? [];
        const filtered = list.filter((c) => c.id !== tempId);
        if (filtered.length === 0) {
          next.delete(threadId);
        } else {
          next.set(threadId, filtered);
        }
        return { ...prev, optimisticComments: next };
      });
    },
    [],
  );

  const resolveThread = useCallback(
    (threadId: string, resolved: boolean) => {
      // Optimistically apply the new resolved state.
      setState((prev) => {
        const next = new Map(prev.optimisticResolves);
        next.set(threadId, { resolved });
        return { ...prev, optimisticResolves: next };
      });

      // Fire the RPC in the background.
      rpc
        .forgePrThreadResolve(repoId, prNumber, threadId, resolved)
        .then(() => {
          // On success: clear the optimistic override (the next data refresh
          // will carry the confirmed state from the server).
          setState((prev) => {
            const next = new Map(prev.optimisticResolves);
            next.delete(threadId);
            return { ...prev, optimisticResolves: next };
          });
        })
        .catch((err: unknown) => {
          // On failure: revert by removing the override and attach an error message.
          const message =
            err instanceof Error ? err.message : "Failed to update thread status";
          setState((prev) => {
            const next = new Map(prev.optimisticResolves);
            // Revert: store the *original* resolved state (opposite of what we tried)
            // with an error so the UI shows the error inline.
            next.set(threadId, { resolved: !resolved, error: message });
            return { ...prev, optimisticResolves: next };
          });
        });
    },
    [repoId, prNumber],
  );

  const dismissResolveError = useCallback((threadId: string) => {
    setState((prev) => {
      const next = new Map(prev.optimisticResolves);
      next.delete(threadId);
      return { ...prev, optimisticResolves: next };
    });
  }, []);

  const editComment = useCallback(
    async (commentId: string, body: string): Promise<void> => {
      // Optimistically apply the edit.
      setState((prev) => {
        const next = new Map(prev.optimisticEdits);
        next.set(commentId, { body });
        return { ...prev, optimisticEdits: next };
      });

      try {
        await rpc.forgePrCommentEdit(repoId, commentId, body);
        // On success: clear the optimistic edit (the next data refresh will
        // carry the confirmed state with edited=true from the server).
        setState((prev) => {
          const next = new Map(prev.optimisticEdits);
          next.delete(commentId);
          return { ...prev, optimisticEdits: next };
        });
      } catch (err: unknown) {
        // On failure: revert the optimistic edit and re-throw so the UI stays in edit mode.
        setState((prev) => {
          const next = new Map(prev.optimisticEdits);
          next.delete(commentId);
          return { ...prev, optimisticEdits: next };
        });
        throw err instanceof Error ? err : new Error("Failed to edit comment");
      }
    },
    [repoId],
  );

  const deleteComment = useCallback(
    async (commentId: string): Promise<void> => {
      // Optimistically remove the comment from the UI.
      setState((prev) => {
        const next = new Set(prev.optimisticDeletes);
        next.add(commentId);
        return { ...prev, optimisticDeletes: next };
      });

      try {
        await rpc.forgePrCommentDelete(repoId, commentId);
        // On success: keep the delete in optimistic state until next data refresh
        // clears it naturally (the comment won't appear in fresh thread data).
      } catch (err: unknown) {
        // On failure: revert the optimistic delete and re-throw.
        setState((prev) => {
          const next = new Set(prev.optimisticDeletes);
          next.delete(commentId);
          return { ...prev, optimisticDeletes: next };
        });
        throw err instanceof Error ? err : new Error("Failed to delete comment");
      }
    },
    [repoId],
  );

  // Merge optimistic comments, edits, deletes, and resolve state into the thread data.
  const augmentedThreads: OptimisticReviewThread[] = threads
    .map((thread) => {
      const optimistic = state.optimisticComments.get(thread.threadId) ?? [];
      // Filter out optimistic comments whose id now matches a real comment
      // (the server-confirmed reply may appear in the next data refresh).
      const realIds = new Set(thread.comments.map((c) => c.id));
      const pendingOptimistic = optimistic.filter((c) => !realIds.has(c.id));

      // Apply optimistic resolve override if present.
      const resolveOverride = state.optimisticResolves.get(thread.threadId);
      const resolved = resolveOverride ? resolveOverride.resolved : thread.resolved;
      const resolveError = resolveOverride?.error;

      // Apply optimistic edits and filter out optimistic deletes.
      const comments: OptimisticThreadComment[] = [...thread.comments, ...pendingOptimistic]
        .filter((c) => !state.optimisticDeletes.has(c.id))
        .map((c) => {
          const edit = state.optimisticEdits.get(c.id);
          if (edit) {
            return { ...c, body: edit.body, edited: true };
          }
          return c;
        });

      return {
        ...thread,
        resolved,
        resolveError,
        comments,
      };
    })
    // Remove threads where all comments have been deleted.
    .filter((thread) => thread.comments.length > 0);

  return {
    threads: augmentedThreads,
    replyToThread,
    retryReply,
    dismissFailedReply,
    resolveThread,
    dismissResolveError,
    editComment,
    deleteComment,
  };
}
