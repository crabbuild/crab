import { useMemo } from "react";
import { ChevronRight, FolderOpen } from "lucide-react";

export interface TerminalBreadcrumbBarProps {
  /** Full working directory path (e.g. "/Users/dev/projects/crab"). */
  cwd: string;
  /** Callback when a path segment is clicked — receives the full path up to that segment. */
  onNavigate: (path: string) => void;
}

/**
 * Compact breadcrumb bar displayed above the terminal viewport showing
 * the tracked working directory. Each segment is clickable to navigate
 * (cd) to that parent directory.
 */
export function TerminalBreadcrumbBar({ cwd, onNavigate }: TerminalBreadcrumbBarProps) {
  const segments = useMemo(() => {
    const parts = cwd.split("/").filter(Boolean);
    return parts.map((part, idx) => ({
      label: part,
      path: "/" + parts.slice(0, idx + 1).join("/"),
    }));
  }, [cwd]);

  if (segments.length === 0) return null;

  return (
    <nav
      className="flex shrink-0 items-center gap-0.5 overflow-x-auto border-b border-border bg-background px-2 py-0.5 text-[10px]"
      aria-label="Terminal working directory"
    >
      <FolderOpen size={11} className="shrink-0 text-muted-foreground mr-0.5" />
      {/* Root slash */}
      <button
        type="button"
        className="rounded px-0.5 py-0.5 text-muted-foreground hover:bg-accent hover:text-foreground transition"
        title="/"
        onClick={() => onNavigate("/")}
      >
        /
      </button>
      {segments.map((seg, idx) => (
        <span key={seg.path} className="flex items-center gap-0.5">
          {idx > 0 && (
            <ChevronRight size={9} className="shrink-0 text-muted-foreground/60" />
          )}
          <button
            type="button"
            className="rounded px-0.5 py-0.5 text-muted-foreground hover:bg-accent hover:text-foreground transition truncate max-w-[140px]"
            title={seg.path}
            onClick={() => onNavigate(seg.path)}
          >
            {seg.label}
          </button>
        </span>
      ))}
    </nav>
  );
}
