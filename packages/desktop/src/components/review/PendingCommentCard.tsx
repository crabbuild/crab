import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { Pencil, Save, Trash2, X } from "lucide-react";
import {
  useCallback,
  useEffect,
  useRef, useState, type KeyboardEvent,
} from "react";
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
      className="my-1 rounded border border-border border-l-amber-400 border-l-2 bg-card shadow-sm"
      aria-label={`Pending comment on ${comment.path} ${lineLabel}`}
    >
      {/* Header: badges + actions */}
      <div className="flex items-center justify-between px-3 py-1.5 border-b border-border/50">
        <div className="flex items-center gap-1.5">
          <span className="inline-flex items-center rounded bg-warning/15 px-1.5 py-0.5 text-[9px] font-medium text-warning">
            Pending
          </span>
          {comment.outdated && (
            <span className="inline-flex items-center rounded bg-warning/15 px-1.5 py-0.5 text-[9px] font-medium text-warning">
              Outdated
            </span>
          )}
        </div>

        {!editing && (
          <div className="flex items-center gap-1">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="inline-flex items-center gap-0.5 rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:bg-primary/10 hover:text-primary"
              onClick={handleEdit}
              aria-label="Edit pending comment"
            >
              <Pencil size={10} />
              Edit
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="inline-flex items-center gap-0.5 rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:bg-destructive/10 hover:text-destructive"
              onClick={handleDelete}
              aria-label="Delete pending comment"
            >
              <Trash2 size={10} />
              Delete
            </Button>
          </div>
        )}
      </div>

      {/* Body: read-only or edit mode */}
      <div className="px-3 py-2">
        {editing ? (
          <>
            <Textarea
              ref={textareaRef}
              className="w-full resize-y rounded border border-border bg-background p-2 text-[11px] leading-relaxed text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
              rows={3}
              value={editBody}
              onChange={(e) => setEditBody(e.target.value)}
              onKeyDown={handleKeyDown}
              aria-label="Edit comment body"
            />
            <div className="mt-1.5 flex items-center justify-end gap-2">
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="inline-flex items-center gap-1 rounded px-2 py-1 text-[10px] text-muted-foreground hover:bg-accent hover:text-foreground"
                onClick={handleCancel}
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
                disabled={!editBody.trim()}
                onClick={handleSave}
                aria-label="Save comment changes"
              >
                <Save size={10} />
                Save
              </Button>
            </div>
          </>
        ) : (
          <p className="whitespace-pre-wrap text-[11px] leading-relaxed text-foreground">
            {comment.body}
          </p>
        )}
      </div>
    </div>
  );
}
