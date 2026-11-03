/**
 * Parse +X / -Y line counts from a unified diff string.
 *
 * Used by EditCard and TurnSummaryCard to render compact delta badges.
 */

export interface DiffStats {
  added: number;
  removed: number;
}

/**
 * Counts lines starting with `+` (excluding `+++` header lines) as added,
 * and lines starting with `-` (excluding `---` header lines) as removed.
 *
 * Returns `{ added: 0, removed: 0 }` for empty or non-diff input.
 */
export function parseDiffStats(patch: string): DiffStats {
  if (!patch) return { added: 0, removed: 0 };

  let added = 0;
  let removed = 0;

  const lines = patch.split("\n");
  for (const line of lines) {
    if (line.startsWith("+++")) continue;
    if (line.startsWith("---")) continue;
    if (line.startsWith("+")) added++;
    else if (line.startsWith("-")) removed++;
  }

  return { added, removed };
}
