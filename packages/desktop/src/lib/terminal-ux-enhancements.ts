/**
 * Terminal UX enhancements module.
 *
 * Provides agent detection, bookmarks, semantic filtering, timestamp
 * filtering, and retry/copy utilities for the terminal view.
 *
 * Font configuration has moved to ./terminal-font.ts — re-exported here
 * for backward compatibility.
 */

import type { Terminal } from "@xterm/xterm";
import type { PromptMark } from "./terminal-shell-integration";

// Re-export font config from the dedicated module.
export type { FontConfig } from "./terminal-font";
export {
  DEFAULT_FONT_CONFIG,
  loadFontConfig,
  saveFontConfig,
  applyFontConfig,
  buildFontFamily,
  fontHasKnownLigatures,
  attachWebgl,
  disposeWebgl,
  rebuildWebgl,
} from "./terminal-font";
export type { WebglState } from "./terminal-font";

// ── Drag-and-Drop Path Insertion (Task 26.2) ────────────────────────

/**
 * Extract file paths from a DragEvent and format them as quoted
 * space-separated absolute paths suitable for terminal insertion.
 */
export function extractDropPaths(e: DragEvent): string | null {
  const files = e.dataTransfer?.files;
  if (files && files.length > 0) {
    const paths = Array.from(files)
      .map((f) => {
        const path = (f as any).path as string | undefined;
        return path || f.name;
      })
      .map((p) => (p.includes(" ") ? `"${p}"` : p));
    return paths.join(" ");
  }
  // Try text/uri-list (from file tree drag)
  const uriList = e.dataTransfer?.getData("text/uri-list");
  if (uriList) {
    const paths = uriList
      .split("\n")
      .filter((l) => l && !l.startsWith("#"))
      .map((uri) => {
        try { return decodeURIComponent(new URL(uri).pathname); }
        catch { return uri; }
      })
      .map((p) => (p.includes(" ") ? `"${p}"` : p));
    if (paths.length > 0) return paths.join(" ");
  }
  // Try plain text
  const text = e.dataTransfer?.getData("text/plain");
  if (text && text.trim()) return text.trim();
  return null;
}

// ── Agent Session Detection (Task 27.1) ─────────────────────────────

export type AgentType = "claude" | "codex" | "aider" | "gemini" | "q" | "unknown";

interface AgentDetection {
  type: AgentType;
  label: string;
  startedAt: number;
}

const AGENT_PATTERNS: { pattern: RegExp; type: AgentType; label: string }[] = [
  { pattern: /\bclaude\b/i, type: "claude", label: "Claude" },
  { pattern: /\bcodex\b/i, type: "codex", label: "Codex" },
  { pattern: /\baider\b/i, type: "aider", label: "Aider" },
  { pattern: /\bgemini\b/i, type: "gemini", label: "Gemini" },
  { pattern: /\b(amazon-?q|aws-?q)\b/i, type: "q", label: "Q" },
];

/**
 * Detect if a running command matches a known agent CLI.
 * Uses the command text from OSC 133 marks.
 */
export function detectAgent(commandText: string | null): AgentDetection | null {
  if (!commandText) return null;
  for (const { pattern, type, label } of AGENT_PATTERNS) {
    if (pattern.test(commandText)) {
      return { type, label, startedAt: Date.now() };
    }
  }
  return null;
}

/** Get a simple icon character for an agent type (used in tab labels). */
export function agentIcon(type: AgentType): string {
  switch (type) {
    case "claude": return "🤖";
    case "codex": return "⚡";
    case "aider": return "🔧";
    case "gemini": return "✨";
    case "q": return "🅠";
    default: return "🤖";
  }
}

// ── Copy Last Command Output with ANSI Stripping (Task 27.2) ────────

// eslint-disable-next-line no-control-regex
const ANSI_REGEX = /\x1b\[[0-9;]*[a-zA-Z]|\x1b\][^\x07]*\x07|\x1b\(B|\x1b\[[\?]?[0-9;]*[hlm]/g;

/** Strip ANSI escape codes for clean clipboard content. */
export function stripAnsiCodes(str: string): string {
  return str.replace(ANSI_REGEX, "");
}

// ── Semantic Output Filtering (Task 27.3) ────────────────────────────

export type SemanticFilter = "errors" | "warnings" | "paths" | "urls" | "all";

/** Filter terminal output lines by semantic category. */
export function filterOutputLines(
  lines: string[],
  filter: SemanticFilter,
): string[] {
  if (filter === "all") return lines;

  return lines.filter((line) => {
    const stripped = stripAnsiCodes(line);
    switch (filter) {
      case "errors":
        // Lines with red ANSI or common error patterns
        return /\x1b\[31m|\x1b\[1;31m/.test(line) ||
          /\b(error|Error|ERROR|FAILED|fatal|panic)\b/.test(stripped);
      case "warnings":
        // Lines with yellow ANSI or common warning patterns
        return /\x1b\[33m|\x1b\[1;33m/.test(line) ||
          /\b(warn|Warning|WARNING|WARN|deprecated)\b/.test(stripped);
      case "paths":
        // Lines containing file paths
        return /(?:\/[\w.-]+){2,}|(?:[A-Z]:\\[\w\\.-]+)/.test(stripped);
      case "urls":
        // Lines containing URLs
        return /https?:\/\/[^\s]+/.test(stripped);
      default:
        return true;
    }
  });
}

// ── Retry Last Command (Task 27.4) ──────────────────────────────────

/**
 * Get the last command text from shell integration marks.
 * Looks for the most recent command-start → command-executed sequence
 * and extracts the command text from the terminal buffer.
 */
export function getLastCommand(
  terminal: Terminal,
  marks: PromptMark[],
): string | null {
  // Find the last command-start mark followed by command-executed
  let lastCommandStartLine = -1;
  let lastCommandExecLine = -1;

  for (let i = marks.length - 1; i >= 0; i--) {
    if (marks[i].type === "command-executed" && lastCommandExecLine === -1) {
      lastCommandExecLine = marks[i].line;
    }
    if (marks[i].type === "command-start" && lastCommandExecLine !== -1) {
      lastCommandStartLine = marks[i].line;
      break;
    }
  }

  if (lastCommandStartLine === -1 || lastCommandExecLine === -1) return null;

  // Extract text between command-start and command-executed marks
  const buffer = terminal.buffer?.active;
  if (!buffer) return null;

  const lines: string[] = [];
  for (let row = lastCommandStartLine; row <= lastCommandExecLine; row++) {
    const bufferLine = buffer.getLine(row);
    if (bufferLine) {
      lines.push(bufferLine.translateToString(true));
    }
  }

  const text = lines.join("").trim();
  return text.length > 0 ? text : null;
}

// ── Terminal Session Bookmarks (Task 27.5) ───────────────────────────

export interface Bookmark {
  id: string;
  line: number;
  label: string;
  timestamp: number;
}

export class BookmarkManager {
  private _bookmarks: Bookmark[] = [];

  get bookmarks(): Bookmark[] {
    return [...this._bookmarks];
  }

  get count(): number {
    return this._bookmarks.length;
  }

  add(line: number, label?: string): Bookmark {
    const bookmark: Bookmark = {
      id: `bm-${Date.now()}-${Math.random().toString(36).slice(2, 6)}`,
      line,
      label: label || `Bookmark ${this._bookmarks.length + 1}`,
      timestamp: Date.now(),
    };
    this._bookmarks.push(bookmark);
    this._bookmarks.sort((a, b) => a.line - b.line);
    return bookmark;
  }

  remove(id: string): void {
    this._bookmarks = this._bookmarks.filter((b) => b.id !== id);
  }

  rename(id: string, newLabel: string): void {
    const bm = this._bookmarks.find((b) => b.id === id);
    if (bm) bm.label = newLabel;
  }

  clear(): void {
    this._bookmarks = [];
  }

  /** Find the next bookmark after the given line. */
  findNext(currentLine: number): Bookmark | null {
    for (const bm of this._bookmarks) {
      if (bm.line > currentLine) return bm;
    }
    return this._bookmarks[0] ?? null; // wrap around
  }

  /** Find the previous bookmark before the given line. */
  findPrevious(currentLine: number): Bookmark | null {
    for (let i = this._bookmarks.length - 1; i >= 0; i--) {
      if (this._bookmarks[i].line < currentLine) return this._bookmarks[i];
    }
    return this._bookmarks[this._bookmarks.length - 1] ?? null; // wrap around
  }
}

// ── Timestamp Filtering (Task 26.6) ─────────────────────────────────

/**
 * Filter prompt marks to only those within the last N minutes.
 * Useful for "show output from last N minutes" quick filter.
 */
export function filterMarksByTime(
  marks: PromptMark[],
  minutesAgo: number,
): PromptMark[] {
  const cutoff = Date.now() - minutesAgo * 60 * 1000;
  return marks.filter((m) => m.timestamp >= cutoff);
}

/**
 * Get the line range for output from the last N minutes.
 * Returns [startLine, endLine] or null if no marks in range.
 */
export function getTimeFilteredRange(
  marks: PromptMark[],
  minutesAgo: number,
): { startLine: number; endLine: number } | null {
  const filtered = filterMarksByTime(marks, minutesAgo);
  if (filtered.length === 0) return null;
  return {
    startLine: filtered[0].line,
    endLine: filtered[filtered.length - 1].line,
  };
}
