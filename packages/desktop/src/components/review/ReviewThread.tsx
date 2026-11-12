import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Textarea } from "@/components/ui/textarea";
import clsx from "clsx";
import {
  AlertCircle,
  Check,
  ChevronRight,
  FileText,
  Loader2,
  MoreHorizontal,
  Pencil,
  RefreshCw,
  Save,
  Send,
  Trash2,
  Undo2,
  X,
} from "lucide-react";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type KeyboardEvent,
} from "react";
import type { ReviewThread as ReviewThreadType, ThreadComment } from "../../review/types";
import type {
  OptimisticReviewThread,
  OptimisticThreadComment,
} from "../../review/useOptimisticThreads";

// ── Types ────────────────────────────────────────────────────────────

export interface ReviewThreadProps {
  thread: ReviewThreadType | OptimisticReviewThread;
  /** Current user login for isMine checks */
  currentUser: string;
  /** Whether the current user has write access (for apply suggestion) */
  hasWriteAccess: boolean;
  /** Callbacks */
  onReply(threadId: string, body: string): Promise<void> | void;
  onResolve(threadId: string, resolved: boolean): Promise<void> | void;
  onEditComment(commentId: string, body: string): Promise<void>;
  onDeleteComment(commentId: string): Promise<void>;
  onApplySuggestion(commentId: string): Promise<void>;
  /** Called when user clicks Retry on a failed optimistic reply. */
  onRetryReply?(threadId: string, tempId: string): void;
  /** Called when user dismisses a failed optimistic reply. */
  onDismissFailedReply?(threadId: string, tempId: string): void;
  /** Called when user dismisses the inline resolve/unresolve error. */
  onDismissResolveError?(threadId: string): void;
}

// ── Helpers ──────────────────────────────────────────────────────────

function formatRelativeDate(iso: string): string {
  try {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return iso;
    const delta = (Date.now() - d.getTime()) / 1000;
    if (delta < 60) return "just now";
    if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
    if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
    if (delta < 604800) return `${Math.floor(delta / 86400)}d ago`;
    return d.toLocaleDateString();
  } catch {
    return iso;
  }
}

// ── Component ────────────────────────────────────────────────────────

/**
 * Renders a review thread anchored to a file:line in the diff.
 *
 * Features:
 * - Header with file:line anchor and Resolve/Unresolve button
 * - Chronological comment list with author, timestamp, body
 * - Overflow menu (Edit/Delete) on own comments
 * - Reply input at the bottom
 * - Collapsed state when resolved (single-line summary, expandable)
 * - Outdated styling (reduced opacity, strikethrough on anchor)
 */
export function ReviewThreadComponent({
  thread,
  currentUser,
  hasWriteAccess: _hasWriteAccess,
  onReply,
  onResolve,
  onEditComment,
  onDeleteComment,
  onApplySuggestion: _onApplySuggestion,
  onRetryReply,
  onDismissFailedReply,
  onDismissResolveError,
}: ReviewThreadProps) {
  const [expanded, setExpanded] = useState(!thread.resolved);
  const [replyBody, setReplyBody] = useState("");
  const [replyLoading, setReplyLoading] = useState(false);
  const replyRef = useRef<HTMLTextAreaElement>(null);

  // Extract resolve error from optimistic thread data.
  const resolveError = "resolveError" in thread ? (thread as OptimisticReviewThread).resolveError : undefined;

  // Sync expanded state when resolved status changes externally.
  useEffect(() => {
    setExpanded(!thread.resolved);
  }, [thread.resolved]);

  const handleReply = useCallback(async () => {
    const trimmed = replyBody.trim();
    if (!trimmed || replyLoading) return;
    setReplyLoading(true);
    try {
      await onReply(thread.threadId, trimmed);
      setReplyBody("");
    } finally {
      setReplyLoading(false);
    }
  }, [replyBody, replyLoading, onReply, thread.threadId]);

  const handleReplyKeyDown = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        handleReply();
      }
    },
    [handleReply],
  );

  const handleResolve = useCallback(() => {
    onResolve(thread.threadId, !thread.resolved);
  }, [onResolve, thread.threadId, thread.resolved]);

  const firstComment = thread.comments[0];
  const summaryText = firstComment
    ? firstComment.body.slice(0, 100) + (firstComment.body.length > 100 ? "…" : "")
    : "";

  // Resolved + collapsed: render single-line summary
  if (thread.resolved && !expanded) {
    return (
      <div
        className={clsx(
          "my-1 rounded border border-border bg-card shadow-sm",
          thread.outdated && "opacity-60",
        )}
      >
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="justify-start flex w-full items-center gap-2 px-3 py-2 text-left text-[11px] text-muted-foreground hover:bg-accent"
          onClick={() => setExpanded(true)}
          aria-label="Expand resolved thread"
        >
          <ChevronRight size={12} className="shrink-0" />
          <Check size={12} className="shrink-0 text-success" />
          <span className="truncate">
            <span className="font-medium text-foreground">
              {firstComment?.author ?? "Unknown"}
            </span>
            {" · "}
            {summaryText}
          </span>
        </Button>
        {resolveError && (
          <div className="flex items-center justify-between border-t border-destructive/30 bg-destructive/5 px-3 py-1.5">
            <span className="flex items-center gap-1.5 text-[10px] text-destructive">
              <AlertCircle size={10} />
              {resolveError}
            </span>
            {onDismissResolveError && (
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="inline-flex items-center rounded p-0.5 text-destructive hover:bg-destructive/10"
                onClick={() => onDismissResolveError(thread.threadId)}
                aria-label="Dismiss resolve error"
              >
                <X size={10} />
              </Button>
            )}
          </div>
        )}
      </div>
    );
  }

  return (
    <div
      className={clsx(
        "my-1 rounded border border-border bg-card shadow-sm",
        thread.outdated && "opacity-60",
      )}
      aria-label={`Review thread on ${thread.path}:${thread.line}`}
    >
      {/* Header: file:line anchor + Resolve/Unresolve */}
      <div className="flex items-center justify-between border-b border-border/50 px-3 py-1.5">
        <div className="flex items-center gap-1.5 text-[10px] text-muted-foreground">
          <FileText size={10} className="shrink-0" />
          <span
            className={clsx(
              "font-mono",
              thread.outdated && "line-through",
            )}
          >
            {thread.path}:{thread.line}
          </span>
          {thread.outdated && (
            <span className="inline-flex items-center rounded bg-warning/15 px-1.5 py-0.5 text-[9px] font-medium text-warning">
              Outdated
            </span>
          )}
        </div>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={clsx(
            "inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px]",
            thread.resolved
              ? "text-muted-foreground hover:bg-warning/10 hover:text-warning"
              : "text-muted-foreground hover:bg-success/10 hover:text-success",
          )}
          onClick={handleResolve}
          aria-label={thread.resolved ? "Unresolve thread" : "Resolve thread"}
        >
          {thread.resolved ? (
            <>
              <Undo2 size={10} />
              Unresolve
            </>
          ) : (
            <>
              <Check size={10} />
              Resolve
            </>
          )}
        </Button>
      </div>

      {/* Inline error from failed resolve/unresolve */}
      {resolveError && (
        <div className="flex items-center justify-between border-b border-destructive/30 bg-destructive/5 px-3 py-1.5">
          <span className="flex items-center gap-1.5 text-[10px] text-destructive">
            <AlertCircle size={10} />
            {resolveError}
          </span>
          {onDismissResolveError && (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="inline-flex items-center rounded p-0.5 text-destructive hover:bg-destructive/10"
              onClick={() => onDismissResolveError(thread.threadId)}
              aria-label="Dismiss resolve error"
            >
              <X size={10} />
            </Button>
          )}
        </div>
      )}

      {/* Comments */}
      <div className="divide-y divide-border/30">
        {thread.comments.map((comment) => {
          const optimistic = comment as OptimisticThreadComment;
          if (optimistic.failed) {
            return (
              <FailedCommentItem
                key={comment.id}
                comment={optimistic}
                threadId={thread.threadId}
                onRetry={onRetryReply}
                onDismiss={onDismissFailedReply}
              />
            );
          }
          if (optimistic.pending) {
            return <PendingCommentItem key={comment.id} comment={optimistic} />;
          }
          return (
            <CommentItem
              key={comment.id}
              comment={comment}
              currentUser={currentUser}
              onEdit={onEditComment}
              onDelete={onDeleteComment}
            />
          );
        })}
      </div>

      {/* Reply input */}
      <div className="border-t border-border/50 px-3 py-2">
        <Textarea
          ref={replyRef}
          className="w-full resize-y rounded border border-border bg-background p-2 text-[11px] leading-relaxed text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
          rows={2}
          placeholder="Write a reply…"
          value={replyBody}
          onChange={(e) => setReplyBody(e.target.value)}
          onKeyDown={handleReplyKeyDown}
          aria-label={`Reply to thread on ${thread.path}:${thread.line}`}
        />
        <div className="mt-1.5 flex items-center justify-end">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="inline-flex items-center gap-1 rounded bg-primary/10 px-2 py-1 text-[10px] text-primary hover:bg-primary/20 disabled:cursor-not-allowed disabled:opacity-50"
            disabled={!replyBody.trim() || replyLoading}
            onClick={handleReply}
            aria-label="Submit reply"
          >
            <Send size={10} />
            {replyLoading ? "Sending…" : "Reply"}
          </Button>
        </div>
      </div>
    </div>
  );
}

// ── CommentItem ──────────────────────────────────────────────────────

interface CommentItemProps {
  comment: ThreadComment;
  currentUser: string;
  onEdit(commentId: string, body: string): Promise<void>;
  onDelete(commentId: string): Promise<void>;
}

function CommentItem({ comment, currentUser, onEdit, onDelete }: CommentItemProps) {
  const [menuOpen, setMenuOpen] = useState(false);
  const [editing, setEditing] = useState(false);
  const [editBody, setEditBody] = useState(comment.body);
  const [editLoading, setEditLoading] = useState(false);
  const [editError, setEditError] = useState<string | null>(null);
  const [deleteConfirm, setDeleteConfirm] = useState(false);
  const [deleteLoading, setDeleteLoading] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  const isMine = comment.isMine || comment.author === currentUser;

  useEffect(() => {
    if (editing) {
      textareaRef.current?.focus();
    }
  }, [editing]);

  // Close menu on outside click.

  const handleStartEdit = useCallback(() => {
    setEditBody(comment.body);
    setEditing(true);
    setMenuOpen(false);
  }, [comment.body]);

  const handleSaveEdit = useCallback(async () => {
    const trimmed = editBody.trim();
    if (!trimmed || editLoading) return;
    setEditLoading(true);
    setEditError(null);
    try {
      await onEdit(comment.id, trimmed);
      setEditing(false);
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Failed to save edit";
      setEditError(message);
    } finally {
      setEditLoading(false);
    }
  }, [editBody, editLoading, onEdit, comment.id]);

  const handleCancelEdit = useCallback(() => {
    setEditBody(comment.body);
    setEditing(false);
    setEditError(null);
  }, [comment.body]);

  const handleEditKeyDown = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        handleSaveEdit();
      } else if (e.key === "Escape") {
        e.preventDefault();
        handleCancelEdit();
      }
    },
    [handleSaveEdit, handleCancelEdit],
  );

  const handleDelete = useCallback(() => {
    setMenuOpen(false);
    setDeleteConfirm(true);
  }, []);

  const handleConfirmDelete = useCallback(async () => {
    setDeleteLoading(true);
    setDeleteError(null);
    try {
      await onDelete(comment.id);
      setDeleteConfirm(false);
    } catch (err: unknown) {
      const message = err instanceof Error ? err.message : "Failed to delete comment";
      setDeleteError(message);
    } finally {
      setDeleteLoading(false);
    }
  }, [onDelete, comment.id]);

  return (
    <div className="px-3 py-2">
      {/* Comment header: author + timestamp + overflow menu */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-1.5 text-[10px]">
          <span className="font-medium text-foreground">@{comment.author}</span>
          <span className="text-muted-foreground">·</span>
          <span className="text-muted-foreground">
            {formatRelativeDate(comment.createdAt)}
          </span>
          {comment.edited && (
            <span className="text-muted-foreground italic">(edited)</span>
          )}
        </div>

        {isMine && !editing && (
          <div className="relative" ref={menuRef}>
            <DropdownMenu open={menuOpen} onOpenChange={setMenuOpen}>
              <DropdownMenuTrigger asChild>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="inline-flex items-center rounded p-0.5 text-muted-foreground hover:bg-accent hover:text-foreground"
                  onClick={() => setMenuOpen((v) => !v)}
                  aria-label="Comment actions"
                  aria-expanded={menuOpen}
                >
                  <MoreHorizontal size={12} />
                </Button>
              </DropdownMenuTrigger>

              {menuOpen && (
                <DropdownMenuContent
                  side="bottom"
                  align="end"
                  className=" min-w-[100px] rounded border border-border bg-card py-1 "
                >
                  <DropdownMenuGroup>
                    <DropdownMenuItem
                      className="justify-start flex w-full items-center gap-2 px-3 py-1.5 text-left text-[10px] text-foreground hover:bg-accent"
                      onSelect={handleStartEdit}
                    >
                      <Pencil size={10} />
                      Edit
                    </DropdownMenuItem>
                    <DropdownMenuItem
                      className="justify-start flex w-full items-center gap-2 px-3 py-1.5 text-left text-[10px] text-destructive hover:bg-destructive/10"
                      onSelect={handleDelete}
                    >
                      <Trash2 size={10} />
                      Delete
                    </DropdownMenuItem>
                  </DropdownMenuGroup>
                </DropdownMenuContent>
              )}
            </DropdownMenu>
          </div>
        )}
      </div>

      {/* Comment body: read-only or edit mode */}
      <div className="mt-1">
        {editing ? (
          <>
            <Textarea
              ref={textareaRef}
              className="w-full resize-y rounded border border-border bg-background p-2 text-[11px] leading-relaxed text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
              rows={3}
              value={editBody}
              onChange={(e) => setEditBody(e.target.value)}
              onKeyDown={handleEditKeyDown}
              aria-label="Edit comment body"
            />
            {editError && (
              <div className="mt-1 flex items-center gap-1.5 text-[10px] text-destructive">
                <AlertCircle size={10} />
                {editError}
              </div>
            )}
            <div className="mt-1.5 flex items-center justify-end gap-2">
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="inline-flex items-center gap-1 rounded px-2 py-1 text-[10px] text-muted-foreground hover:bg-accent hover:text-foreground"
                onClick={handleCancelEdit}
                aria-label="Cancel editing"
              >
                <X size={10} />
                Cancel
              </Button>
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="inline-flex items-center gap-1 rounded bg-primary/10 px-2 py-1 text-[10px] text-primary hover:bg-primary/20 disabled:cursor-not-allowed disabled:opacity-50"
                disabled={!editBody.trim() || editLoading}
                onClick={handleSaveEdit}
                aria-label="Save comment changes"
              >
                <Save size={10} />
                {editLoading ? "Saving…" : "Save"}
              </Button>
            </div>
          </>
        ) : (
          <p className="whitespace-pre-wrap text-[11px] leading-relaxed text-foreground">
            {comment.body}
          </p>
        )}
      </div>

      {/* Delete confirmation */}
      {deleteConfirm && (
        <div className="mt-2 flex items-center gap-2 rounded border border-destructive/30 bg-destructive/5 px-2 py-1.5">
          <span className="text-[10px] text-destructive">
            Delete this comment?
          </span>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="inline-flex items-center gap-1 rounded bg-destructive/15 px-2 py-0.5 text-[10px] text-destructive hover:bg-destructive/25 disabled:cursor-not-allowed disabled:opacity-50"
            onClick={handleConfirmDelete}
            disabled={deleteLoading}
            aria-label="Confirm delete"
          >
            {deleteLoading ? "Deleting…" : "Delete"}
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="inline-flex items-center gap-1 rounded px-2 py-0.5 text-[10px] text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={() => { setDeleteConfirm(false); setDeleteError(null); }}
            disabled={deleteLoading}
            aria-label="Cancel delete"
          >
            Cancel
          </Button>
          {deleteError && (
            <span className="flex items-center gap-1 text-[10px] text-destructive">
              <AlertCircle size={10} />
              {deleteError}
            </span>
          )}
        </div>
      )}
    </div>
  );
}

// ── PendingCommentItem ───────────────────────────────────────────────

interface PendingCommentItemProps {
  comment: OptimisticThreadComment;
}

/** Renders an optimistic reply that is still being sent to the server. */
function PendingCommentItem({ comment }: PendingCommentItemProps) {
  return (
    <div className="px-3 py-2 opacity-70">
      <div className="flex items-center gap-1.5 text-[10px]">
        <span className="font-medium text-foreground">@{comment.author}</span>
        <span className="text-muted-foreground">·</span>
        <span className="inline-flex items-center gap-1 text-muted-foreground">
          <Loader2 size={10} className="animate-spin" />
          Sending…
        </span>
      </div>
      <div className="mt-1">
        <p className="whitespace-pre-wrap text-[11px] leading-relaxed text-foreground">
          {comment.body}
        </p>
      </div>
    </div>
  );
}

// ── FailedCommentItem ────────────────────────────────────────────────

interface FailedCommentItemProps {
  comment: OptimisticThreadComment;
  threadId: string;
  onRetry?(threadId: string, tempId: string): void;
  onDismiss?(threadId: string, tempId: string): void;
}

/** Renders a reply that failed to publish, with a red indicator and retry/dismiss actions. */
function FailedCommentItem({ comment, threadId, onRetry, onDismiss }: FailedCommentItemProps) {
  return (
    <div className="border-l-2 border-destructive px-3 py-2">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-1.5 text-[10px]">
          <span className="font-medium text-foreground">@{comment.author}</span>
          <span className="text-muted-foreground">·</span>
          <span className="inline-flex items-center gap-1 text-destructive">
            <AlertCircle size={10} />
            Failed
          </span>
        </div>
      </div>
      <div className="mt-1">
        <p className="whitespace-pre-wrap text-[11px] leading-relaxed text-foreground">
          {comment.body}
        </p>
      </div>
      {comment.failureMessage && (
        <p className="mt-1 text-[10px] text-destructive">
          {comment.failureMessage}
        </p>
      )}
      <div className="mt-1.5 flex items-center gap-2">
        {onRetry && (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="inline-flex items-center gap-1 rounded bg-primary/10 px-2 py-0.5 text-[10px] text-primary hover:bg-primary/20"
            onClick={() => onRetry(threadId, comment.id)}
            aria-label="Retry sending reply"
          >
            <RefreshCw size={10} />
            Retry
          </Button>
        )}
        {onDismiss && (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="inline-flex items-center gap-1 rounded px-2 py-0.5 text-[10px] text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={() => onDismiss(threadId, comment.id)}
            aria-label="Dismiss failed reply"
          >
            <X size={10} />
            Dismiss
          </Button>
        )}
      </div>
    </div>
  );
}
