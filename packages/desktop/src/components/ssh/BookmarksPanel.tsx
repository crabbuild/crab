import { Input } from "@/components/ui/input";
/**
 * Bookmarks Panel — bookmark list above directory tree, add/remove with
 * label input, recent files list, connection initiation on selection,
 * and path-not-found handling.
 */

import { useState, useCallback, useMemo } from "react";
import {
  Bookmark,
  Plus,
  Trash2,
  Clock,
  FolderOpen,
  FileText,
  AlertCircle,
  Loader2,
  X,
} from "lucide-react";
import type { RemoteBookmark, RecentFile } from "../../../electron/ssh-types";

// ── Types ────────────────────────────────────────────────────────

interface BookmarksPanelProps {
  bookmarks: RemoteBookmark[];
  recents: RecentFile[];
  /** Whether a connection/navigation is in progress */
  connecting?: boolean;
  /** Error state for path not found */
  pathNotFound?: { path: string; bookmarkId?: string; isRecent?: boolean };
  onAddBookmark: (path: string, label: string) => void;
  onRemoveBookmark: (bookmarkId: string) => void;
  onSelectBookmark: (bookmark: RemoteBookmark) => void;
  onSelectRecent: (recent: RecentFile) => void;
  /** Called when user confirms removal of a not-found path */
  onRemoveNotFound: (path: string, bookmarkId?: string) => void;
  /** Called to dismiss the path-not-found error */
  onDismissNotFound: () => void;
}

// ── Helpers ──────────────────────────────────────────────────────

function formatTimestamp(ts: number): string {
  const now = Date.now();
  const diffMs = now - ts;

  if (diffMs < 60_000) return "Just now";
  if (diffMs < 3_600_000) return `${Math.floor(diffMs / 60_000)}m ago`;
  if (diffMs < 86_400_000) return `${Math.floor(diffMs / 3_600_000)}h ago`;
  const date = new Date(ts);
  return date.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

// ── Add Bookmark Form ────────────────────────────────────────────

function AddBookmarkForm({
  onAdd,
  onCancel,
}: {
  onAdd: (path: string, label: string) => void;
  onCancel: () => void;
}) {
  const [path, setPath] = useState("");
  const [label, setLabel] = useState("");
  const [error, setError] = useState<string | null>(null);

  const handleSubmit = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      if (!path.trim()) {
        setError("Path is required");
        return;
      }
      if (!label.trim()) {
        setError("Label is required");
        return;
      }
      if (label.trim().length > 100) {
        setError("Label must be 100 characters or fewer");
        return;
      }
      onAdd(path.trim(), label.trim());
    },
    [path, label, onAdd],
  );

  return (
    <form
      onSubmit={handleSubmit}
      className="border border-border border-border rounded-md p-3 space-y-2"
    >
      <div>
        <label htmlFor="bk-path" className="text-xs text-muted-foreground text-muted-foreground">
          Remote Path
        </label>
        <Input
          id="bk-path"
          type="text"
          value={path}
          onChange={(e) => { setPath(e.target.value); setError(null); }}
          placeholder="/home/user/project"
          className="w-full px-2 py-1 text-sm border border-border border-border rounded bg-background bg-card"
          required
        />
      </div>
      <div>
        <label htmlFor="bk-label" className="text-xs text-muted-foreground text-muted-foreground">
          Label (1-100 characters)
        </label>
        <Input
          id="bk-label"
          type="text"
          value={label}
          onChange={(e) => { setLabel(e.target.value); setError(null); }}
          placeholder="My Project"
          maxLength={100}
          className="w-full px-2 py-1 text-sm border border-border border-border rounded bg-background bg-card"
          required
        />
      </div>
      {error && (
        <p className="flex items-center gap-1 text-[10px] text-destructive" role="alert">
          <AlertCircle className="w-3 h-3" />
          {error}
        </p>
      )}
      <div className="flex justify-end gap-2 pt-1">
        <button
          type="button"
          onClick={onCancel}
          className="px-3 py-1 text-xs text-muted-foreground text-muted-foreground hover:bg-card hover:bg-card rounded"
        >
          Cancel
        </button>
        <button
          type="submit"
          className="px-3 py-1 text-xs bg-info text-info-foreground rounded hover:bg-info"
        >
          Add Bookmark
        </button>
      </div>
    </form>
  );
}

// ── Path Not Found Banner ────────────────────────────────────────

function PathNotFoundBanner({
  path,
  onRemove,
  onDismiss,
}: {
  path: string;
  bookmarkId?: string;
  onRemove: () => void;
  onDismiss: () => void;
}) {
  return (
    <div className="border border-warning border-warning rounded-md p-2 bg-warning/10 bg-warning/20 space-y-1.5">
      <div className="flex items-start justify-between gap-2">
        <div className="flex items-start gap-2">
          <AlertCircle className="w-3.5 h-3.5 text-warning shrink-0 mt-0.5" />
          <div>
            <p className="text-xs text-warning text-warning font-medium">
              Path not found
            </p>
            <p className="text-[10px] text-muted-foreground text-muted-foreground font-mono truncate">
              {path}
            </p>
          </div>
        </div>
        <button
          onClick={onDismiss}
          className="p-0.5 text-muted-foreground hover:text-muted-foreground hover:text-muted-foreground rounded"
          aria-label="Dismiss"
        >
          <X className="w-3 h-3" />
        </button>
      </div>
      <div className="flex justify-end gap-2">
        <button
          onClick={onDismiss}
          className="px-2 py-0.5 text-[10px] text-muted-foreground text-muted-foreground hover:bg-card hover:bg-card rounded"
        >
          Keep
        </button>
        <button
          onClick={onRemove}
          className="px-2 py-0.5 text-[10px] text-destructive text-destructive hover:bg-destructive/10 hover:bg-destructive/20 rounded"
        >
          Remove entry
        </button>
      </div>
    </div>
  );
}

// ── Main Component ───────────────────────────────────────────────

export function BookmarksPanel({
  bookmarks,
  recents,
  connecting = false,
  pathNotFound,
  onAddBookmark,
  onRemoveBookmark,
  onSelectBookmark,
  onSelectRecent,
  onRemoveNotFound,
  onDismissNotFound,
}: BookmarksPanelProps) {
  const [showAddForm, setShowAddForm] = useState(false);

  const handleAdd = useCallback(
    (path: string, label: string) => {
      onAddBookmark(path, label);
      setShowAddForm(false);
    },
    [onAddBookmark],
  );

  const sortedRecents = useMemo(
    () => [...recents].sort((a, b) => b.lastAccessedAt - a.lastAccessedAt),
    [recents],
  );

  return (
    <div className="flex flex-col h-full overflow-hidden" role="region" aria-label="Bookmarks and Recent Files">
      {/* Header */}
      <div className="flex items-center justify-between px-3 py-2 border-b border-border border-border">
        <h3 className="text-xs font-semibold text-muted-foreground text-muted-foreground flex items-center gap-1.5">
          <Bookmark className="w-4 h-4" />
          Bookmarks
        </h3>
        <div className="flex items-center gap-1">
          {connecting && (
            <Loader2 className="w-3.5 h-3.5 text-info animate-spin" aria-label="Connecting" />
          )}
          {!showAddForm && (
            <button
              onClick={() => setShowAddForm(true)}
              className="flex items-center gap-1 px-2 py-1 text-xs text-info text-info hover:bg-info/10 hover:bg-info/20 rounded"
              aria-label="Add bookmark"
            >
              <Plus className="w-3 h-3" />
              Add
            </button>
          )}
        </div>
      </div>

      <div className="flex-1 overflow-y-auto px-3 py-2 space-y-3">
        {/* Path not found banner */}
        {pathNotFound && (
          <PathNotFoundBanner
            path={pathNotFound.path}
            bookmarkId={pathNotFound.bookmarkId}
            onRemove={() => onRemoveNotFound(pathNotFound.path, pathNotFound.bookmarkId)}
            onDismiss={onDismissNotFound}
          />
        )}

        {/* Add form */}
        {showAddForm && (
          <AddBookmarkForm onAdd={handleAdd} onCancel={() => setShowAddForm(false)} />
        )}

        {/* Bookmarks list */}
        <section>
          {bookmarks.length === 0 && !showAddForm ? (
            <p className="text-xs text-muted-foreground text-muted-foreground italic text-center py-2">
              No bookmarks yet.
            </p>
          ) : (
            <ul className="space-y-0.5" aria-label="Bookmark list">
              {bookmarks.map((bk) => (
                <li
                  key={bk.id}
                  className="flex items-center justify-between px-2 py-1.5 rounded hover:bg-background hover:bg-card group cursor-pointer"
                  onClick={() => onSelectBookmark(bk)}
                  role="button"
                  tabIndex={0}
                  onKeyDown={(e) => { if (e.key === "Enter") onSelectBookmark(bk); }}
                >
                  <div className="flex items-center gap-2 min-w-0">
                    <FolderOpen className="w-3.5 h-3.5 text-info text-info shrink-0" />
                    <div className="min-w-0">
                      <p className="text-xs font-medium text-foreground text-muted-foreground truncate">
                        {bk.label}
                      </p>
                      <p className="text-[10px] text-muted-foreground text-muted-foreground truncate font-mono">
                        {bk.path}
                      </p>
                    </div>
                  </div>
                  <button
                    onClick={(e) => { e.stopPropagation(); onRemoveBookmark(bk.id); }}
                    className="p-0.5 text-muted-foreground hover:text-destructive hover:text-destructive rounded opacity-0 group-hover:opacity-100 transition-opacity"
                    aria-label={`Remove bookmark ${bk.label}`}
                    title="Remove bookmark"
                  >
                    <Trash2 className="w-3 h-3" />
                  </button>
                </li>
              ))}
            </ul>
          )}
        </section>

        {/* Recent files */}
        {sortedRecents.length > 0 && (
          <section>
            <h4 className="text-xs font-medium text-muted-foreground text-muted-foreground flex items-center gap-1 mb-1.5">
              <Clock className="w-3.5 h-3.5" />
              Recent Files
            </h4>
            <ul className="space-y-0.5" aria-label="Recent files">
              {sortedRecents.map((recent) => (
                <li
                  key={`${recent.profileId}-${recent.path}`}
                  className="flex items-center gap-2 px-2 py-1.5 rounded hover:bg-background hover:bg-card cursor-pointer"
                  onClick={() => onSelectRecent(recent)}
                  role="button"
                  tabIndex={0}
                  onKeyDown={(e) => { if (e.key === "Enter") onSelectRecent(recent); }}
                >
                  <FileText className="w-3.5 h-3.5 text-muted-foreground text-muted-foreground shrink-0" />
                  <div className="min-w-0 flex-1">
                    <p className="text-xs font-medium text-foreground text-muted-foreground truncate">
                      {recent.fileName}
                    </p>
                    <p className="text-[10px] text-muted-foreground text-muted-foreground truncate font-mono">
                      {recent.path}
                    </p>
                  </div>
                  <span className="text-[10px] text-muted-foreground text-muted-foreground shrink-0">
                    {formatTimestamp(recent.lastAccessedAt)}
                  </span>
                </li>
              ))}
            </ul>
          </section>
        )}
      </div>
    </div>
  );
}
