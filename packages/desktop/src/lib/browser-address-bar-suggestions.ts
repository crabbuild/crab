import {
  buildSearchUrl,
  DEFAULT_SEARCH_ENGINE,
  looksLikeSearchQuery,
  normalizeBrowserNavigationUrl,
  SEARCH_ENGINE_LABELS,
  type SearchEngine,
} from "./browser-url";

export const MAX_BROWSER_ADDRESS_BAR_SUGGESTIONS = 8;

export interface BrowserAddressBarSuggestion {
  url: string;
  title: string;
  subtitle: string;
  lastVisitedAt: number;
  visitCount: number;
  isSearch: boolean;
}

/**
 * Score a history entry against the user's query.
 * Prioritizes exact URL matches, then frequent visits, then recency.
 */
function scoreSuggestion(
  entry: { url: string; title: string; lastVisitedAt: number; visitCount: number },
  query: string,
): number {
  const lowerQuery = query.toLowerCase();
  const lowerUrl = entry.url.toLowerCase();
  const lowerTitle = entry.title.toLowerCase();

  if (!lowerUrl.includes(lowerQuery) && !lowerTitle.includes(lowerQuery)) {
    return -1;
  }

  let score = 0;
  if (lowerUrl.startsWith(lowerQuery) || lowerUrl.startsWith(`https://${lowerQuery}`)) {
    score += 100;
  }
  score += Math.min(entry.visitCount, 50);
  const ageHours = (Date.now() - entry.lastVisitedAt) / (1000 * 60 * 60);
  score += Math.max(0, 24 - ageHours);
  return score;
}

/**
 * Build address bar suggestions from history, search queries, and URL normalization.
 *
 * When the user types, up to 8 suggestions are shown:
 * - A top action (either a search URL or a normalized navigation URL)
 * - History entries that match the query, scored by recency and frequency
 */
export function buildBrowserAddressBarSuggestions({
  browserUrlHistory,
  searchEngine = DEFAULT_SEARCH_ENGINE,
  value,
}: {
  browserUrlHistory: readonly { url: string; title: string; lastVisitedAt: number; visitCount: number }[];
  searchEngine?: SearchEngine;
  value: string;
}): BrowserAddressBarSuggestion[] {
  const trimmed = value.trim();
  if (trimmed === "" || trimmed === "about:blank" || trimmed.startsWith("data:")) {
    // Empty or blank → show most recent history
    if (browserUrlHistory.length === 0) return [];
    return [...browserUrlHistory]
      .sort((a, b) => b.lastVisitedAt - a.lastVisitedAt)
      .slice(0, MAX_BROWSER_ADDRESS_BAR_SUGGESTIONS)
      .map((entry) => ({ ...entry, subtitle: entry.url, isSearch: false }));
  }

  // Score history entries against the query
  const historySuggestions: BrowserAddressBarSuggestion[] =
    browserUrlHistory.length > 0
      ? browserUrlHistory
          .map((entry) => ({ entry, score: scoreSuggestion(entry, trimmed) }))
          .filter((item) => item.score >= 0)
          .sort((a, b) => b.score - a.score)
          .slice(0, MAX_BROWSER_ADDRESS_BAR_SUGGESTIONS - 1)
          .map((item) => ({ ...item.entry, subtitle: item.entry.url, isSearch: false }))
      : [];

  // Build the top action (search or navigation)
  const isQuery = looksLikeSearchQuery(trimmed);
  let topAction: BrowserAddressBarSuggestion | null;

  if (isQuery) {
    topAction = {
      url: buildSearchUrl(trimmed, searchEngine),
      title: trimmed,
      subtitle: `${SEARCH_ENGINE_LABELS[searchEngine]} Search`,
      lastVisitedAt: 0,
      visitCount: 0,
      isSearch: true,
    };
  } else {
    const normalizedUrl = normalizeBrowserNavigationUrl(trimmed, searchEngine);
    topAction = normalizedUrl
      ? {
          url: normalizedUrl,
          title: trimmed,
          subtitle: "",
          lastVisitedAt: 0,
          visitCount: 0,
          isSearch: false,
        }
      : null;
  }

  if (!topAction) {
    return historySuggestions.slice(0, MAX_BROWSER_ADDRESS_BAR_SUGGESTIONS);
  }

  // Deduplicate: if the top action URL is already in history, skip it
  const duplicateIdx = historySuggestions.findIndex((h) => h.url === topAction.url);
  if (duplicateIdx >= 0) {
    return historySuggestions.slice(0, MAX_BROWSER_ADDRESS_BAR_SUGGESTIONS);
  }

  return [topAction, ...historySuggestions].slice(0, MAX_BROWSER_ADDRESS_BAR_SUGGESTIONS);
}
