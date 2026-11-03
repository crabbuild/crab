/**
 * Shell integration parser for xterm.js terminals.
 *
 * Handles OSC 7 (current working directory reporting) and OSC 133
 * (prompt/command marks) for prompt navigation, command duration
 * tracking, and exit code recording.
 *
 * OSC 7 format: `\x1b]7;file://hostname/path\x07`
 * OSC 133 format: `\x1b]133;X\x07` where X is A, B, C, or D;exitcode
 */

import type { Terminal, IDisposable } from "@xterm/xterm";

// ── Types ───────────────────────────────────────────────────────────

export interface PromptMark {
  /** Line number in the terminal buffer where this mark occurs. */
  line: number;
  /** Type of mark: prompt-start, command-start, command-executed, command-finished. */
  type: "prompt-start" | "command-start" | "command-executed" | "command-finished";
  /** Exit code (only present on command-finished marks). */
  exitCode?: number;
  /** Timestamp when this mark was recorded. */
  timestamp: number;
}

export interface ShellIntegrationState {
  /** Current working directory reported by OSC 7. */
  cwd: string | null;
  /** Whether the shell is currently at a prompt (no command running). */
  atPrompt: boolean;
  /** Ring buffer of prompt marks (most recent N marks). */
  marks: PromptMark[];
  /** Currently running command text (between command-start and command-finished). */
  runningCommand: string | null;
  /** Timestamp when the current command started. */
  commandStartedAt: number | null;
  /** Duration of the last completed command in milliseconds. */
  lastCommandDuration: number | null;
  /** Exit code of the last completed command. */
  lastCommandExitCode: number | null;
}

// ── MarkRingBuffer ─────────────────────────────────────────────────

/**
 * Circular buffer for prompt marks. Uses a fixed-capacity array with
 * O(1) push. When full, the oldest mark is overwritten.
 */
export class MarkRingBuffer {
  readonly capacity: number;
  /** Internal storage array (pre-allocated to capacity). */
  private _marks: (PromptMark | null)[];
  /** Write position (next slot to write). */
  private _head: number = 0;
  /** Number of marks currently stored. */
  private _size: number = 0;

  constructor(capacity: number = 1000) {
    this.capacity = capacity;
    this._marks = new Array(capacity).fill(null);
  }

  /** Current number of stored marks. */
  get size(): number {
    return this._size;
  }

  /** All stored marks in insertion order (oldest first). */
  get marks(): PromptMark[] {
    if (this._size === 0) return [];
    const result: PromptMark[] = [];
    const start = this._size < this.capacity ? 0 : this._head;
    for (let i = 0; i < this._size; i++) {
      const idx = (start + i) % this.capacity;
      result.push(this._marks[idx]!);
    }
    return result;
  }

  /** Write position (exposed for interface compatibility). */
  get head(): number {
    return this._head;
  }

  /** Add a mark to the buffer. O(1). */
  push(mark: PromptMark): void {
    this._marks[this._head] = mark;
    this._head = (this._head + 1) % this.capacity;
    if (this._size < this.capacity) {
      this._size++;
    }
  }

  /** Find the nearest mark with line < fromLine, searching backwards. */
  findPrevious(fromLine: number): PromptMark | null {
    const allMarks = this.marks;
    for (let i = allMarks.length - 1; i >= 0; i--) {
      if (allMarks[i].line < fromLine) return allMarks[i];
    }
    return null;
  }

  /** Find the nearest mark with line > fromLine, searching forwards. */
  findNext(fromLine: number): PromptMark | null {
    const allMarks = this.marks;
    for (let i = 0; i < allMarks.length; i++) {
      if (allMarks[i].line > fromLine) {
        return allMarks[i];
      }
    }
    return null;
  }

  /** Get all marks within [startLine, endLine] inclusive. */
  getRange(startLine: number, endLine: number): PromptMark[] {
    return this.marks.filter(m => m.line >= startLine && m.line <= endLine);
  }
}

// ── OSC 7 Parsing ──────────────────────────────────────────────────

/**
 * Parse an OSC 7 payload and extract the filesystem path.
 *
 * Expected format: `file://hostname/path` or `file:///path`
 * The path is URL-decoded to handle percent-encoded characters.
 *
 * Returns the decoded path, or null if the payload is not a valid file URI.
 */
export function parseOsc7Uri(data: string): string | null {
  const prefix = "file://";
  if (!data.startsWith(prefix)) return null;

  const afterScheme = data.slice(prefix.length);

  // Find the path portion — it starts at the first `/` after the hostname.
  const slashIndex = afterScheme.indexOf("/");
  if (slashIndex === -1) return null;

  const rawPath = afterScheme.slice(slashIndex);
  if (rawPath.length === 0) return null;

  try {
    return decodeURIComponent(rawPath);
  } catch {
    // Malformed percent-encoding — return the raw path as a best effort.
    return rawPath;
  }
}

// ── OSC 133 Parsing ────────────────────────────────────────────────

/**
 * Parse an OSC 133 data payload and return the marker type and optional exit code.
 *
 * OSC 133 data format:
 * - "A" → prompt-start
 * - "B" → command-start
 * - "C" → command-executed
 * - "D;exitcode" → command-finished (exitcode is an integer)
 * - "D" → command-finished (no exit code, defaults to 0)
 */
export function parseOsc133(data: string): { type: PromptMark["type"]; exitCode?: number } | null {
  if (data.length === 0) return null;

  const marker = data[0];
  switch (marker) {
    case "A":
      return { type: "prompt-start" };
    case "B":
      return { type: "command-start" };
    case "C":
      return { type: "command-executed" };
    case "D": {
      // Format: "D" or "D;exitcode"
      if (data.length === 1) {
        return { type: "command-finished", exitCode: 0 };
      }
      if (data[1] === ";") {
        const codeStr = data.slice(2);
        const exitCode = parseInt(codeStr, 10);
        return { type: "command-finished", exitCode: isNaN(exitCode) ? 0 : exitCode };
      }
      return { type: "command-finished", exitCode: 0 };
    }
    default:
      return null;
  }
}

// ── Duration Formatting ────────────────────────────────────────────

/**
 * Format a duration in milliseconds as a human-readable string.
 *
 * - Less than 1 second: "<1s"
 * - Seconds only: "5s"
 * - Minutes and seconds: "2m 34s"
 * - Hours and minutes: "1h 5m"
 */
export function formatDuration(ms: number): string {
  if (ms < 1000) return "<1s";

  const totalSeconds = Math.floor(ms / 1000);
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;

  if (hours > 0) {
    return minutes > 0 ? `${hours}h ${minutes}m` : `${hours}h`;
  }
  if (minutes > 0) {
    return seconds > 0 ? `${minutes}m ${seconds}s` : `${minutes}m`;
  }
  return `${seconds}s`;
}

// ── Public API ─────────────────────────────────────────────────────

/**
 * Attach shell integration parsing to an xterm.js terminal instance.
 *
 * Handles OSC 7 (cwd tracking) and OSC 133 (prompt marks, command
 * duration, exit codes, prompt navigation).
 */
export function attachShellIntegration(terminal: Terminal): {
  state: ShellIntegrationState;
  ringBuffer: MarkRingBuffer;
  jumpToPreviousPrompt(): void;
  jumpToNextPrompt(): void;
  selectCommandOutput(markIndex: number): string | null;
  getCommandDuration(): number | null;
  dispose(): void;
} {
  const ringBuffer = new MarkRingBuffer(1000);

  const state: ShellIntegrationState = {
    cwd: null,
    atPrompt: false,
    marks: ringBuffer.marks,
    runningCommand: null,
    commandStartedAt: null,
    lastCommandDuration: null,
    lastCommandExitCode: null,
  };

  const disposables: IDisposable[] = [];

  // Register OSC 7 handler for cwd tracking.
  const osc7Handler = terminal.parser.registerOscHandler(7, (data: string) => {
    const path = parseOsc7Uri(data);
    if (path !== null) {
      state.cwd = path;
    }
    return true;
  });
  disposables.push(osc7Handler);

  // Register OSC 133 handler for prompt/command marks.
  const osc133Handler = terminal.parser.registerOscHandler(133, (data: string) => {
    const parsed = parseOsc133(data);
    if (parsed === null) return true;

    const line = terminal.buffer?.active?.cursorY ?? 0;
    const baseOffset = terminal.buffer?.active?.baseY ?? 0;
    const absoluteLine = baseOffset + line;

    const mark: PromptMark = {
      line: absoluteLine,
      type: parsed.type,
      timestamp: Date.now(),
    };

    if (parsed.exitCode !== undefined) {
      mark.exitCode = parsed.exitCode;
    }

    ringBuffer.push(mark);
    // Keep state.marks in sync with the ring buffer.
    state.marks = ringBuffer.marks;

    // Update state based on marker type.
    switch (parsed.type) {
      case "prompt-start":
        state.atPrompt = true;
        state.runningCommand = null;
        break;
      case "command-start":
        state.atPrompt = false;
        state.commandStartedAt = Date.now();
        break;
      case "command-executed":
        // Command text has been entered, now executing.
        break;
      case "command-finished":
        if (state.commandStartedAt !== null) {
          state.lastCommandDuration = Date.now() - state.commandStartedAt;
        }
        state.lastCommandExitCode = parsed.exitCode ?? null;
        state.runningCommand = null;
        state.commandStartedAt = null;
        break;
    }

    return true;
  });
  disposables.push(osc133Handler);

  return {
    state,
    ringBuffer,

    jumpToPreviousPrompt(): void {
      const currentLine = (terminal.buffer?.active?.baseY ?? 0) +
        (terminal.buffer?.active?.cursorY ?? 0);
      const prev = ringBuffer.findPrevious(currentLine);
      if (prev !== null) {
        terminal.scrollToLine(prev.line);
      }
    },

    jumpToNextPrompt(): void {
      const currentLine = (terminal.buffer?.active?.baseY ?? 0) +
        (terminal.buffer?.active?.cursorY ?? 0);
      const next = ringBuffer.findNext(currentLine);
      if (next !== null) {
        terminal.scrollToLine(next.line);
      }
    },

    selectCommandOutput(markIndex: number): string | null {
      const allMarks = ringBuffer.marks;
      if (markIndex < 0 || markIndex >= allMarks.length) return null;

      // Find the command-executed mark at or after markIndex.
      let execMark: PromptMark | null = null;
      let nextPromptMark: PromptMark | null = null;

      // Look for a command-executed mark at the given index.
      const targetMark = allMarks[markIndex];
      if (targetMark.type === "command-executed") {
        execMark = targetMark;
      } else {
        // Search forward from markIndex for the nearest command-executed.
        for (let i = markIndex; i < allMarks.length; i++) {
          if (allMarks[i].type === "command-executed") {
            execMark = allMarks[i];
            break;
          }
        }
      }

      if (execMark === null) return null;

      // Find the next prompt-start mark after the exec mark.
      for (let i = 0; i < allMarks.length; i++) {
        if (allMarks[i].line > execMark.line && allMarks[i].type === "prompt-start") {
          nextPromptMark = allMarks[i];
          break;
        }
      }

      const startLine = execMark.line;
      const endLine = nextPromptMark !== null ? nextPromptMark.line - 1 : startLine;

      if (endLine < startLine) return null;

      // Extract text from the terminal buffer between the marks.
      const buffer = terminal.buffer?.active;
      if (!buffer) return null;

      const lines: string[] = [];
      for (let row = startLine; row <= endLine; row++) {
        const bufferLine = buffer.getLine(row);
        if (bufferLine) {
          lines.push(bufferLine.translateToString(true));
        }
      }

      const text = lines.join("\n");
      return text.length > 0 ? text : null;
    },

    getCommandDuration(): number | null {
      if (state.commandStartedAt === null) return null;
      return Date.now() - state.commandStartedAt;
    },

    dispose(): void {
      for (const d of disposables) {
        d.dispose();
      }
      disposables.length = 0;
    },
  };
}
