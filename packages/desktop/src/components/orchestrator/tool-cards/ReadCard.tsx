import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for Read tool calls.
 *
 * Shows the file path with a file icon, and optionally the file
 * content (from tool_result) in a collapsible pane.
 *
 * Agent-agnostic: works with any adapter that provides:
 *   - input.file_path / input.path
 *   - detail (file content from tool_result)
 */


import clsx from "clsx";
import { ChevronDown, ChevronRight, FileText, FolderOpen } from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";
import { ShikiPreview } from "./ShikiPreview";

// ── Helpers ─────────────────────────────────────────────────────────

/** Infer a language hint from the file extension for syntax context. */
function getLanguageHint(path: string): string | null {
  const ext = path.split(".").pop()?.toLowerCase();
  const map: Record<string, string> = {
    ts: "TypeScript",
    tsx: "TypeScript",
    js: "JavaScript",
    jsx: "JavaScript",
    rs: "Rust",
    py: "Python",
    go: "Go",
    rb: "Ruby",
    java: "Java",
    kt: "Kotlin",
    swift: "Swift",
    c: "C",
    cpp: "C++",
    h: "C/C++",
    css: "CSS",
    html: "HTML",
    json: "JSON",
    yaml: "YAML",
    yml: "YAML",
    toml: "TOML",
    md: "Markdown",
    sql: "SQL",
    sh: "Shell",
    bash: "Shell",
  };
  return ext ? map[ext] ?? null : null;
}

const CONTENT_TRUNCATION_LIMIT = 5000;

// ── Component ───────────────────────────────────────────────────────

export function ReadCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const input = event.input as Record<string, unknown> | undefined;
  const filePath = String(input?.file_path ?? input?.path ?? event.summary?.replace(/^Read:\s*/, "") ?? "");
  const content = event.detail ?? "";
  const hasContent = content.length > 0;

  // File metadata.
  const shortPath = filePath.split("/").pop() ?? filePath;
  const dirPath = filePath.includes("/") ? filePath.slice(0, filePath.lastIndexOf("/")) : "";
  const langHint = getLanguageHint(filePath);
  const lineCount = content ? content.split("\n").length : null;

  // Detect if this is a directory listing (Glob result or multi-file read).
  const isDirectory = filePath.endsWith("/") || event.tool === "Glob";

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const icon = isDirectory
      ? <FolderOpen size={11} className="shrink-0 text-gx-muted" />
      : <FileText size={11} className="shrink-0 text-gx-muted" />;
    const defaultSummary = (
      <>
        {icon}
        <span className="font-medium text-gx-text/80 truncate">{shortPath || "Read"}</span>
        {lineCount !== null && lineCount > 0 && (
          <span className="shrink-0 text-[9px] text-gx-muted tabular-nums">{lineCount}L</span>
        )}
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Read: ${shortPath}`}
      />
    );
  }

  return (
    <div className="rounded-md border border-gx-border/40 overflow-hidden">
      {/* Header */}
      <button
        type="button"
        onClick={() => hasContent && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          hasContent && "hover:bg-gx-surface-hover cursor-pointer",
          !hasContent && "cursor-default",
        )}
      >
        {isDirectory
          ? <FolderOpen size={11} className="text-gx-muted shrink-0" />
          : <FileText size={11} className="text-gx-muted shrink-0" />
        }
        <span className="shrink-0 font-medium text-gx-muted">Read</span>

        {/* File path */}
        <span className="min-w-0 flex-1 flex items-baseline gap-1 truncate">
          <span className="text-gx-text/90 font-medium truncate">{shortPath}</span>
          {dirPath && (
            <span className="text-[9px] text-gx-muted/60 truncate">{dirPath}</span>
          )}
        </span>

        {/* Metadata badges */}
        {langHint && (
          <span className="shrink-0 rounded bg-gx-surface-hover px-1 py-0.5 text-[8px] text-gx-muted border border-gx-border/50">
            {langHint}
          </span>
        )}
        {lineCount !== null && lineCount > 0 && (
          <span className="shrink-0 text-[9px] text-gx-muted tabular-nums">
            {lineCount}L
          </span>
        )}

        {hasContent && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-gx-muted/60" />
            : <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />
        )}
      </button>

      {/* Expanded: file content */}
      {expanded && hasContent && (
        <div className="border-t border-gx-border/30 max-h-[320px] overflow-auto">
          <ShikiPreview
            code={content.length > CONTENT_TRUNCATION_LIMIT
              ? content.slice(0, CONTENT_TRUNCATION_LIMIT) + "\n… (truncated)"
              : content}
            path={filePath}
          />
        </div>
      )}
    </div>
  );
}
