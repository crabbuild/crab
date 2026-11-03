/**
 * Parsed conflict region from git merge conflict markers.
 */
export interface ConflictRegion {
  /** 0-based line index of the `<<<<<<<` marker */
  startLine: number;
  /** 0-based line index of the `>>>>>>>` marker */
  endLine: number;
  /** Content between `<<<<<<<` and `=======` (current/ours) */
  currentContent: string;
  /** Content between `=======` and `>>>>>>>` (incoming/theirs) */
  incomingContent: string;
}

/**
 * Parses git conflict markers in a string and returns an array of conflict regions.
 *
 * Recognises the standard 3-way merge marker format:
 * ```
 * <<<<<<< ...
 * current content
 * =======
 * incoming content
 * >>>>>>> ...
 * ```
 */
export function parseConflictMarkers(content: string): ConflictRegion[] {
  const lines = content.split("\n");
  const regions: ConflictRegion[] = [];

  let i = 0;
  while (i < lines.length) {
    if (lines[i].startsWith("<<<<<<<")) {
      const startLine = i;
      const currentLines: string[] = [];
      const incomingLines: string[] = [];
      let inIncoming = false;
      let endLine = -1;

      i++;
      while (i < lines.length) {
        if (lines[i].startsWith("=======")) {
          inIncoming = true;
          i++;
          continue;
        }
        if (lines[i].startsWith(">>>>>>>")) {
          endLine = i;
          break;
        }
        if (inIncoming) {
          incomingLines.push(lines[i]);
        } else {
          currentLines.push(lines[i]);
        }
        i++;
      }

      if (endLine !== -1) {
        regions.push({
          startLine,
          endLine,
          currentContent: currentLines.join("\n"),
          incomingContent: incomingLines.join("\n"),
        });
      }
    }
    i++;
  }

  return regions;
}

/**
 * Resolves a single conflict region in the content by replacing the conflict
 * markers with the chosen resolution.
 */
export function resolveConflict(
  content: string,
  region: ConflictRegion,
  resolution: "current" | "incoming" | "both"
): string {
  const lines = content.split("\n");

  let replacement: string;
  switch (resolution) {
    case "current":
      replacement = region.currentContent;
      break;
    case "incoming":
      replacement = region.incomingContent;
      break;
    case "both":
      replacement = region.currentContent + "\n" + region.incomingContent;
      break;
  }

  // Replace lines from startLine to endLine (inclusive) with the resolution
  const before = lines.slice(0, region.startLine);
  const after = lines.slice(region.endLine + 1);

  // Handle the case where replacement is empty (both sides empty)
  if (replacement === "") {
    return [...before, ...after].join("\n");
  }

  return [...before, ...replacement.split("\n"), ...after].join("\n");
}
