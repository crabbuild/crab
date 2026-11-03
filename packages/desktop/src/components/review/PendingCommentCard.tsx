import { useCallback, useRef, useState, useEffect, type KeyboardEvent } from "react";
import { Pencil, Trash2, Save, X } from "lucide-react";
import type { PendingComment } from "../../review/types";

// ── Types ────────────────────────────────────────────────────────────

export interface PendingCommentCardProps {
  comment: PendingComment;
  onUpdate(id: string, body: string): void;
  onRemove(id: string): void;
}

// ── Component ────────────────────────────────────────────────────────

/**
 * Renders a pending (draft) comment inline in the diff view.
 *
 * Distinguished from published comments by an amber/yellow left border
 * and a "Pending" badge. Supports inline editing and immediate deletion
 * (no confirmation needed for local drafts).
 */
export function PendingCommentCard({ comment, onUpdate, onRemove }: PendingCommentCardProps) {
  const [editing, setEditing] = useState(false);
  const [editBody, setEditBody] = useState(comment.body);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // Focus textarea when entering edit mode.
  useEffect(() => {
    if (editing) {
      textareaRef.current?.focus();
    }
  }, [editing]);

  const handleEdit = useCallback(() => {
    setEditBody(comment.body);
    setEditing(true);
  }, [comment.body]);

  const handleSave = useCallback(() => {
    const trimmed = editBody.trim();
    if (!trimmed) return;
    onUpdate(comment.id, trimmed);
    setEditing(false);
  }, [editBody, comment.id, onUpdate]);

  const handleCancel = useCallback(() => {
    setEditBody(comment.body);
    setEditing(false);
  }, [comment.body]);

  const handleDelete = useCallback(() => {
    onRemove(comment.id);
  }, [comment.id, onRemove]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        handleSave();
      } else if (e.key === "Escape") {
        e.preventDefault();
        handleCancel();
      }
    },
    [handleSave, handleCancel],
  );

  const lineLabel =
    comment.endLine != null && comment.endLine !== comment.line
      ? `L${comment.line}–${comment.endLine}`
      : `L${comment.line}`;

  return (
    <div
      className="my-1 rounded border border-gx-border border-l-amber-400 border-l-2 bg-gx-surface shadow-sm"
      aria-label={`Pending comment on ${comment.path} ${lineLabel}`}
    >
      {/* Header: badges + actions */}
      <div className="flex items-center justify-between px-3 py-1.5 border-b border-gx-border/50">
        <div className="flex items-center gap-1.5">
          <span className="inline-flex items-center rounded bg-amber-500/15 px-1.5 py-0.5 text-[9px] font-medium text-amber-400">
            Pending
          </span>
          {comment.outdated && (
            <span className="inline-flex items-center rounded bg-orange-500/15 px-1.5 py-0.5 text-[9px] font-medium text-orange-400">
              Outdated
            </span>
          )}
        </div>

        {!editing && (
          <div className="flex items-center gap-1">
            <button
              type="button"
              className="inline-flex items-center gap-0.5 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:bg-gx-accent/10 hover:text-gx-accent"
              onClick={handleEdit}
              aria-label="Edit pending comment"
            >
              <Pencil size={10} />
              Edit
            </button>
            <button
              type="button"
              className="inline-flex items-center gap-0.5 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:bg-red-500/10 hover:text-red-400"
              onClick={handleDelete}
              aria-label="Delete pending comment"
            >
              <Trash2 size={10} />
              Delete
            </button>
          </div>
        )}
      </div>

      {/* Body: read-only or edit mode */}
      <div className="px-3 py-2">
        {editing ? (
          <>
            <textarea
              ref={textareaRef}
              className="w-full resize-y rounded border border-gx-border bg-gx-bg-sub p-2 text-[11px] leading-relaxed text-gx-text placeholder:text-gx-muted focus:border-gx-accent focus-visible:outline-none"
              rows={3}
              value={editBody}
              onChange={(e) => setEditBody(e.target.value)}
              onKeyDown={handleKeyDown}
              aria-label="Edit comment body"
            />
            <div className="mt-1.5 flex items-center justify-end gap-2">
              <button
                type="button"
                className="inline-flex items-center gap-1 rounded px-2 py-1 text-[10px] text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text"
                onClick={handleCancel}
                aria-label="Cancel editing"
              >
                <X size={10} />
                Cancel
              </button>
              <button
                type="button"
                className="inline-flex items-center gap-1 rounded bg-gx-accent/10 px-2 py-1 text-[10px] text-gx-accent hover:bg-gx-accent/20 disabled:cursor-not-allowed disabled:opacity-50"
                disabled={!editBody.trim()}
                onClick={handleSave}
                aria-label="Save comment changes"
              >
                <Save size={10} />
                Save
              </button>
            </div>
          </>
        ) : (
          <p className="whitespace-pre-wrap text-[11px] leading-relaxed text-gx-text">
            {comment.body}
          </p>
        )}
      </div>
    </div>
  );
}
