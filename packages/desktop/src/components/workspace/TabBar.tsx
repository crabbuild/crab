import { useCallback, useEffect } from "react";
import clsx from "clsx";
import { X } from "lucide-react";
import type { WorkspaceRepoEntry } from "../../state";

export interface TabBarProps {
  tabs: WorkspaceRepoEntry[];
  activeRelativePath: string | null;
  onSelect: (relativePath: string) => void;
  onClose: (relativePath: string) => void;
}

/** Truncate a string to roughly 20 characters with ellipsis. */
function truncateName(name: string, max = 20): string {
  if (name.length <= max) return name;
  return name.slice(0, max - 1) + "…";
}

/**
 * Thin horizontal tab strip showing recently-activated workspace repos.
 * Max 9 tabs; LRU eviction is handled by the reducer on `tab-bar-activate`.
 *
 * Keyboard shortcuts ⌘1–⌘9 are wired externally (see useTabBarShortcuts).
 */
export function TabBar({ tabs, activeRelativePath, onSelect, onClose }: TabBarProps) {
  // Middle-click to close a tab.
  const handleAuxClick = useCallback(
    (e: React.MouseEvent, relativePath: string) => {
      if (e.button === 1) {
        e.preventDefault();
        onClose(relativePath);
      }
    },
    [onClose],
  );

  if (tabs.length === 0) return null;

  return (
    <div
      className="flex h-[30px] shrink-0 items-center gap-px overflow-x-auto border-b border-gx-border bg-gx-bg-sub px-1"
      role="tablist"
      aria-label="Workspace repo tabs"
    >
      {tabs.map((entry) => {
        const isActive = entry.relativePath === activeRelativePath;
        const isDirty = (entry.dirtyCount ?? 0) > 0;

        return (
          <div
            key={entry.relativePath}
            role="tab"
            aria-selected={isActive}
            className={clsx(
              "group relative flex h-[26px] max-w-[180px] shrink-0 cursor-pointer items-center gap-1 rounded px-2 text-xs transition select-none",
              isActive
                ? "bg-gx-bg text-gx-text shadow-sm"
                : "text-gx-muted hover:bg-gx-surface-hover hover:text-gx-text",
            )}
            onClick={() => onSelect(entry.relativePath)}
            onAuxClick={(e) => handleAuxClick(e, entry.relativePath)}
            title={entry.relativePath}
          >
            {/* Repo name */}
            <span className="truncate">{truncateName(entry.name)}</span>

            {/* Dirty indicator dot */}
            {isDirty && (
              <span
                className="h-2 w-2 shrink-0 rounded-full bg-orange-400"
                title={`${entry.dirtyCount} uncommitted change${entry.dirtyCount === 1 ? "" : "s"}`}
              />
            )}

            {/* Close button — visible on hover */}
            <button
              type="button"
              className={clsx(
                "ml-0.5 shrink-0 rounded p-0.5 transition",
                "opacity-0 group-hover:opacity-100",
                "hover:bg-gx-surface-hover hover:text-gx-error",
              )}
              onClick={(e) => {
                e.stopPropagation();
                onClose(entry.relativePath);
              }}
              aria-label={`Close ${entry.name} tab`}
              title="Close tab"
            >
              <X size={11} />
            </button>
          </div>
        );
      })}
    </div>
  );
}

/**
 * Hook that registers ⌘1–⌘9 keyboard shortcuts for tab positions.
 * Only active when the workspace is open (tabs array is non-empty).
 */
export function useTabBarShortcuts(
  tabs: WorkspaceRepoEntry[],
  onSelect: (relativePath: string) => void,
  enabled: boolean,
) {
  useEffect(() => {
    if (!enabled || tabs.length === 0) return;

    const handler = (e: KeyboardEvent) => {
      if (!e.metaKey || e.shiftKey || e.altKey || e.ctrlKey) return;
      const num = parseInt(e.key, 10);
      if (num < 1 || num > 9 || isNaN(num)) return;

      const index = num - 1;
      if (index >= tabs.length) return;

      e.preventDefault();
      onSelect(tabs[index].relativePath);
    };

    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [tabs, onSelect, enabled]);
}
