import type { Command } from "./command-registry";

/**
 * Fuzzy subsequence match scoring commands against a query.
 * Matches against both label and category (case-insensitive).
 * Returns ranked results sorted by score descending (best match first).
 */
export function fuzzyFilterCommands(
  commands: Command[],
  query: string,
): Array<{ command: Command; score: number }> {
  if (!query) return commands.map((command) => ({ command, score: 0 }));

  const q = query.toLowerCase();
  const results: Array<{ command: Command; score: number }> = [];

  for (const command of commands) {
    const labelScore = matchScore(command.label.toLowerCase(), q);
    const categoryScore = matchScore(command.category.toLowerCase(), q);
    const score = Math.max(labelScore, categoryScore);
    if (score > 0) {
      results.push({ command, score });
    }
  }

  results.sort((a, b) => b.score - a.score);
  return results;
}

/**
 * Subsequence match with scoring that favours:
 *  - prefix matches (query starts at beginning of target)
 *  - consecutive matching characters
 *  - shorter target strings (more specific matches)
 *
 * Returns 0 if the query is not a subsequence of the target.
 */
function matchScore(target: string, query: string): number {
  let ti = 0;
  let qi = 0;
  let score = 0;
  let prevHit = -2;

  while (ti < target.length && qi < query.length) {
    if (target[ti] === query[qi]) {
      // Base credit for each matched character
      score += 10;

      // Bonus for consecutive character matches
      if (prevHit === ti - 1) {
        score += 15;
      }

      // Bonus for prefix match (first query char matches first target char)
      if (qi === 0 && ti === 0) {
        score += 20;
      }

      prevHit = ti;
      qi++;
    }
    ti++;
  }

  // Not a full subsequence match — reject
  if (qi < query.length) return 0;

  // Penalise longer targets (shorter labels rank higher for same match quality)
  score -= Math.floor(target.length / 3);

  return score;
}
