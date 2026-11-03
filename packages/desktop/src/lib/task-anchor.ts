/**
 * Anchor parsing and formatting helpers for the task list.
 *
 * An anchor is a string that ties a task to a location in the repo:
 * - "file:src/main.ts:42" — a file at a specific line
 * - "commit:abc1234" — a specific commit
 */

export interface FileAnchor {
  type: "file";
  path: string;
  line: number;
}

export interface CommitAnchor {
  type: "commit";
  sha: string;
}

export type ParsedAnchor = FileAnchor | CommitAnchor;

/**
 * Parse an anchor string into a structured object.
 * Returns `null` for invalid or unrecognized formats.
 */
export function parseAnchor(anchor: string): ParsedAnchor | null {
  if (!anchor) return null;

  if (anchor.startsWith("file:")) {
    // Format: "file:<path>:<line>"
    // The path may contain colons (e.g. Windows paths are unlikely here
    // but we handle it by splitting from the right for the line number).
    const rest = anchor.slice(5); // strip "file:"
    const lastColon = rest.lastIndexOf(":");
    if (lastColon === -1) return null;

    const path = rest.slice(0, lastColon);
    const lineStr = rest.slice(lastColon + 1);
    const line = parseInt(lineStr, 10);

    if (!path || isNaN(line) || line < 0) return null;
    return { type: "file", path, line };
  }

  if (anchor.startsWith("commit:")) {
    const sha = anchor.slice(7); // strip "commit:"
    if (!sha || !/^[0-9a-f]{4,40}$/i.test(sha)) return null;
    return { type: "commit", sha };
  }

  return null;
}

/**
 * Format an anchor string for display. Returns a short human-readable
 * label suitable for a chip or tooltip.
 *
 * - "file:src/utils/helpers.ts:42" → "helpers.ts:42"
 * - "commit:abc1234def5678" → "abc1234"
 */
export function formatAnchor(anchor: string): string {
  const parsed = parseAnchor(anchor);
  if (!parsed) return anchor;

  if (parsed.type === "file") {
    const segments = parsed.path.split("/");
    const filename = segments[segments.length - 1] || parsed.path;
    return `${filename}:${parsed.line}`;
  }

  // Show first 7 chars of the SHA (standard short-sha convention).
  return parsed.sha.slice(0, 7);
}
