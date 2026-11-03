// ─── Webview Registry ───────────────────────────────────────────────────────
// Manages webview parking, eviction, focus management, and drag passthrough.
// Heavily inspired by Orca's webview-registry.ts.

export const webviewRegistry = new Map<string, Electron.WebviewTag>();
export const MAX_PARKED_WEBVIEWS = 6;

let hiddenContainer: HTMLDivElement | null = null;

/**
 * Get or create the off-screen container where parked webviews live.
 * Moving hidden webviews out of the visible DOM (rather than display:none)
 * prevents macOS from briefly flashing the guest content during tab switches.
 */
function getHiddenContainer(): HTMLDivElement {
  if (!hiddenContainer) {
    hiddenContainer = document.createElement("div");
    hiddenContainer.style.position = "fixed";
    hiddenContainer.style.left = "-9999px";
    hiddenContainer.style.top = "-9999px";
    hiddenContainer.style.width = "100vw";
    hiddenContainer.style.height = "100vh";
    hiddenContainer.style.overflow = "hidden";
    hiddenContainer.style.pointerEvents = "none";
    document.body.appendChild(hiddenContainer);
  }
  return hiddenContainer;
}

/**
 * Register a webview for a browser tab.
 */
export function registerWebview(tabId: string, webview: Electron.WebviewTag): void {
  webviewRegistry.set(tabId, webview);
}

/**
 * Unregister a webview without destroying it.
 */
export function unregisterWebview(tabId: string): void {
  webviewRegistry.delete(tabId);
}

/**
 * Park a webview by moving it to the hidden off-screen container.
 * This preserves the webview's state (navigation history, session cookies)
 * while removing it from the visible DOM.
 */
export function parkWebview(tabId: string): void {
  const webview = webviewRegistry.get(tabId);
  if (!webview) return;

  // Move focus back to the renderer before detaching the webview
  moveFocusBeforeDetach(webview);

  const container = getHiddenContainer();
  if (webview.parentNode !== container) {
    container.appendChild(webview);
  }
}

/**
 * Evict the oldest parked webviews when the registry exceeds the limit.
 * Called after parking a new webview.
 */
export function evictParkedWebviews(excludeTabId: string): void {
  if (webviewRegistry.size <= MAX_PARKED_WEBVIEWS) return;

  // Build list of tab IDs ordered by when they were parked
  // We need to check which webviews are in the hidden container
  const parked: string[] = [];
  const container = getHiddenContainer();
  for (const [tid, wv] of webviewRegistry.entries()) {
    if (tid === excludeTabId) continue;
    if (wv.parentNode === container) {
      parked.push(tid);
    }
  }

  // Evict oldest parked webviews
  const toEvict = Math.max(0, webviewRegistry.size - MAX_PARKED_WEBVIEWS);
  for (let i = 0; i < toEvict && i < parked.length; i++) {
    destroyWebview(parked[i]);
  }
}

/**
 * Destroy a webview for a browser tab.
 * Cleans up the webview element, removes it from the DOM, and
 * unregisters it from the registry.
 */
export function destroyWebview(tabId: string): void {
  const webview = webviewRegistry.get(tabId);
  if (!webview) {
    webviewRegistry.delete(tabId);
    return;
  }

  // Unregister from the main process
  try {
    window.crab.browser.unregisterGuest({ browserTabId: tabId }).catch(() => {});
  } catch {
    // best-effort
  }

  // Move focus before removal
  moveFocusBeforeDetach(webview);

  // Remove from DOM
  try {
    webview.remove();
  } catch {
    // best-effort
  }

  webviewRegistry.delete(tabId);
}

/**
 * Before detaching or removing a focused webview, blur it and refocus
 * the window. This prevents macOS from switching to a different app
 * when the focused element is destroyed.
 */
function moveFocusBeforeDetach(webview: Electron.WebviewTag): void {
  if (typeof document === "undefined" || typeof window === "undefined") return;
  const active = document.activeElement as HTMLElement | null;
  if (!active) return;
  if (webview === active || webview.contains(active)) {
    try {
      active.blur();
    } catch {
      // best-effort
    }
    window.focus();
  }
}

/**
 * Clean up all webviews. Called during component unmount.
 */
export function destroyAllWebviews(): void {
  const tabIds = [...webviewRegistry.keys()];
  for (const tabId of tabIds) {
    destroyWebview(tabId);
  }

  // Clean up hidden container
  if (hiddenContainer && hiddenContainer.parentNode) {
    try {
      hiddenContainer.remove();
    } catch {
      // best-effort
    }
  }
  hiddenContainer = null;
}
