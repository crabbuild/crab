import type { KanbanCardInfo } from "../../ipc";

/**
 * Escapes regex special characters in a string so it can be used
 * as a literal pattern in a RegExp constructor.
 */
function escapeRegex(str: string): string {
  return str.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Filters cards by case-insensitive substring match against title and description.
 * Returns only cards where the query appears in either field.
 * Returns all cards when query is empty.
 */
export function searchCards(cards: KanbanCardInfo[], query: string): KanbanCardInfo[] {
  const trimmed = query.trim();
  if (trimmed.length === 0) {
    return cards;
  }

  const pattern = new RegExp(escapeRegex(trimmed), "i");

  return cards.filter(
    (card) => pattern.test(card.title) || pattern.test(card.description),
  );
}

/**
 * Returns the indices of all non-overlapping matching substrings in a text
 * for highlighting purposes. Matches are case-insensitive.
 * Returns an empty array when query is empty or not found.
 */
export function findMatchRanges(
  text: string,
  query: string,
): Array<{ start: number; end: number }> {
  const trimmed = query.trim();
  if (trimmed.length === 0 || text.length === 0) {
    return [];
  }

  const pattern = new RegExp(escapeRegex(trimmed), "gi");
  const ranges: Array<{ start: number; end: number }> = [];

  let match: RegExpExecArray | null;
  while ((match = pattern.exec(text)) !== null) {
    ranges.push({ start: match.index, end: match.index + match[0].length });
  }

  return ranges;
}
