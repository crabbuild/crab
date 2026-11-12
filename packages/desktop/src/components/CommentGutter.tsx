import { Button } from "@/components/ui/button";
import { Textarea } from "@/components/ui/textarea";
import { MessageSquare, Plus, Send, X } from "lucide-react";
import { useCallback, useState } from "react";

// ── Types ────────────────────────────────────────────────────────────

export interface DiffComment {
  id: string;
  author: string;
  body: string;
  created_at: string;
}

export type CommentsByLine = Record<number, DiffComment[]>;

interface CommentGutterProps {
  /** Map of line numbers to comment threads. */
  commentsByLine: CommentsByLine;
  /** Called when the user submits a new comment on a line. */
  onAddComment?: (line: number, body: string) => void;
}

// ── Main component ───────────────────────────────────────────────────

/**
 * Overlay that renders inline comment threads and add-comment affordances
 * alongside a diff. Designed to be composed inside `<DiffView />`.
 *
 * When `commentsByLine` is empty and `onAddComment` is undefined, this
 * component renders nothing — preserving the existing DiffView behavior.
 */
export function CommentGutter({ commentsByLine, onAddComment }: CommentGutterProps) {
  const [expandedLines, setExpandedLines] = useState<Set<number>>(new Set());
  const [composingLine, setComposingLine] = useState<number | null>(null);

  const toggleThread = useCallback((line: number) => {
    setExpandedLines((prev) => {
      const next = new Set(prev);
      if (next.has(line)) next.delete(line);
      else next.add(line);
      return next;
    });
  }, []);

  const startCompose = useCallback((line: number) => {
    setComposingLine(line);
  }, []);

  const cancelCompose = useCallback(() => {
    setComposingLine(null);
  }, []);

  const lines = Object.keys(commentsByLine)
    .map(Number)
    .sort((a, b) => a - b);

  const hasContent = lines.length > 0 || onAddComment != null;
  if (!hasContent) return null;

  return (
    <div className="border-t border-border bg-background">
      {/* Existing comment threads */}
      {lines.map((line) => (
        <CommentLineEntry
          key={line}
          line={line}
          comments={commentsByLine[line]}
          expanded={expandedLines.has(line)}
          onToggle={() => toggleThread(line)}
          onAddComment={onAddComment}
          composingLine={composingLine}
          onStartCompose={startCompose}
          onCancelCompose={cancelCompose}
        />
      ))}

      {/* Compose on a new line (not already in commentsByLine) */}
      {composingLine != null && !commentsByLine[composingLine] && (
        <NewLineComposer
          line={composingLine}
          onSubmit={(body) => {
            onAddComment?.(composingLine, body);
            setComposingLine(null);
          }}
          onCancel={cancelCompose}
        />
      )}
    </div>
  );
}

// ── Gutter icon strip ────────────────────────────────────────────────

interface GutterIconsProps {
  /** Lines that have existing comments. */
  commentsByLine: CommentsByLine;
  /** Whether the add-comment affordance is available. */
  canAdd: boolean;
  onToggleThread: (line: number) => void;
  onStartCompose: (line: number) => void;
  /** The line the user is currently hovering. */
  hoveredLine: number | null;
}

/**
 * Renders a vertical strip of gutter icons — comment indicators for
 * lines with threads, and a "+" button on hover for adding new comments.
 */
export function GutterIcons({
  commentsByLine,
  canAdd,
  onToggleThread,
  onStartCompose,
  hoveredLine,
}: GutterIconsProps) {
  const lines = Object.keys(commentsByLine)
    .map(Number)
    .sort((a, b) => a - b);

  return (
    <div className="flex flex-col gap-0.5">
      {lines.map((line) => (
        <Button
          variant="ghost"
          size="compact"
          key={`comment-${line}`}
          type="button"
          className="group flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-primary hover:bg-primary/10"
          onClick={() => onToggleThread(line)}
          aria-label={`Toggle comment thread on line ${line}`}
        >
          <MessageSquare size={11} />
          <span className="font-mono">L{line}</span>
          <span className="text-muted-foreground">
            ({commentsByLine[line].length})
          </span>
        </Button>
      ))}
      {canAdd && hoveredLine != null && !commentsByLine[hoveredLine] && (
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:bg-primary/10 hover:text-primary"
          onClick={() => onStartCompose(hoveredLine)}
          aria-label={`Add comment on line ${hoveredLine}`}
        >
          <Plus size={11} />
          <span className="font-mono">L{hoveredLine}</span>
        </Button>
      )}
    </div>
  );
}

// ── Per-line entry ───────────────────────────────────────────────────

function CommentLineEntry({
  line,
  comments,
  expanded,
  onToggle,
  onAddComment,
  composingLine,
  onStartCompose,
  onCancelCompose,
}: {
  line: number;
  comments: DiffComment[];
  expanded: boolean;
  onToggle: () => void;
  onAddComment?: (line: number, body: string) => void;
  composingLine: number | null;
  onStartCompose: (line: number) => void;
  onCancelCompose: () => void;
}) {
  return (
    <div className="border-b border-border/50">
      {/* Collapsed header */}
      <Button
        variant="ghost"
        size="compact"
        type="button"
        className="justify-start flex w-full items-center gap-2 px-3 py-1.5 text-left text-[11px] hover:bg-accent"
        onClick={onToggle}
        aria-label={`${expanded ? "Collapse" : "Expand"} comment thread on line ${line}`}
      >
        <MessageSquare size={12} className="shrink-0 text-primary" />
        <span className="font-mono text-foreground">Line {line}</span>
        <span className="text-muted-foreground">
          {comments.length} {comments.length === 1 ? "comment" : "comments"}
        </span>
        {!expanded && comments.length > 0 && (
          <span className="ml-2 truncate text-muted-foreground/70 italic">
            {comments[0].author}: {comments[0].body.slice(0, 60)}
            {comments[0].body.length > 60 ? "…" : ""}
          </span>
        )}
      </Button>

      {/* Expanded thread */}
      {expanded && (
        <div className="space-y-2 px-3 pb-3 pt-1">
          {comments.map((comment) => (
            <CommentBubble key={comment.id} comment={comment} />
          ))}

          {/* Reply input */}
          {onAddComment && composingLine === line ? (
            <InlineComposer
              onSubmit={(body) => {
                onAddComment(line, body);
                onCancelCompose();
              }}
              onCancel={onCancelCompose}
            />
          ) : onAddComment ? (
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="flex items-center gap-1.5 rounded px-2 py-1 text-[11px] text-muted-foreground hover:bg-accent hover:text-foreground"
              onClick={() => onStartCompose(line)}
            >
              <Plus size={10} />
              Reply
            </Button>
          ) : null}
        </div>
      )}
    </div>
  );
}

// ── Comment bubble ───────────────────────────────────────────────────

function CommentBubble({ comment }: { comment: DiffComment }) {
  return (
    <div className="rounded border border-border bg-card p-2">
      <div className="flex items-center gap-2 text-[10px]">
        <span className="font-medium text-foreground">{comment.author}</span>
        <span className="text-muted-foreground">
          {formatTimestamp(comment.created_at)}
        </span>
      </div>
      <div className="mt-1 whitespace-pre-wrap text-[11px] text-foreground/80">
        {comment.body}
      </div>
    </div>
  );
}

// ── Inline composer ──────────────────────────────────────────────────

function InlineComposer({
  onSubmit,
  onCancel,
}: {
  onSubmit: (body: string) => void;
  onCancel: () => void;
}) {
  const [body, setBody] = useState("");

  return (
    <div className="rounded border border-border bg-card p-2">
      <Textarea
        className="w-full resize-none rounded border border-border bg-background p-1.5 text-[11px] text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
        rows={2}
        placeholder="Write a reply…"
        value={body}
        onChange={(e) => setBody(e.target.value)}
        autoFocus
      />
      <div className="mt-1.5 flex items-center justify-end gap-2">
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="rounded px-2 py-0.5 text-[10px] text-muted-foreground hover:bg-accent hover:text-foreground"
          onClick={onCancel}
          aria-label="Cancel comment"
        >
          <X size={10} className="inline" /> Cancel
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="inline-flex items-center gap-1 rounded bg-primary/10 px-2 py-0.5 text-[10px] text-primary hover:bg-primary/20 disabled:opacity-50"
          disabled={!body.trim()}
          onClick={() => onSubmit(body.trim())}
          aria-label="Submit comment"
        >
          <Send size={10} /> Reply
        </Button>
      </div>
    </div>
  );
}

// ── New-line composer (for lines without existing threads) ────────────

function NewLineComposer({
  line,
  onSubmit,
  onCancel,
}: {
  line: number;
  onSubmit: (body: string) => void;
  onCancel: () => void;
}) {
  const [body, setBody] = useState("");

  return (
    <div className="border-b border-border/50 px-3 py-2">
      <div className="mb-1.5 flex items-center gap-2 text-[11px]">
        <Plus size={11} className="text-primary" />
        <span className="font-mono text-foreground">Line {line}</span>
        <span className="text-muted-foreground">— new comment</span>
      </div>
      <div className="rounded border border-border bg-card p-2">
        <Textarea
          className="w-full resize-none rounded border border-border bg-background p-1.5 text-[11px] text-foreground placeholder:text-muted-foreground focus:border-primary focus-visible:outline-none"
          rows={2}
          placeholder="Write a comment…"
          value={body}
          onChange={(e) => setBody(e.target.value)}
          autoFocus
        />
        <div className="mt-1.5 flex items-center justify-end gap-2">
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded px-2 py-0.5 text-[10px] text-muted-foreground hover:bg-accent hover:text-foreground"
            onClick={onCancel}
            aria-label="Cancel comment"
          >
            <X size={10} className="inline" /> Cancel
          </Button>
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="inline-flex items-center gap-1 rounded bg-primary/10 px-2 py-0.5 text-[10px] text-primary hover:bg-primary/20 disabled:opacity-50"
            disabled={!body.trim()}
            onClick={() => onSubmit(body.trim())}
            aria-label="Submit comment"
          >
            <Send size={10} /> Comment
          </Button>
        </div>
      </div>
    </div>
  );
}

// ── Helpers ──────────────────────────────────────────────────────────

function formatTimestamp(iso: string): string {
  try {
    const d = new Date(iso);
    const diffMs = Date.now() - d.getTime();
    if (diffMs < 60_000) return "just now";
    if (diffMs < 3_600_000) return `${Math.floor(diffMs / 60_000)}m ago`;
    if (diffMs < 86_400_000) return `${Math.floor(diffMs / 3_600_000)}h ago`;
    if (diffMs < 604_800_000) return `${Math.floor(diffMs / 86_400_000)}d ago`;
    return d.toLocaleDateString();
  } catch {
    return iso;
  }
}
