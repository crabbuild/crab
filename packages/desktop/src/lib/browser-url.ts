// ─── Constants ──────────────────────────────────────────────────────────────

export const CRAB_BROWSER_BLANK_URL: string = 'data:text/html,';

const LOCAL_ADDRESS_PATTERN =
  /^(?:localhost|127(?:\.\d{1,3}){3}|0\.0\.0\.0|\[[0-9a-f:]+\])(?::\d+)?(?:[/?#].*)?$/i;

// Bare words like "react hooks" trigger a search. Inputs that look like
// domain names ("example.com", "foo.bar/path") navigate directly.
const LOOKS_LIKE_URL_PATTERN = /^[^\s]+\.[a-z]{2,}(\/.*)?$/i;
const WINDOWS_ABSOLUTE_PATH_PATTERN = /^[A-Za-z]:[\\/].*$/;
const WINDOWS_UNC_PATH_PATTERN = /^\\\\[^\s\\/]+[\\/][^\\/]+(?:[\\/].*)?$/;
const UNIX_ABSOLUTE_PATH_PATTERN = /^\/.*$/;

// ─── Search Engines ─────────────────────────────────────────────────────────

export type SearchEngine = 'google' | 'duckduckgo' | 'bing';

export const SEARCH_ENGINE_LABELS: Record<SearchEngine, string> = {
  google: 'Google',
  duckduckgo: 'DuckDuckGo',
  bing: 'Bing',
};

const SEARCH_ENGINE_URLS: Record<SearchEngine, string> = {
  google: 'https://www.google.com/search?q=',
  duckduckgo: 'https://duckduckgo.com/?q=',
  bing: 'https://www.bing.com/search?q=',
};

export const DEFAULT_SEARCH_ENGINE: SearchEngine = 'google';

// ─── Search URL Builder ─────────────────────────────────────────────────────

export function buildSearchUrl(
  query: string,
  engine: SearchEngine = DEFAULT_SEARCH_ENGINE,
): string {
  return `${SEARCH_ENGINE_URLS[engine]}${encodeURIComponent(query)}`;
}

// ─── URL vs Search Detection ───────────────────────────────────────────────

/** Returns true when the input is likely a search query, not a URL attempt. */
export function looksLikeSearchQuery(input: string): boolean {
  if (input.includes(' ')) return true;
  if (LOOKS_LIKE_URL_PATTERN.test(input)) return false;
  if (input.includes('.') || input.includes(':')) return false;
  return true;
}

// ─── File Path → file:// Conversion ────────────────────────────────────────

function absolutePathToFileUrl(filePath: string): string {
  const normalizedPath = filePath.replaceAll('\\', '/');
  const segments = normalizedPath.split('/').map((segment, index) => {
    if (index === 0 && /^[A-Za-z]:$/.test(segment)) return segment;
    return encodeURIComponent(segment);
  });
  return normalizedPath.startsWith('/')
    ? `file://${segments.join('/')}`
    : `file:///${segments.join('/')}`;
}

function windowsUncPathToFileUrl(filePath: string): string {
  const normalizedPath = filePath.replaceAll('\\', '/').replace(/^\/+/, '');
  const [host, ...pathSegments] = normalizedPath.split('/');
  return `file://${host}/${pathSegments.map(encodeURIComponent).join('/')}`;
}

// ─── Main URL Normalization ────────────────────────────────────────────────

/**
 * Normalize a user-entered string into a navigable URL.
 *
 * When `searchEngine` is passed, inputs that don't look like URLs are
 * sent to the search engine instead of returning null.
 *
 * Only http://, https://, and file:// schemes are allowed.
 */
export function normalizeBrowserNavigationUrl(
  rawUrl: string,
  searchEngine?: SearchEngine | null,
): string | null {
  const trimmed = rawUrl.trim();
  if (trimmed.length === 0 || trimmed === 'about:blank' || trimmed === CRAB_BROWSER_BLANK_URL) {
    return CRAB_BROWSER_BLANK_URL;
  }

  // Local addresses → http:// prefix (localhost uses http, not https)
  if (LOCAL_ADDRESS_PATTERN.test(trimmed)) {
    try {
      return new URL(`http://${trimmed}`).toString();
    } catch {
      return null;
    }
  }

  // Windows UNC paths → file://
  if (WINDOWS_UNC_PATH_PATTERN.test(trimmed)) {
    return windowsUncPathToFileUrl(trimmed);
  }

  // Absolute paths → file://
  if (UNIX_ABSOLUTE_PATH_PATTERN.test(trimmed) || WINDOWS_ABSOLUTE_PATH_PATTERN.test(trimmed)) {
    return absolutePathToFileUrl(trimmed);
  }

  // Valid URL schemes
  try {
    const parsed = new URL(trimmed);
    if (
      parsed.protocol === 'http:' ||
      parsed.protocol === 'https:' ||
      parsed.protocol === 'file:'
    ) {
      return parsed.toString();
    }
    return null; // Reject javascript:, data:, blob:, etc.
  } catch {
    // Not a valid URL — try search fallback
    const searchEnabled = searchEngine !== undefined;
    try {
      const withScheme = new URL(`https://${trimmed}`);
      if (!searchEnabled || !looksLikeSearchQuery(trimmed)) {
        return withScheme.toString();
      }
    } catch {
      // Not a valid URL even with https:// prefix
    }

    if (!searchEnabled) return null;
    return buildSearchUrl(trimmed, searchEngine ?? DEFAULT_SEARCH_ENGINE);
  }
}

/**
 * Normalize a URL for opening in the system browser (shell.openExternal).
 * Only http(s) targets are allowed; file:// is rejected for external opening.
 */
export function normalizeExternalBrowserUrl(rawUrl: string): string | null {
  const normalized = normalizeBrowserNavigationUrl(rawUrl);
  if (normalized === null || normalized === CRAB_BROWSER_BLANK_URL) return null;
  if (normalized.startsWith('file:')) return null;
  return normalized;
}
