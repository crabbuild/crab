import type { KanbanCardInfo, KanbanCardPriority, KanbanPipelineState } from "../../ipc";

export interface ActiveFilters {
  assignees: string[];
  labels: string[];
  priorities: KanbanCardPriority[];
  pipelineStates: KanbanPipelineState[];
}

export interface FilterOptions {
  assignees: string[];
  labels: string[];
  priorities: KanbanCardPriority[];
  pipelineStates: KanbanPipelineState[];
}

/**
 * Filters cards by active criteria.
 * AND logic across filter types, OR logic within a single filter type.
 *
 * A card is included only if it matches at least one value in every non-empty
 * filter category. Empty filter categories are ignored (treated as "any").
 */
export function filterCards(cards: KanbanCardInfo[], filters: ActiveFilters): KanbanCardInfo[] {
  return cards.filter((card) => {
    if (filters.assignees.length > 0) {
      if (!card.assignee || !filters.assignees.includes(card.assignee)) {
        return false;
      }
    }

    if (filters.labels.length > 0) {
      const hasMatchingLabel = card.labels.some((label) => filters.labels.includes(label));
      if (!hasMatchingLabel) {
        return false;
      }
    }

    if (filters.priorities.length > 0) {
      if (!filters.priorities.includes(card.priority)) {
        return false;
      }
    }

    if (filters.pipelineStates.length > 0) {
      if (!filters.pipelineStates.includes(card.pipelineState)) {
        return false;
      }
    }

    return true;
  });
}

/**
 * Extracts all unique filter options from the current card set.
 * Returns sorted arrays for consistent display ordering.
 */
export function extractFilterOptions(cards: KanbanCardInfo[]): FilterOptions {
  const assigneeSet = new Set<string>();
  const labelSet = new Set<string>();
  const prioritySet = new Set<KanbanCardPriority>();
  const pipelineStateSet = new Set<KanbanPipelineState>();

  for (const card of cards) {
    if (card.assignee) {
      assigneeSet.add(card.assignee);
    }
    for (const label of card.labels) {
      labelSet.add(label);
    }
    prioritySet.add(card.priority);
    pipelineStateSet.add(card.pipelineState);
  }

  return {
    assignees: [...assigneeSet].sort(),
    labels: [...labelSet].sort(),
    priorities: [...prioritySet],
    pipelineStates: [...pipelineStateSet],
  };
}

/**
 * Fuzzy-matches a query string against a list of options.
 * Returns matching options ranked by relevance (exact prefix > contains > fuzzy).
 *
 * Matching is case-insensitive. A match requires that all characters of the
 * query appear in order within the option string (subsequence match).
 */
export function fuzzyMatchOptions(query: string, options: string[]): string[] {
  if (!query) {
    return options;
  }

  const lowerQuery = query.toLowerCase();

  interface ScoredOption {
    option: string;
    score: number;
  }

  const scored: ScoredOption[] = [];

  for (const option of options) {
    const lowerOption = option.toLowerCase();

    // Exact prefix match — highest relevance
    if (lowerOption.startsWith(lowerQuery)) {
      scored.push({ option, score: 3 });
      continue;
    }

    // Contains as substring — high relevance
    if (lowerOption.includes(lowerQuery)) {
      scored.push({ option, score: 2 });
      continue;
    }

    // Subsequence match — lower relevance
    if (isSubsequence(lowerQuery, lowerOption)) {
      scored.push({ option, score: 1 });
    }
  }

  scored.sort((a, b) => b.score - a.score);
  return scored.map((s) => s.option);
}

/**
 * Checks if `query` is a subsequence of `text`.
 * All characters of query must appear in text in order.
 */
function isSubsequence(query: string, text: string): boolean {
  let qi = 0;
  for (let ti = 0; ti < text.length && qi < query.length; ti++) {
    if (text[ti] === query[qi]) {
      qi++;
    }
  }
  return qi === query.length;
}
