/**
 * Paste Guard — inspects clipboard content before writing to the PTY
 * and warns users about potentially dangerous pastes.
 */

export interface PasteAnalysis {
  /** Whether the paste requires user confirmation. */
  requiresConfirmation: boolean;
  /** Reason(s) for requiring confirmation. */
  reasons: PasteWarningReason[];
  /** Line count of the pasted content. */
  lineCount: number;
  /** Control characters found (excluding \n and \t). */
  controlChars: string[];
  /** Dangerous patterns matched. */
  dangerousPatterns: string[];
  /** Sanitized preview for the confirmation dialog. */
  preview: string;
}

export type PasteWarningReason =
  | "control-chars"
  | "multi-line"
  | "dangerous-pattern";

export interface PasteGuardConfig {
  enabled: boolean;
  multiLineThreshold: number;
  dangerousPatterns: RegExp[];
}

/** Default dangerous patterns that trigger a warning. */
export const DEFAULT_DANGEROUS_PATTERNS: RegExp[] = [
  /rm\s+-rf\s+\//,
  /rm\s+-rf\s+~/,
  /:\(\)\{\s*:\|:\&\s*\}\s*;?\s*:/,
  />\s*\/dev\/sda/,
  />\s*\/dev\/null/,
  /\bmkfs\b/,
  /\bdd\s+if=/,
  /chmod\s+-R\s+777\s+\//,
  /curl\s+.*\|\s*(?:sudo\s+)?(?:ba)?sh/,
  /wget\s+.*\|\s*(?:sudo\s+)?(?:ba)?sh/,
];

export const DEFAULT_PASTE_GUARD_CONFIG: PasteGuardConfig = {
  enabled: true,
  multiLineThreshold: 5,
  dangerousPatterns: DEFAULT_DANGEROUS_PATTERNS,
};

/**
 * Map of control character code points to their caret-notation representations.
 * Excludes \n (0x0a) and \t (0x09) which are considered safe.
 */
const CONTROL_CHAR_NAMES: Record<number, string> = {
  0x00: "^@",
  0x01: "^A",
  0x02: "^B",
  0x03: "^C",
  0x04: "^D",
  0x05: "^E",
  0x06: "^F",
  0x07: "^G",
  0x08: "^H",
  // 0x09 = \t (excluded)
  // 0x0a = \n (excluded)
  0x0b: "^K",
  0x0c: "^L",
  0x0d: "^M",
  0x0e: "^N",
  0x0f: "^O",
  0x10: "^P",
  0x11: "^Q",
  0x12: "^R",
  0x13: "^S",
  0x14: "^T",
  0x15: "^U",
  0x16: "^V",
  0x17: "^W",
  0x18: "^X",
  0x19: "^Y",
  0x1a: "^Z",
  0x1b: "^[",
  0x1c: "^\\",
  0x1d: "^]",
  0x1e: "^^",
  0x1f: "^_",
  0x7f: "^?",
};

/**
 * Detect control characters in content, excluding \n (0x0a) and \t (0x09).
 */
function detectControlChars(content: string): string[] {
  const found: string[] = [];
  const seen = new Set<number>();

  for (let i = 0; i < content.length; i++) {
    const code = content.charCodeAt(i);
    // Control chars: 0x00-0x1f (excluding 0x09 tab and 0x0a newline) and 0x7f (DEL)
    const isControl =
      (code >= 0x00 && code <= 0x1f && code !== 0x09 && code !== 0x0a) ||
      code === 0x7f;
    if (isControl && !seen.has(code)) {
      seen.add(code);
      found.push(CONTROL_CHAR_NAMES[code] ?? `0x${code.toString(16).padStart(2, "0")}`);
    }
  }

  return found;
}

/**
 * Match content against dangerous patterns and return matched pattern descriptions.
 */
function matchDangerousPatterns(
  content: string,
  patterns: RegExp[]
): string[] {
  const matched: string[] = [];
  for (const pattern of patterns) {
    if (pattern.test(content)) {
      matched.push(pattern.source);
    }
  }
  return matched;
}

/**
 * Build a sanitized preview of the content suitable for display.
 * - Replaces control characters with visible caret-notation representations.
 * - For multi-line content (>6 lines), shows first 3 lines + "..." + last 3 lines.
 */
function buildPreview(content: string): string {
  // Replace control chars (excluding \n and \t) with visible representations
  let sanitized = "";
  for (let i = 0; i < content.length; i++) {
    const code = content.charCodeAt(i);
    const isControl =
      (code >= 0x00 && code <= 0x1f && code !== 0x09 && code !== 0x0a) ||
      code === 0x7f;
    if (isControl) {
      sanitized += CONTROL_CHAR_NAMES[code] ?? `0x${code.toString(16).padStart(2, "0")}`;
    } else {
      sanitized += content[i];
    }
  }

  const lines = sanitized.split("\n");
  if (lines.length > 6) {
    const first3 = lines.slice(0, 3);
    const last3 = lines.slice(-3);
    return [...first3, "...", ...last3].join("\n");
  }

  return sanitized;
}

/**
 * Analyze paste content and determine if user confirmation is required.
 *
 * Confirmation is required when any of these conditions hold:
 * 1. Content contains control characters other than \n and \t
 * 2. Content has more lines than the configured threshold
 * 3. Content matches a known dangerous pattern
 */
export function analyzePaste(
  content: string,
  config: PasteGuardConfig
): PasteAnalysis {
  const lines = content.split("\n");
  const lineCount = lines.length;
  const reasons: PasteWarningReason[] = [];

  // Detect control characters
  const controlChars = detectControlChars(content);
  if (controlChars.length > 0) {
    reasons.push("control-chars");
  }

  // Detect multi-line content exceeding threshold
  if (lineCount > config.multiLineThreshold) {
    reasons.push("multi-line");
  }

  // Match dangerous patterns
  const dangerousPatterns = matchDangerousPatterns(
    content,
    config.dangerousPatterns
  );
  if (dangerousPatterns.length > 0) {
    reasons.push("dangerous-pattern");
  }

  const preview = buildPreview(content);

  return {
    requiresConfirmation: reasons.length > 0,
    reasons,
    lineCount,
    controlChars,
    dangerousPatterns,
    preview,
  };
}

/**
 * Wrap content in bracketed paste mode escape sequences.
 * Prefix: ESC[200~ (0x1b 0x5b 0x32 0x30 0x30 0x7e)
 * Suffix: ESC[201~ (0x1b 0x5b 0x32 0x30 0x31 0x7e)
 */
export function wrapBracketedPaste(content: string): string {
  return `\x1b[200~${content}\x1b[201~`;
}
