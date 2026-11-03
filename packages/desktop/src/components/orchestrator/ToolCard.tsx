import { useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  FileText,
  FilePen,
  Globe,
  ListTodo,
  Search,
  Terminal,
  Wrench,
} from "lucide-react";
import { ShikiPreview } from "./tool-cards/ShikiPreview";

// ── Types ───────────────────────────────────────────────────────────

export interface ToolCardProps {
  /** Tool name from the adapter (Read, Write, Edit, Bash, etc.) */
  tool: string;
  /** One-line summary (relative path, command, URL, etc.) */
  summary: string;
  /** Full tool input (shown on expand) */
  input?: unknown;
  /** Tool result content (attached when tool_result arrives) */
  detail?: string;
  /** Whether the card starts expanded */
  defaultExpanded?: boolean;
}

// ── Icon mapping ────────────────────────────────────────────────────

const DETAIL_TRUNCATION_LIMIT = 5000;

type IconConfig = { icon: React.ReactNode; label: string };

function getToolIcon(tool: string): IconConfig {
  switch (tool) {
    case "Read":
      return { icon: <FileText size={11} className="text-gx-muted" />, label: "Read" };
    case "Write":
    case "Edit":
    case "NotebookEdit":
      return { icon: <FilePen size={11} className="text-gx-muted" />, label: tool };
    case "Bash":
    case "Terminal":
      return { icon: <Terminal size={11} className="text-gx-muted" />, label: tool };
    case "Grep":
    case "Glob":
      return { icon: <Search size={11} className="text-gx-muted" />, label: tool };
    case "WebFetch":
      return { icon: <Globe size={11} className="text-gx-muted" />, label: "WebFetch" };
    case "TodoWrite":
    case "Task":
    case "TaskOutput":
    case "TaskStop":
      return { icon: <ListTodo size={11} className="text-gx-muted" />, label: tool };
    default:
      return { icon: <Wrench size={11} className="text-gx-muted" />, label: tool };
  }
}

function truncateDetail(detail: string): string {
  if (detail.length <= DETAIL_TRUNCATION_LIMIT) return detail;
  return detail.slice(0, DETAIL_TRUNCATION_LIMIT) + "\n… (truncated)";
}

// ── Component ───────────────────────────────────────────────────────

export function ToolCard({ tool, summary, input, detail, defaultExpanded = false }: ToolCardProps) {
  const [expanded, setExpanded] = useState(defaultExpanded);

  const { icon, label } = getToolIcon(tool);
  const hasExpandableContent = input != null || !!detail;

  return (
    <div className="rounded-md border border-gx-border/40 overflow-hidden">
      <button
        type="button"
        onClick={() => hasExpandableContent && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          hasExpandableContent && "hover:bg-gx-surface-hover cursor-pointer",
          !hasExpandableContent && "cursor-default",
        )}
        aria-expanded={hasExpandableContent ? expanded : undefined}
      >
        {icon}
        <span className="shrink-0 font-medium text-gx-muted">{label}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-gx-text/80">{summary}</span>
        {hasExpandableContent && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-gx-muted/60" />
            : <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />
        )}
      </button>
      {expanded && hasExpandableContent && (
        <div className="border-t border-gx-border/30 space-y-0">
          {input != null && (
            <div className="max-h-[200px] overflow-auto">
              <ShikiPreview
                code={JSON.stringify(input, null, 2)}
                lang="json"
                className="[&_pre]:!py-2 [&_pre]:!px-2.5"
              />
            </div>
          )}
          {detail && (
            <div className={clsx(input != null && "border-t border-gx-border/20", "max-h-[200px] overflow-auto")}>
              <GenericDetailRenderer detail={detail} />
            </div>
          )}
        </div>
      )}
    </div>
  );
}

// ── Detail renderer ─────────────────────────────────────────────────

/**
 * Renders tool detail content with appropriate formatting:
 *   - JSON → syntax-highlighted
 *   - Otherwise → plain monospace (truncated)
 */
function GenericDetailRenderer({ detail }: { detail: string }) {
  const text = truncateDetail(detail);
  const trimmed = text.trimStart();

  // JSON detection.
  if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
    try {
      const parsed = JSON.parse(detail);
      const pretty = JSON.stringify(parsed, null, 2);
      const display = pretty.length > DETAIL_TRUNCATION_LIMIT
        ? pretty.slice(0, DETAIL_TRUNCATION_LIMIT) + "\n… (truncated)"
        : pretty;
      return <ShikiPreview code={display} lang="json" className="[&_pre]:!py-2 [&_pre]:!px-2.5" />;
    } catch {
      // Partial JSON — still highlight.
      return <ShikiPreview code={text} lang="json" className="[&_pre]:!py-2 [&_pre]:!px-2.5" />;
    }
  }

  return (
    <pre className="whitespace-pre-wrap font-mono text-[10px] text-gx-text/70 px-2.5 py-2">
      {text}
    </pre>
  );
}
