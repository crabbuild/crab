/**
 * Scrollback buffer configuration, validation, and memory estimation.
 *
 * Manages terminal scrollback size limits, memory caps, and provides
 * utilities for the settings UI to display estimated memory usage.
 * Also implements virtual scrollback (disk-backed overflow) for
 * "unlimited" mode.
 */

// --- Default constants ---

export const DEFAULT_MAX_LINES = 50_000;
export const DEFAULT_MEMORY_CAP = 256 * 1024 * 1024; // 256 MB
export const DEFAULT_OVERSCAN_LINES = 100;
export const MIN_MAX_LINES = 1_000;
export const MAX_MAX_LINES = 1_000_000;
export const DEFAULT_AVG_CHARS_PER_LINE = 80;

// --- Interfaces ---

export interface ScrollbackConfig {
  maxLines: number; // 1000–1000000, default: 50000
  memoryCapBytes: number; // default: 256 * 1024 * 1024
  virtualScrollbackEnabled: boolean; // "unlimited" mode
  overscanLines: number; // default: 100
}

export interface ScrollbackStats {
  currentLines: number;
  estimatedMemoryBytes: number;
  overflowLines: number; // lines on disk (virtual scrollback)
  overflowFilePath: string | null;
}

// --- Functions ---

/**
 * Estimate memory usage for a given scrollback line count.
 *
 * Calculation: lineCount × avgCharsPerLine × 2 (UTF-16 encoding overhead).
 */
export function estimateMemory(
  lineCount: number,
  avgCharsPerLine: number = DEFAULT_AVG_CHARS_PER_LINE,
): number {
  if (lineCount < 0) return 0;
  if (avgCharsPerLine < 0) return 0;
  return lineCount * avgCharsPerLine * 2;
}

/**
 * Validate and normalize a partial scrollback configuration.
 *
 * Clamps maxLines to [1000, 1000000], ensures memoryCapBytes is positive,
 * ensures overscanLines is non-negative, and returns errors for invalid inputs.
 */
export function validateScrollbackConfig(config: Partial<ScrollbackConfig>): {
  valid: boolean;
  normalized: ScrollbackConfig;
  errors: string[];
} {
  const errors: string[] = [];

  // maxLines: clamp to valid range
  let maxLines = config.maxLines ?? DEFAULT_MAX_LINES;
  if (typeof maxLines !== "number" || isNaN(maxLines)) {
    errors.push("maxLines must be a valid number");
    maxLines = DEFAULT_MAX_LINES;
  } else if (maxLines < MIN_MAX_LINES) {
    errors.push(
      `maxLines ${maxLines} is below minimum (${MIN_MAX_LINES}), clamped to ${MIN_MAX_LINES}`,
    );
    maxLines = MIN_MAX_LINES;
  } else if (maxLines > MAX_MAX_LINES) {
    errors.push(
      `maxLines ${maxLines} exceeds maximum (${MAX_MAX_LINES}), clamped to ${MAX_MAX_LINES}`,
    );
    maxLines = MAX_MAX_LINES;
  }
  maxLines = Math.round(maxLines);

  // memoryCapBytes: must be positive
  let memoryCapBytes = config.memoryCapBytes ?? DEFAULT_MEMORY_CAP;
  if (typeof memoryCapBytes !== "number" || isNaN(memoryCapBytes)) {
    errors.push("memoryCapBytes must be a valid number");
    memoryCapBytes = DEFAULT_MEMORY_CAP;
  } else if (memoryCapBytes <= 0) {
    errors.push("memoryCapBytes must be positive");
    memoryCapBytes = DEFAULT_MEMORY_CAP;
  }

  // virtualScrollbackEnabled: default false
  let virtualScrollbackEnabled = config.virtualScrollbackEnabled ?? false;
  if (typeof virtualScrollbackEnabled !== "boolean") {
    errors.push("virtualScrollbackEnabled must be a boolean");
    virtualScrollbackEnabled = false;
  }

  // overscanLines: must be non-negative
  let overscanLines = config.overscanLines ?? DEFAULT_OVERSCAN_LINES;
  if (typeof overscanLines !== "number" || isNaN(overscanLines)) {
    errors.push("overscanLines must be a valid number");
    overscanLines = DEFAULT_OVERSCAN_LINES;
  } else if (overscanLines < 0) {
    errors.push("overscanLines must be non-negative");
    overscanLines = 0;
  }
  overscanLines = Math.round(overscanLines);

  return {
    valid: errors.length === 0,
    normalized: {
      maxLines,
      memoryCapBytes,
      virtualScrollbackEnabled,
      overscanLines,
    },
    errors,
  };
}

// ── Virtual Scrollback (disk-backed overflow) ───────────────────────

/**
 * Line index entry for random access into the overflow file.
 * Stores the byte offset and length of each line in the temp file.
 */
interface LineIndexEntry {
  offset: number;
  length: number;
}

/**
 * Virtual scrollback manager for "unlimited" mode.
 *
 * When enabled, overflow lines (those that would be discarded by the
 * in-memory scrollback limit) are written to a temp file on disk.
 * Lines are indexed for random access so they can be loaded on-demand
 * when the user scrolls into the overflow region.
 *
 * Falls back to in-memory-only mode if disk write fails.
 */
export class VirtualScrollback {
  private lineIndex: LineIndexEntry[] = [];
  private currentOffset: number = 0;
  private disabled: boolean = false;
  private sessionId: string;
  private _overflowFilePath: string | null = null;

  constructor(sessionId: string) {
    this.sessionId = sessionId;
  }

  /** Whether virtual scrollback is active (not disabled by errors). */
  get active(): boolean {
    return !this.disabled;
  }

  /** Number of overflow lines stored on disk. */
  get overflowLineCount(): number {
    return this.lineIndex.length;
  }

  /** Path to the overflow temp file (null if not yet created). */
  get overflowFilePath(): string | null {
    return this._overflowFilePath;
  }

  /**
   * Write overflow lines to the temp file.
   * Lines are appended and indexed for later random access.
   *
   * Returns the data to be written to disk (caller handles actual I/O).
   * Returns null if virtual scrollback is disabled.
   */
  writeLines(lines: string[]): { data: string; path: string } | null {
    if (this.disabled || lines.length === 0) return null;

    if (!this._overflowFilePath) {
      this._overflowFilePath = this.buildOverflowPath();
    }

    let data = "";
    for (const line of lines) {
      const encoded = line + "\n";
      this.lineIndex.push({
        offset: this.currentOffset,
        length: encoded.length,
      });
      this.currentOffset += encoded.length;
      data += encoded;
    }

    return { data, path: this._overflowFilePath };
  }

  /**
   * Read lines by index range from the overflow file.
   *
   * Returns { startOffset, length } for the caller to read from disk.
   * Returns null if the range is invalid or virtual scrollback is disabled.
   */
  getReadRange(startLine: number, endLine: number): { offset: number; length: number } | null {
    if (this.disabled) return null;
    if (startLine < 0 || endLine < startLine) return null;

    const clampedStart = Math.max(0, startLine);
    const clampedEnd = Math.min(this.lineIndex.length - 1, endLine);

    if (clampedStart >= this.lineIndex.length) return null;

    const startEntry = this.lineIndex[clampedStart];
    const endEntry = this.lineIndex[clampedEnd];

    return {
      offset: startEntry.offset,
      length: (endEntry.offset + endEntry.length) - startEntry.offset,
    };
  }

  /**
   * Get the byte offset and length for a single line.
   * Returns null if the index is out of bounds.
   */
  getLineRange(lineIndex: number): { offset: number; length: number } | null {
    if (this.disabled || lineIndex < 0 || lineIndex >= this.lineIndex.length) {
      return null;
    }
    return { ...this.lineIndex[lineIndex] };
  }

  /**
   * Parse lines from raw data read from the overflow file.
   * The data should correspond to the range returned by getReadRange().
   */
  parseLines(data: string): string[] {
    // Split by newline, removing the trailing empty string from the final \n
    const lines = data.split("\n");
    if (lines.length > 0 && lines[lines.length - 1] === "") {
      lines.pop();
    }
    return lines;
  }

  /**
   * Disable virtual scrollback (e.g., on disk write failure).
   * Falls back to in-memory-only mode.
   */
  disable(): void {
    this.disabled = true;
  }

  /** Get current stats. */
  getStats(): { overflowLines: number; overflowFilePath: string | null; disabled: boolean } {
    return {
      overflowLines: this.lineIndex.length,
      overflowFilePath: this._overflowFilePath,
      disabled: this.disabled,
    };
  }

  /** Reset the virtual scrollback (e.g., on terminal clear). */
  reset(): void {
    this.lineIndex = [];
    this.currentOffset = 0;
  }

  private buildOverflowPath(): string {
    // The actual path is constructed by the main process; this is a
    // placeholder that the IPC layer will resolve.
    return `~/.crab-desktop/terminal-scrollback-overflow/${this.sessionId}.tmp`;
  }
}
