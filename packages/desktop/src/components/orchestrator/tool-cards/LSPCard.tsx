import { useCardExpanded } from "./useCardExpanded";
/**
 * Specialized renderer for LSP (Language Server Protocol) tool calls.
 *
 * Shows code intelligence operations: go-to-definition, find references,
 * type info, diagnostics, etc. Output is syntax-highlighted when it
 * contains JSON (common for LSP responses).
 */


import clsx from "clsx";
import { ChevronDown, ChevronRight, Braces, AlertTriangle, Info } from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";
import { ShikiPreview } from "./ShikiPreview";

// ── Component ───────────────────────────────────────────────────────

export function LSPCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);

  const input = event.input as Record<string, unknown> | undefined;
  const action = String(input?.action ?? input?.command ?? input?.method ?? "");
  const filePath = String(input?.file_path ?? input?.path ?? input?.uri ?? "");
  const symbol = String(input?.symbol ?? input?.name ?? "");
  const output = event.detail ?? "";
  const hasOutput = output.length > 0;

  // Determine the operation type for display.
  const operation = getOperationLabel(action);
  const shortPath = filePath.split("/").pop() ?? filePath;

  // Parse diagnostics from output if present.
  const diagnostics = parseDiagnostics(output);
  const hasDiagnostics = diagnostics.length > 0;

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const defaultSummary = (
      <>
        <Braces size={11} className="shrink-0 text-gx-muted" />
        <span className="font-medium text-gx-text/80">LSP</span>
        <span className="text-[9px] text-gx-muted">{operation}</span>
        {symbol && <code className="font-mono text-gx-text/70 truncate">{symbol}</code>}
        {!symbol && shortPath && <span className="truncate text-gx-text/70">{shortPath}</span>}
        {hasDiagnostics && (
          <span className="shrink-0 flex items-center gap-0.5 text-[9px] text-gx-warn">
            <AlertTriangle size={9} /> {diagnostics.length}
          </span>
        )}
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`LSP ${operation}: ${symbol || shortPath}`}
      />
    );
  }

  return (
    <div className="rounded-md border border-gx-border/40 overflow-hidden">
      <button
        type="button"
        onClick={() => (hasOutput || hasDiagnostics) && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-[10px] text-left transition",
          (hasOutput || hasDiagnostics) && "hover:bg-gx-surface-hover cursor-pointer",
          !(hasOutput || hasDiagnostics) && "cursor-default",
        )}
      >
        <Braces size={11} className="text-gx-muted shrink-0" />
        <span className="shrink-0 font-medium text-gx-muted">LSP</span>

        {/* Operation badge */}
        <span className="shrink-0 rounded bg-gx-surface-hover px-1 py-0.5 text-[8px] font-medium text-gx-muted border border-gx-border/50">
          {operation}
        </span>

        {/* Symbol or file */}
        <span className="min-w-0 flex-1 flex items-center gap-1.5 truncate">
          {symbol && (
            <code className="font-mono text-gx-text/80">{symbol}</code>
          )}
          {shortPath && (
            <span className="text-[9px] text-gx-muted/60 truncate">{shortPath}</span>
          )}
        </span>

        {/* Diagnostic count */}
        {hasDiagnostics && (
          <span className="shrink-0 flex items-center gap-0.5 text-[9px]">
            <AlertTriangle size={9} className="text-gx-warn" />
            <span className="text-gx-warn">{diagnostics.length}</span>
          </span>
        )}

        {(hasOutput || hasDiagnostics) && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-gx-muted/60" />
            : <ChevronRight size={10} className="shrink-0 text-gx-muted/60" />
        )}
      </button>

      {expanded && (hasOutput || hasDiagnostics) && (
        <div className="border-t border-gx-border/30 max-h-[250px] overflow-auto">
          {/* Diagnostics list */}
          {hasDiagnostics && (
            <div className="px-2.5 py-2 space-y-1">
              {diagnostics.map((diag, idx) => (
                <div key={idx} className="flex items-start gap-1.5 text-[10px]">
                  <DiagnosticIcon severity={diag.severity} />
                  <span className="flex-1 text-gx-text/70">{diag.message}</span>
                  {diag.line && (
                    <span className="shrink-0 text-[9px] text-gx-muted font-mono">L{diag.line}</span>
                  )}
                </div>
              ))}
            </div>
          )}

          {/* Raw output */}
          {hasOutput && !hasDiagnostics && (
            <LspOutput output={output} />
          )}
        </div>
      )}
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────

function getOperationLabel(action: string): string {
  const lower = action.toLowerCase();
  if (lower.includes("definition") || lower.includes("goto")) return "go-to-def";
  if (lower.includes("reference")) return "references";
  if (lower.includes("hover") || lower.includes("type")) return "type-info";
  if (lower.includes("diagnostic") || lower.includes("error")) return "diagnostics";
  if (lower.includes("symbol")) return "symbols";
  if (lower.includes("implementation")) return "impl";
  if (lower.includes("call")) return "call-hierarchy";
  if (lower.includes("completion")) return "completions";
  return action || "query";
}

interface Diagnostic {
  severity: "error" | "warning" | "info";
  message: string;
  line?: number;
}

function parseDiagnostics(output: string): Diagnostic[] {
  if (!output.trim()) return [];

  const diagnostics: Diagnostic[] = [];
  const lines = output.split("\n");

  for (const line of lines) {
    // Match common diagnostic formats: "file:line:col: severity: message"
    const match = line.match(/(?::(\d+)(?::\d+)?)?:\s*(error|warning|warn|info|hint):\s*(.+)/i);
    if (match) {
      const severity = match[2].toLowerCase().startsWith("error") ? "error"
        : match[2].toLowerCase().startsWith("warn") ? "warning"
        : "info";
      diagnostics.push({
        severity,
        message: match[3].trim(),
        line: match[1] ? Number(match[1]) : undefined,
      });
    }
  }

  return diagnostics;
}

function DiagnosticIcon({ severity }: { severity: Diagnostic["severity"] }) {
  switch (severity) {
    case "error":
      return <AlertTriangle size={10} className="text-gx-error shrink-0 mt-0.5" />;
    case "warning":
      return <AlertTriangle size={10} className="text-gx-warn shrink-0 mt-0.5" />;
    default:
      return <Info size={10} className="text-gx-info shrink-0 mt-0.5" />;
  }
}

// ── LSP output renderer ─────────────────────────────────────────────

const LSP_OUTPUT_LIMIT = 3000;

function LspOutput({ output }: { output: string }) {
  const text = output.length > LSP_OUTPUT_LIMIT
    ? output.slice(0, LSP_OUTPUT_LIMIT) + "\n… (truncated)"
    : output;

  // LSP responses are typically JSON.
  const trimmed = text.trimStart();
  if (trimmed.startsWith("{") || trimmed.startsWith("[")) {
    try {
      const parsed = JSON.parse(output.length > LSP_OUTPUT_LIMIT ? output : trimmed);
      const pretty = JSON.stringify(parsed, null, 2);
      const display = pretty.length > LSP_OUTPUT_LIMIT
        ? pretty.slice(0, LSP_OUTPUT_LIMIT) + "\n… (truncated)"
        : pretty;
      return <ShikiPreview code={display} lang="json" className="[&_pre]:!py-2 [&_pre]:!px-2.5" />;
    } catch {
      // Partial JSON — still try to highlight.
      return <ShikiPreview code={text} lang="json" className="[&_pre]:!py-2 [&_pre]:!px-2.5" />;
    }
  }

  return (
    <pre className="px-2.5 py-2 font-mono text-[10px] leading-[1.5] text-gx-text/60 whitespace-pre-wrap">
      {text}
    </pre>
  );
}
