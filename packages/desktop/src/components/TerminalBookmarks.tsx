import { Button } from "@/components/ui/button";
/**
 * Terminal session bookmarks panel.
 * Allows users to bookmark positions in scrollback and navigate between them.
 */

import { Bookmark, ChevronDown, ChevronUp, Plus, X } from "lucide-react";
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
    <div className="shrink-0 border-b border-border bg-card px-3 py-1.5">
      <div className="flex items-center gap-2">
        <Bookmark size={12} className="text-primary shrink-0" />
        <span className="text-[11px] text-foreground font-medium">
          Bookmarks ({bookmarks.length})
        </span>

        <div className="flex-1" />

        {/* Navigation */}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onPrevBookmark}
          className="rounded px-1 py-0.5 text-muted-foreground hover:text-foreground hover:bg-accent"
          title="Previous bookmark"
        >
          <ChevronUp size={12} />
        </Button>
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onNextBookmark}
          className="rounded px-1 py-0.5 text-muted-foreground hover:text-foreground hover:bg-accent"
          title="Next bookmark"
        >
          <ChevronDown size={12} />
        </Button>

        {/* Add bookmark */}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onAdd}
          className="flex items-center gap-1 rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:text-primary hover:bg-accent"
          title="Add bookmark at current position"
        >
          <Plus size={10} />
          Add
        </Button>

        <Button
          variant="ghost"
          size="compact"
          type="button"
          onClick={onClose}
          className="text-muted-foreground hover:text-foreground"
          title="Close bookmarks"
        >
          <X size={12} />
        </Button>
      </div>

      {/* Bookmark list */}
      {bookmarks.length > 0 && (
        <div className="mt-1 max-h-24 overflow-y-auto">
          {bookmarks.map((bm) => (
            <div
              key={bm.id}
              className="group flex items-center gap-2 rounded px-2 py-0.5 hover:bg-accent"
            >
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={() => onNavigate(bm.line)}
                className="justify-start flex-1 truncate text-left text-[10px] text-foreground"
                title={`Line ${bm.line} — ${new Date(bm.timestamp).toLocaleTimeString()}`}
              >
                <span className="font-mono text-muted-foreground mr-1">
                  L{bm.line}
                </span>
                {bm.label}
              </Button>
              <Button
                variant="ghost"
                size="compact"
                type="button"
                onClick={() => onRemove(bm.id)}
                className="opacity-0 group-hover:opacity-100 text-muted-foreground hover:text-destructive transition"
                aria-label="Remove bookmark"
              >
                <X size={9} />
              </Button>
            </div>
          ))}
        </div>
      )}

      {bookmarks.length === 0 && (
        <div className="mt-1 text-[10px] text-muted-foreground">
          No bookmarks. Click "Add" to bookmark the current scroll position.
        </div>
      )}
    </div>
  );
}
