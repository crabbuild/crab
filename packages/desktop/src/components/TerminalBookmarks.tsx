/**
 * Terminal session bookmarks panel.
 * Allows users to bookmark positions in scrollback and navigate between them.
 */

import { Bookmark, ChevronUp, ChevronDown, X, Plus } from "lucide-react";
import type { Bookmark as BookmarkType } from "../lib/terminal-ux-enhancements";

interface Props {
  bookmarks: BookmarkType[];
  onAdd: () => void;
  onRemove: (id: string) => void;
  onNavigate: (line: number) => void;
  onNextBookmark: () => void;
  onPrevBookmark: () => void;
  onClose: () => void;
}

export function TerminalBookmarksPanel({
  bookmarks,
  onAdd,
  onRemove,
  onNavigate,
  onNextBookmark,
  onPrevBookmark,
  onClose,
}: Props) {
  return (
    <div className="shrink-0 border-b border-gx-border bg-gx-surface px-3 py-1.5">
      <div className="flex items-center gap-2">
        <Bookmark size={12} className="text-gx-accent shrink-0" />
        <span className="text-[11px] text-gx-text font-medium">
          Bookmarks ({bookmarks.length})
        </span>

        <div className="flex-1" />

        {/* Navigation */}
        <button
          type="button"
          onClick={onPrevBookmark}
          className="rounded px-1 py-0.5 text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
          title="Previous bookmark"
        >
          <ChevronUp size={12} />
        </button>
        <button
          type="button"
          onClick={onNextBookmark}
          className="rounded px-1 py-0.5 text-gx-muted hover:text-gx-text hover:bg-gx-surface-hover"
          title="Next bookmark"
        >
          <ChevronDown size={12} />
        </button>

        {/* Add bookmark */}
        <button
          type="button"
          onClick={onAdd}
          className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-gx-muted hover:text-gx-accent hover:bg-gx-surface-hover"
          title="Add bookmark at current position"
        >
          <Plus size={10} />
          Add
        </button>

        <button
          type="button"
          onClick={onClose}
          className="text-gx-muted hover:text-gx-text"
          title="Close bookmarks"
        >
          <X size={12} />
        </button>
      </div>

      {/* Bookmark list */}
      {bookmarks.length > 0 && (
        <div className="mt-1 max-h-24 overflow-y-auto">
          {bookmarks.map((bm) => (
            <div
              key={bm.id}
              className="group flex items-center gap-2 rounded px-2 py-0.5 hover:bg-gx-surface-hover"
            >
              <button
                type="button"
                onClick={() => onNavigate(bm.line)}
                className="flex-1 truncate text-left text-[10px] text-gx-text"
                title={`Line ${bm.line} — ${new Date(bm.timestamp).toLocaleTimeString()}`}
              >
                <span className="font-mono text-gx-muted mr-1">L{bm.line}</span>
                {bm.label}
              </button>
              <button
                type="button"
                onClick={() => onRemove(bm.id)}
                className="opacity-0 group-hover:opacity-100 text-gx-muted hover:text-gx-error transition"
                aria-label="Remove bookmark"
              >
                <X size={9} />
              </button>
            </div>
          ))}
        </div>
      )}

      {bookmarks.length === 0 && (
        <div className="mt-1 text-[10px] text-gx-muted">
          No bookmarks. Click "Add" to bookmark the current scroll position.
        </div>
      )}
    </div>
  );
}
