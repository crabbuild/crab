/**
 * Specialized renderer for Bash/Terminal/command_execution tool calls.
 *
 * Shows the command in a terminal-style header with the output
 * (stdout/stderr) in a scrollable monospace pane below. Supports
 * the Codex streaming pattern where the command appears first
 * (tool_use) and output arrives later (tool_result → detail).
 *
 * When the command is a file-reading operation (cat, head, tail, sed,
 * etc.), the output is rendered with Shiki syntax highlighting based
 * on the target file's extension — giving the same rich code preview
 * as the ReadCard.
 *
 * Agent-agnostic: works with any adapter that provides:
 *   - input.command (the shell command)
 *   - detail (stdout/stderr output from tool_result)
 */

import { useState } from "react";
import clsx from "clsx";
import {
  ChevronDown,
  ChevronRight,
  Terminal,
  Copy,
  Check,
  Loader2,
  CheckCircle2,
  XCircle,
} from "lucide-react";
import type { ToolCardRendererProps } from "./types";
import { CollapsedCardLine } from "./CollapsedCardLine";
import { ShikiPreview } from "./ShikiPreview";
import { useCardExpanded } from "./useCardExpanded";
import { languageForPath, isUnifiedDiff } from "../../../lib/codex-diff";
import { AnsiText, stripAnsiToText } from "../../../lib/ansi-text";
import { summariseCommand } from "../../../lib/command-summaries";
import { UnifiedDiff } from "../UnifiedDiff";

// ── Helpers ─────────────────────────────────────────────────────────

const OUTPUT_TRUNCATION_LIMIT = 8000;

function truncateOutput(output: string): { text: string; truncated: boolean } {
  if (output.length <= OUTPUT_TRUNCATION_LIMIT) return { text: output, truncated: false };
  return {
    text: output.slice(0, OUTPUT_TRUNCATION_LIMIT),
    truncated: true,
  };
}

/**
 * Extracts exit code from the output text. Codex appends "Exit code: N"
 * at the end; Claude Code may use other patterns.
 */
function extractExitCode(output: string): number | null {
  const match = output.match(/(?:exit code|Exit Code|ExitCode)[:\s]+(\d+)\s*$/m);
  return match ? Number(match[1]) : null;
}

/**
 * Extracts duration from the output text (appended by the adapter).
 */
function extractDuration(output: string): string | null {
  const match = output.match(/Duration:\s*([\d.]+s)\s*$/m);
  return match ? match[1] : null;
}

/**
 * Shortens a command for display in the collapsed header.
 * Strips common shell invocation prefixes and trims to a reasonable length.
 */
function formatCommandSummary(command: string): string {
  // Strip common shell wrappers: `/bin/zsh -lc "..."` → inner command
  const shellWrapped = command.match(/^(?:\/bin\/(?:zsh|bash|sh)|bash|sh|zsh)\s+(?:-\w+\s+)*["'](.+)["']$/s);
  if (shellWrapped) return shellWrapped[1];
  return command;
}

/**
 * Renders Codex's `commandActions` parse into a friendly intent line.
 * Codex emits a best-effort breakdown of what a command will do; we
 * surface the most informative subset (read/write/git/network) so
 * the header reads like prose instead of raw argv.
 *
 * Falls back to `null` when there's no useful summary.
 */
function summariseCommandActions(actions: unknown): string | null {
  if (!Array.isArray(actions) || actions.length === 0) return null;
  const parts: string[] = [];
  for (const a of actions) {
    if (!a || typeof a !== "object") continue;
    const action = a as Record<string, unknown>;
    const t = String(action.type ?? "");
    if (t === "read" && action.path) parts.push(`read ${shorten(String(action.path))}`);
    else if (t === "write" && action.path) parts.push(`write ${shorten(String(action.path))}`);
    else if (t === "delete" && action.path) parts.push(`delete ${shorten(String(action.path))}`);
    else if (t === "git_diff") {
      const refs = Array.isArray(action.refs) ? (action.refs as string[]).join("..") : "";
      parts.push(refs ? `git diff ${refs}` : "git diff");
    } else if (t === "git_log") parts.push("git log");
    else if (t === "git_status") parts.push("git status");
    else if (t === "network" && action.url) parts.push(`fetch ${shorten(String(action.url), 40)}`);
    else if (t === "test") parts.push("run tests");
    else if (t === "build") parts.push("build");
    if (parts.length >= 3) break;
  }
  if (parts.length === 0) return null;
  return parts.join(" • ");
}

function shorten(s: string, max = 32): string {
  return s.length <= max ? s : s.slice(0, max - 1) + "…";
}

/**
 * Attempts to extract a file path from a command that reads/displays
 * file content. Returns the path if the command is a file-reading
 * operation, or null if it's a general shell command.
 *
 * Recognised patterns:
 *   cat foo.ts, head -n 20 foo.rs, tail -f log.txt,
 *   sed 's/x/y/' foo.ts, sed -i '' 's/x/y/' foo.ts,
 *   less foo.py, bat foo.rs, etc.
 */
function extractFilePathFromCommand(command: string): string | null {
  // Normalise: strip shell wrappers first.
  let cmd = command.trim();
  const shellWrapped = cmd.match(/^(?:\/bin\/(?:zsh|bash|sh)|bash|sh|zsh)\s+(?:-\w+\s+)*["'](.+)["']$/s);
  if (shellWrapped) cmd = shellWrapped[1].trim();

  // Commands that output file content.
  // Pattern: <command> [flags...] <filepath>
  // The filepath is the last non-flag argument.
  const FILE_READ_COMMANDS = /^(?:cat|head|tail|less|more|bat|batcat|sed|awk|perl|sort|uniq|nl|tac|rev|cut|paste|expand|unexpand|fold|fmt|pr|column)\b/;

  if (!FILE_READ_COMMANDS.test(cmd)) return null;

  // Split into tokens (basic shell tokenisation — handles simple quoting).
  const tokens = shellTokenise(cmd);
  if (tokens.length < 2) return null;

  // Walk backwards from the end to find the last argument that looks
  // like a file path (has an extension or a path separator).
  for (let i = tokens.length - 1; i >= 1; i--) {
    const tok = tokens[i];
    // Skip flags.
    if (tok.startsWith("-")) continue;
    // Skip sed/awk expressions (contain /, s, {, etc. but no extension).
    if (/^['"]?s[\/|#@]/.test(tok)) continue;
    if (/^['"]?\{/.test(tok)) continue;
    // Must look like a path: has a dot-extension or contains /
    if (/\.\w+$/.test(tok) || tok.includes("/")) {
      // Strip surrounding quotes.
      return tok.replace(/^['"]|['"]$/g, "");
    }
  }
  return null;
}

/**
 * Minimal shell tokeniser — splits on unquoted whitespace, respects
 * single and double quotes. Good enough for extracting file paths
 * from typical agent commands.
 */
function shellTokenise(cmd: string): string[] {
  const tokens: string[] = [];
  let current = "";
  let inSingle = false;
  let inDouble = false;

  for (let i = 0; i < cmd.length; i++) {
    const ch = cmd[i];
    if (ch === "'" && !inDouble) { inSingle = !inSingle; continue; }
    if (ch === '"' && !inSingle) { inDouble = !inDouble; continue; }
    if (ch === "\\" && i + 1 < cmd.length && (inDouble || (!inSingle && !inDouble))) {
      current += cmd[++i];
      continue;
    }
    if (/\s/.test(ch) && !inSingle && !inDouble) {
      if (current) { tokens.push(current); current = ""; }
      continue;
    }
    current += ch;
  }
  if (current) tokens.push(current);
  return tokens;
}

/**
 * Determines whether the command output should be syntax-highlighted.
 * Returns the Shiki language id if yes, null otherwise.
 */
function detectOutputLanguage(command: string): string | null {
  const filePath = extractFilePathFromCommand(command);
  if (!filePath) return null;
  const lang = languageForPath(filePath);
  return lang !== "plaintext" ? lang : null;
}

// ── Component ───────────────────────────────────────────────────────

export function BashCard({ event, defaultExpanded = false, collapsed: collapsedProp, onToggleCollapse, collapsedSummary }: ToolCardRendererProps) {
  const [expanded, setExpanded] = useCardExpanded(collapsedProp, defaultExpanded);
  const [copied, setCopied] = useState(false);

  const input = event.input as Record<string, unknown> | undefined;
  const rawCommand = String(input?.command ?? event.summary?.replace(/^(Bash|Terminal):\s*/, "") ?? "");
  const displayCommand = stripAnsiToText(formatCommandSummary(rawCommand));
  const actionSummary = summariseCommandActions(input?.commandActions);
  const cwd = typeof input?.cwd === "string" ? (input.cwd as string) : "";
  const output = event.detail ?? "";
  const hasOutput = output.length > 0;

  // Strip ANSI from the text we feed into the exit-code / duration parsers
  // so escape sequences that wrap the trailing "Exit code: 1" line don't
  // hide it from the regex.
  const plainOutput = hasOutput ? stripAnsiToText(output) : "";

  // Determine execution state.
  const exitCode = hasOutput ? extractExitCode(plainOutput) : null;
  const duration = hasOutput ? extractDuration(plainOutput) : null;
  const isRunning = !hasOutput && !event.detail;
  const isError = exitCode !== null && exitCode !== 0;
  const isSuccess = hasOutput && (exitCode === null || exitCode === 0);

  // Smart command summary badge (CHAT7.1–7.7). Best-effort; null means
  // no parser matched and the raw output renderer handles it.
  const commandSummary = hasOutput ? summariseCommand(rawCommand, plainOutput) : null;

  const handleCopy = () => {
    navigator.clipboard.writeText(rawCommand).catch(() => {});
    setCopied(true);
    setTimeout(() => setCopied(false), 1500);
  };

  // Collapsed one-liner branch.
  const collapsed = collapsedProp ?? false;
  if (collapsed) {
    const statusIcon = isRunning
      ? <Loader2 size={11} className="shrink-0 text-info animate-spin" />
      : isError
        ? <XCircle size={11} className="shrink-0 text-destructive" />
        : isSuccess
          ? <CheckCircle2 size={11} className="shrink-0 text-success" />
          : <Terminal size={11} className="shrink-0 text-muted-foreground" />;
    const defaultSummary = (
      <>
        {statusIcon}
        <code className="font-mono text-foreground/80 truncate">{displayCommand}</code>
        {duration && (
          <span className="shrink-0 text-[9px] font-mono text-muted-foreground">{duration}</span>
        )}
        {exitCode !== null && exitCode !== 0 && (
          <span className="shrink-0 text-[9px] font-mono text-destructive">exit {exitCode}</span>
        )}
      </>
    );
    return (
      <CollapsedCardLine
        summary={collapsedSummary ?? defaultSummary}
        onExpand={() => onToggleCollapse?.(false)}
        ariaLabel={`Bash: ${displayCommand}`}
      />
    );
  }

  return (
    <div
      className={clsx(
        "rounded-md border overflow-hidden transition-colors",
        isError
          ? "border-destructive/30"
          : isRunning
            ? "border-info/30"
            : "border-border/40",
      )}
    >
      {/* Header: command line */}
      <button
        type="button"
        onClick={() => hasOutput && setExpanded(!expanded)}
        className={clsx(
          "flex w-full items-center gap-2 px-2.5 py-1.5 text-left transition group",
          hasOutput && "hover:bg-accent cursor-pointer",
          !hasOutput && "cursor-default",
        )}
        aria-expanded={hasOutput ? expanded : undefined}
      >
        {/* Status icon */}
        {isRunning ? (
          <Loader2 size={11} className="shrink-0 text-info animate-spin" />
        ) : isError ? (
          <XCircle size={11} className="shrink-0 text-destructive" />
        ) : isSuccess ? (
          <CheckCircle2 size={11} className="shrink-0 text-success" />
        ) : (
          <Terminal size={11} className="shrink-0 text-muted-foreground" />
        )}

        {/* Command */}
        <code className="min-w-0 flex-1 truncate font-mono text-[10px] text-foreground/80">
          {displayCommand}
        </code>

        {/* Codex commandActions intent summary (compact, non-essential) */}
        {actionSummary && (
          <span
            className="shrink-0 hidden md:inline-flex max-w-[280px] truncate text-[9px] text-muted-foreground/70"
            title={actionSummary}
          >
            {actionSummary}
          </span>
        )}

        {/* Parsed command summary badge (CHAT7.7 — coexists with actionSummary) */}
        {commandSummary && (
          <span
            className="shrink-0 hidden md:inline-flex items-center gap-1 max-w-[280px] truncate rounded px-1 py-0.5 text-[9px] font-medium text-foreground/70 bg-card/60 border border-border/30"
            title={commandSummary.badge}
          >
            <span>{commandSummary.icon}</span>
            <span className="truncate">{commandSummary.badge}</span>
          </span>
        )}

        {/* Exit code badge */}
        {exitCode !== null && (
          <span
            className={clsx(
              "shrink-0 rounded px-1 py-0.5 text-[8px] font-mono font-medium border",
              exitCode === 0
                ? "bg-success/10 text-success border-success/30"
                : "bg-destructive/10 text-destructive border-destructive/30",
            )}
          >
            {exitCode === 0 ? "✓" : `exit ${exitCode}`}
          </span>
        )}

        {/* Duration badge */}
        {duration && (
          <span className="shrink-0 rounded px-1 py-0.5 text-[8px] font-mono text-muted-foreground border border-border/40">
            {duration}
          </span>
        )}

        {/* Running indicator */}
        {isRunning && (
          <span className="shrink-0 rounded px-1 py-0.5 text-[8px] font-medium text-info border border-info/30 bg-info/10">
            running
          </span>
        )}

        {/* Copy button (visible on hover) */}
        <button
          type="button"
          onClick={(e) => { e.stopPropagation(); handleCopy(); }}
          className="shrink-0 opacity-0 group-hover:opacity-100 text-muted-foreground hover:text-foreground transition p-0.5"
          aria-label="Copy command"
        >
          {copied ? <Check size={9} className="text-success" /> : <Copy size={9} />}
        </button>

        {hasOutput && (
          expanded
            ? <ChevronDown size={10} className="shrink-0 text-muted-foreground/60" />
            : <ChevronRight size={10} className="shrink-0 text-muted-foreground/60" />
        )}
      </button>

      {/* Expanded: output */}
      {expanded && hasOutput && (
        <div className="border-t border-border/30">
          {cwd && (
            <div className="border-b border-border/20 px-2.5 py-1 text-[9px] text-muted-foreground/70 font-mono">
              cwd: {cwd}
            </div>
          )}
          <TerminalOutput
            output={output}
            isError={isError}
            command={rawCommand}
            language={isError ? null : detectOutputLanguage(rawCommand)}
            filePath={extractFilePathFromCommand(rawCommand)}
          />
        </div>
      )}
    </div>
  );
}

// ── Terminal output renderer ────────────────────────────────────────

function TerminalOutput({
  output,
  isError,
  command,
  language,
  filePath,
}: {
  output: string;
  isError: boolean;
  /** The raw command string, used to detect git diff commands. */
  command?: string;
  /** Shiki language id when the output is file content. */
  language: string | null;
  /** Original file path (for ShikiPreview path-based inference). */
  filePath: string | null;
}) {
  const { text, truncated } = truncateOutput(output);
  const plainText = stripAnsiToText(text);

  // Detect git diff output: either the command is a git diff variant,
  // or the output itself looks like a unified diff. Render with the
  // rich @pierre/diffs PatchDiff component instead of plain text.
  const isDiffCommand = command != null && /\bgit\s+diff\b/.test(command);
  const isDiffOutput = !isError && (isDiffCommand || isUnifiedDiff(plainText));

  if (isDiffOutput && isUnifiedDiff(plainText)) {
    return (
      <div className="max-h-[320px] overflow-auto">
        <UnifiedDiff patch={plainText} />
        {truncated && (
          <div className="px-2.5 pb-1 text-[9px] text-muted-foreground italic">
            … output truncated ({Math.round(output.length / 1024)}KB total)
          </div>
        )}
      </div>
    );
  }

  // When we detect the output is file content, render with syntax
  // highlighting via ShikiPreview — same component ReadCard uses.
  if (language && !isError) {
    return (
      <div className="max-h-[320px] overflow-auto">
        <ShikiPreview code={text} lang={language} path={filePath ?? undefined} />
        {truncated && (
          <div className="px-2.5 pb-1 text-[9px] text-muted-foreground italic">
            … output truncated ({Math.round(output.length / 1024)}KB total)
          </div>
        )}
      </div>
    );
  }

  // Default: plain terminal output. ANSI escapes (colourised diffs from
  // git, eslint, npm test, …) are parsed and rendered as styled spans.
  return (
    <div className="max-h-[250px] overflow-auto px-2.5 py-2">
      <pre
        className={clsx(
          "font-mono text-[10px] leading-[1.5] whitespace-pre-wrap break-all",
          isError ? "text-destructive/80" : "text-foreground/60",
        )}
      >
        <AnsiText>{text}</AnsiText>
      </pre>
      {truncated && (
        <div className="mt-1 text-[9px] text-muted-foreground italic">
          … output truncated ({Math.round(output.length / 1024)}KB total)
        </div>
      )}
    </div>
  );
}
