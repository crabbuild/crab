import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for MCP (Model Context Protocol) tool calls.
 *
 * The card has two layers:
 *
 *   1. A generic header that surfaces server name, tool name, status,
 *      and duration — the universal MCP shape from
 *      `mcp_tool_call.{started,completed,progress}`.
 *
 *   2. An optional server-aware mini-card on top of the generic body,
 *      keyed by `(server, tool)`. Today we recognise GitHub and Linear
 *      well enough to render their results inline; unknown servers
 *      fall back to the generic JSON pretty-print.
 *
 * The server-aware fast paths are deliberately small. They exist to
 * make the chat surface feel like the official integration (instead
 * of "JSON blob in a code block") for the most common cases.
 */


import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  Plug,
  Database,
  ExternalLink,
  GitBranch,
  Loader2,
  CheckCircle2,
  XCircle,
} from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";
import { ShikiPreview } from "./ShikiPreview";

const OUTPUT_TRUNCATION_LIMIT = 3000;

// ── Component ───────────────────────────────────────────────────────

export function McpCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const tool = event.tool ?? "";
  const input = event.input as Record<string, unknown> | undefined;
  const output = event.detail ?? "";
  const hasOutput = output.length > 0;

  const isList = tool === "ListMcpResources" || tool === "ListMcpResourcesTool";
  const isRead = tool === "ReadMcpResource" || tool === "ReadMcpResourceTool";

  const server = String(input?.server ?? input?.server_name ?? "");
  const status = String(input?.status ?? (hasOutput ? "completed" : "in_progress"));
  const durationMs = typeof input?.durationMs === "number" ? (input.durationMs as number) : null;
  const isProgressUpdate = input?.progress === true;

  const Icon = isList ? Database : Plug;
  const label = isList ? "MCP List" : isRead ? "MCP Read" : "MCP";

  const uri = String(input?.uri ?? input?.resource_uri ?? "");
  const summary = uri || tool || (isList ? "listing resources" : "");

  const fastCard = renderFastCard(server, tool, input, output);

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const statusIcon = status === "in_progress"
      ? <Loader2 size={11} className="shrink-0 text-info animate-spin" />
      : status === "failed"
        ? <XCircle size={11} className="shrink-0 text-destructive" />
        : <CheckCircle2 size={11} className="shrink-0 text-success" />;
    const defaultSummary = (
      <>
        {statusIcon}
        <span className="font-medium text-foreground/80">{label}</span>
        {server && (
          <span className="text-[9px] text-muted-foreground">[{server}]</span>
        )}
        <span className="truncate text-foreground/70 font-mono">{summary}</span>
        {durationMs !== null && (
          <span className="shrink-0 text-[9px] font-mono text-muted-foreground">{(durationMs / 1000).toFixed(2)}s</span>
        )}
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`MCP: ${server ? server + " " : ""}${summary}`}
      />
    );
  }

  return (
    <div
      className={clsx(
        "rounded-md border overflow-hidden transition-colors",
        status === "in_progress" ? "border-info/30 bg-info/5" :
        status === "failed"      ? "border-destructive/30 bg-destructive/5" :
                                   "border-border/40",
      )}
    >
      <button
        type="button"
        onClick={() => (hasOutput || fastCard) && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          (hasOutput || fastCard) && "hover:bg-accent cursor-pointer",
          !hasOutput && !fastCard && "cursor-default",
        )}
      >
        <Icon size={11} className="text-muted-foreground shrink-0" />
        {server && (
          <span className="shrink-0 rounded border border-border/40 bg-card px-1 py-0.5 text-[8px] font-medium text-muted-foreground">
            {server}
          </span>
        )}
        <span className="shrink-0 font-medium text-muted-foreground">{label}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-foreground/80">{summary}</span>

        {durationMs !== null && (
          <span className="shrink-0 rounded px-1 py-0.5 text-[8px] font-mono text-muted-foreground border border-border/40">
            {(durationMs / 1000).toFixed(2)}s
          </span>
        )}

        {/* Status pill */}
        {status === "in_progress" && (
          <span className="shrink-0 inline-flex items-center gap-0.5 text-[8px] text-info">
            <Loader2 size={9} className="animate-spin" />
            running
          </span>
        )}
        {status === "failed" && (
          <span className="shrink-0 inline-flex items-center gap-0.5 text-[8px] text-destructive">
            <XCircle size={9} />
            failed
          </span>
        )}
        {status === "completed" && !isProgressUpdate && (
          <CheckCircle2 size={10} className="shrink-0 text-success" />
        )}

        {(hasOutput || fastCard) && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
            : <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
        )}
      </button>

      {expanded && (hasOutput || fastCard) && (
        <div className="border-t border-border/30 max-h-[260px] overflow-auto">
          {fastCard}
          {!fastCard && hasOutput && (
            <McpOutputRenderer output={output} />
          )}
        </div>
      )}
    </div>
  );
}

// ── MCP output renderer ─────────────────────────────────────────────

/**
 * Intelligently renders MCP tool output:
 *   - JSON → syntax-highlighted with Shiki
 *   - Markdown-like → could be rendered rich, but we keep it as
 *     highlighted code since MCP results are typically structured
 *   - Plain text → monospace fallback
 */
function McpOutputRenderer({ output }: { output: string }) {
  const text = output.length > OUTPUT_TRUNCATION_LIMIT
    ? output.slice(0, OUTPUT_TRUNCATION_LIMIT) + "\n… (truncated)"
    : output;

  // Detect JSON output (very common for MCP tools).
  const trimmed = text.trimStart();
  if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
    // Try to pretty-print if it's valid JSON.
    try {
      const parsed = JSON.parse(trimmed.length < output.length ? output : trimmed);
      const pretty = JSON.stringify(parsed, null, 2);
      const displayJson = pretty.length > OUTPUT_TRUNCATION_LIMIT
        ? pretty.slice(0, OUTPUT_TRUNCATION_LIMIT) + "\n… (truncated)"
        : pretty;
      return <ShikiPreview code={displayJson} lang="json" />;
    } catch {
      // Not valid JSON — render with json lang anyway for partial highlighting.
      return <ShikiPreview code={text} lang="json" />;
    }
  }

  // Fallback: plain monospace.
  return (
    <pre className="px-2.5 py-2 font-mono text-[10px] leading-[1.5] text-foreground/60 whitespace-pre-wrap">
      {text}
    </pre>
  );
}

// ── Server-aware fast paths ─────────────────────────────────────────

/**
 * Returns a React node for known server/tool combinations, or `null`
 * to fall back to the generic JSON output. We keep this deliberately
 * narrow — adding more cases is a one-line addition once we have a
 * real result fixture for the new server.
 */
function renderFastCard(
  server: string,
  tool: string,
  input: Record<string, unknown> | undefined,
  output: string,
): React.ReactNode | null {
  const args = (input?.arguments ?? input) as Record<string, unknown> | undefined;

  // GitHub MCP — issue / PR creation responses
  if (server === "github" && (tool === "create_issue" || tool === "create_pull_request" || tool === "create_pr")) {
    const result = parseJsonish(output);
    const url = pickString(result, ["html_url", "url"]);
    const number = pickNumber(result, ["number"]);
    const title = pickString(result, ["title"]) ?? pickString(args, ["title"]) ?? "(no title)";
    const body = pickString(result, ["body"]) ?? pickString(args, ["body"]) ?? "";
    return (
      <div className="px-3 py-2">
        <div className="flex items-center gap-2 text-[10px]">
          <GitBranch size={11} className="text-muted-foreground" />
          <span className="font-medium text-foreground/90 truncate">{title}</span>
          {number !== null && (
            <span className="rounded bg-primary/10 px-1 py-0.5 text-[9px] font-mono text-primary border border-primary/20">
              #{number}
            </span>
          )}
          {url && (
            <a
              href={url}
              target="_blank"
              rel="noopener noreferrer"
              className="ml-auto inline-flex items-center gap-0.5 text-[9px] text-primary hover:text-primary-hover"
            >
              Open <ExternalLink size={9} />
            </a>
          )}
        </div>
        {body && (
          <p className="mt-1 text-[10px] text-foreground/70 line-clamp-3 whitespace-pre-wrap">{body}</p>
        )}
      </div>
    );
  }

  // GitHub MCP — file get / search
  if (server === "github" && (tool === "get_file_contents" || tool === "search_code")) {
    const path = pickString(args, ["path", "query"]);
    const ref = pickString(args, ["ref", "branch"]);
    return (
      <div className="px-3 py-2">
        <div className="flex items-center gap-2 text-[10px]">
          <GitBranch size={11} className="text-muted-foreground" />
          <span className="font-mono text-foreground/90 truncate">{path ?? tool}</span>
          {ref && (
            <span className="inline-flex items-center gap-0.5 rounded bg-card px-1 py-0.5 text-[9px] text-muted-foreground border border-border/40">
              <GitBranch size={9} /> {ref}
            </span>
          )}
        </div>
        {output && (
          <div className="mt-1 max-h-[200px] overflow-auto">
            <ShikiPreview
              code={output.length > 800 ? output.slice(0, 800) + "\n… (truncated)" : output}
              path={path ?? undefined}
            />
          </div>
        )}
      </div>
    );
  }

  // Linear MCP — issue creation / fetch
  if (server === "linear" && (tool === "create_issue" || tool === "get_issue" || tool === "update_issue")) {
    const result = parseJsonish(output);
    const identifier = pickString(result, ["identifier"]);
    const title = pickString(result, ["title"]) ?? pickString(args, ["title"]);
    const url = pickString(result, ["url"]);
    return (
      <div className="px-3 py-2">
        <div className="flex items-center gap-2 text-[10px]">
          <span className="rounded bg-primary/10 px-1 py-0.5 text-[9px] font-mono text-primary border border-primary/20">
            {identifier ?? "linear"}
          </span>
          <span className="font-medium text-foreground/90 truncate">{title ?? "(no title)"}</span>
          {url && (
            <a
              href={url}
              target="_blank"
              rel="noopener noreferrer"
              className="ml-auto inline-flex items-center gap-0.5 text-[9px] text-primary hover:text-primary-hover"
            >
              Open <ExternalLink size={9} />
            </a>
          )}
        </div>
      </div>
    );
  }

  return null;
}

// ── Helpers ─────────────────────────────────────────────────────────

function parseJsonish(text: string): Record<string, unknown> | null {
  if (!text) return null;
  try {
    const parsed = JSON.parse(text);
    return typeof parsed === "object" && parsed !== null ? (parsed as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

function pickString(obj: Record<string, unknown> | undefined | null, keys: string[]): string | null {
  if (!obj) return null;
  for (const k of keys) {
    const v = obj[k];
    if (typeof v === "string" && v) return v;
  }
  return null;
}

function pickNumber(obj: Record<string, unknown> | undefined | null, keys: string[]): number | null {
  if (!obj) return null;
  for (const k of keys) {
    const v = obj[k];
    if (typeof v === "number") return v;
  }
  return null;
}
