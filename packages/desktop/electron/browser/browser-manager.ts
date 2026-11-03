/* eslint-disable max-lines -- Why: BrowserManager is the single privileged
facade for guest registration, download lifecycle, anti-detection, permission
routing, context-menu delegation, and viewport emulation. Keeping authority
in one file avoids scattering the browser security boundary across modules. */

import { app,clipboard,session,webContents,BrowserWindow,dialog,type Session } from 'electron';
import { randomUUID } from 'node:crypto';
import { join } from 'node:path';

import { browserSessionRegistry } from "./browser-session-registry";
import {
pickCookieFile,
importCookiesFromFile,
detectInstalledBrowsers,
selectBrowserProfile,
importCookiesFromBrowser,type DetectedBrowser
} from "./browser-cookie-import";
import { BrowserGrabSessionController } from "./browser-grab-session-controller";
import { buildGuestOverlayScript } from "./grab-guest-script";
import { clampGrabPayload } from "./browser-grab-payload";
import { captureSelectionScreenshot as captureGrabSelectionScreenshot } from "./browser-grab-screenshot";
import type {
BrowserGrabCancelReason,
BrowserGrabPayload,
BrowserGrabRect,
BrowserGrabScreenshot,
BrowserAnnotationPayload,
BrowserPageAnnotation,
} from "./browser-grab-types";
import type { BrowserAnnotationViewportBridgeOptions } from "./browser-annotation-viewport-bridge";
;
// ─── Constants ─────────────────────────────────────────────────────────────

export const CRAB_BROWSER_PARTITION = 'persist:crab-browser';
export const CRAB_BROWSER_BLANK_URL: string = 'data:text/html,';

// ─── Anti-Detection Script ────────────────────────────────────────────────
// Injected via CDP Page.addScriptToEvaluateOnNewDocument before any page JS.
// Masks automation signals that CDP debugger attachment and Electron webview
// expose — without this, Cloudflare Turnstile and Google block the browser.

export const ANTI_DETECTION_SCRIPT = `(function() {
  Object.defineProperty(navigator, 'webdriver', { get: () => false });
  if (navigator.plugins.length === 0) {
    Object.defineProperty(navigator, 'plugins', {
      get: () => [
        { name: 'Chrome PDF Plugin', filename: 'internal-pdf-viewer' },
        { name: 'Chrome PDF Viewer', filename: 'mhjfbmdgcfjbbpaeojofohoefgiehjai' },
        { name: 'Native Client', filename: 'internal-nacl-plugin' }
      ]
    });
  }
  if (!window.chrome) { window.chrome = {}; }
  if (!window.chrome.runtime) { window.chrome.runtime = {}; }
  if (!window.chrome.csi) {
    window.chrome.csi = function() {
      return { startE: Date.now(), onloadT: Date.now(), pageT: performance.now(), tran: 15 };
    };
  }
  if (!window.chrome.loadTimes) {
    window.chrome.loadTimes = function() {
      return {
        commitLoadTime: Date.now() / 1000, connectionInfo: 'h2',
        finishDocumentLoadTime: Date.now() / 1000, finishLoadTime: Date.now() / 1000,
        firstPaintAfterLoadTime: 0, firstPaintTime: Date.now() / 1000,
        navigationType: 'Other', npnNegotiatedProtocol: 'h2',
        requestTime: Date.now() / 1000 - 0.16, startLoadTime: Date.now() / 1000 - 0.3,
        wasAlternateProtocolAvailable: false, wasFetchedViaSpdy: true, wasNpnNegotiated: true
      };
    };
  }
  const promptPerms = new Set([
    'notifications', 'geolocation', 'camera', 'microphone',
    'midi', 'idle-detection', 'storage-access'
  ]);
  const origQuery = Permissions.prototype.query;
  Permissions.prototype.query = function(desc) {
    if (promptPerms.has(desc.name)) {
      return Promise.resolve({ state: 'prompt', onchange: null });
    }
    return origQuery.call(this, desc);
  };
  try { Object.defineProperty(Notification, 'permission', { get: () => 'default' }); } catch {}
  if (!navigator.languages || navigator.languages.length === 0) {
    Object.defineProperty(navigator, 'languages', { get: () => ['en-US', 'en'] });
  }
})()`;

// ─── User-Agent Cleaning ──────────────────────────────────────────────────

export function cleanElectronUserAgent(ua: string): string {
  return ua
    .replace(/\s+Electron\/\S+/, '')
    .replace(/(\)\s+)\S+\s+(Chrome\/)/, '$1$2');
}

function setupClientHintsOverride(sess: Session, ua: string): void {
  const chromeMatch = ua.match(/Chrome\/([\d.]+)/);
  if (!chromeMatch) return;
  const fullChromeVersion = chromeMatch[1];
  const majorVersion = fullChromeVersion.split('.')[0];

  let brand = 'Google Chrome';
  let brandFullVersion = fullChromeVersion;
  const edgeMatch = ua.match(/Edg\/([\d.]+)/);
  if (edgeMatch) { brand = 'Microsoft Edge'; brandFullVersion = edgeMatch[1]; }
  const brandMajor = brandFullVersion.split('.')[0];

  const secChUa = `"${brand}";v="${brandMajor}", "Chromium";v="${majorVersion}", "Not/A)Brand";v="24"`;
  const secChUaFull = `"${brand}";v="${brandFullVersion}", "Chromium";v="${fullChromeVersion}", "Not/A)Brand";v="24.0.0.0"`;

  sess.webRequest.onBeforeSendHeaders({ urls: ['https://*/*'] }, (details, callback) => {
    const headers = details.requestHeaders;
    for (const key of Object.keys(headers)) {
      const lower = key.toLowerCase();
      if (lower === 'sec-ch-ua') headers[key] = secChUa;
      else if (lower === 'sec-ch-ua-full-version-list') headers[key] = secChUaFull;
    }
    callback({ requestHeaders: headers });
  });
}

// ─── Types ─────────────────────────────────────────────────────────────────

export type CrabBrowserTabState = {
  browserTabId: string;
  webContentsId: number;
  rendererWebContentsId: number;
  worktreeId: string;
  url: string;
  title: string;
  loading: boolean;
  faviconUrl: string | null;
  canGoBack: boolean;
  canGoForward: boolean;
};

export type CrabBrowserEvent = {
  browserTabId: string;
  url?: string;
  title?: string;
  faviconUrl?: string | null;
  loading?: boolean;
  canGoBack?: boolean;
  canGoForward?: boolean;
};

export type CrabDownloadRequestedEvent = {
  downloadId: string;
  browserTabId: string;
  origin: string;
  filename: string;
  totalBytes: number | null;
  mimeType: string | null;
};

export type CrabDownloadProgressEvent = {
  downloadId: string;
  browserTabId: string;
  receivedBytes: number;
  totalBytes: number;
};

export type CrabDownloadFinishedEvent = {
  downloadId: string;
  browserTabId: string;
  status: 'completed' | 'canceled' | 'interrupted';
  savePath: string | null;
  error: string | null;
};

export type CrabContextMenuEvent = {
  browserTabId: string;
  x: number;
  y: number;
  pageUrl: string;
  linkUrl: string | null;
  canGoBack: boolean;
  canGoForward: boolean;
};

export type CrabLoadFailureEvent = {
  browserTabId: string;
  code: number;
  description: string;
  validatedUrl: string;
};

// ─── Active Downloads ───────────────────────────────────────────────────────

type ActiveDownload = {
  downloadId: string;
  guestWebContentsId: number;
  browserTabId: string | null;
  rendererWebContentsId: number | null;
  origin: string;
  filename: string;
  totalBytes: number | null;
  mimeType: string | null;
  receivedBytes: number;
  item: Electron.DownloadItem;
  state: 'requested' | 'downloading' | 'completed' | 'canceled';
  savePath: string | null;
  cleanup: (() => void) | null;
};

function safeOrigin(rawUrl: string): string {
  try { return new URL(rawUrl).origin; } catch { return 'unknown'; }
}


// ─── Navigation Guard ─────────────────────────────────────────────────────────

/**
 * Validates whether a URL is safe for in-guest navigation.
 * Blocks Chrome-internal pages, file:// navigations from remote pages,
 * and other dangerous schemes while allowing blob:http(s) for Turnstile.
 */
function isAllowedNavigationUrl(url: string): boolean {
  // blob: URLs are same-origin (inherit creator origin). Allow only
  // http(s) blobs for Cloudflare Turnstile compatibility.
  if (url.startsWith("blob:https://") || url.startsWith("blob:http://")) {
    return true;
  }
  // file: navigations inside a guest loaded from http(s) would let
  // a remote page probe the local filesystem.
  if (url.startsWith("file:")) {
    return false;
  }
  // Chrome-internal schemes — no web content should reach these.
  const blockedSchemes = ["chrome:", "about:", "chrome-extension:", "chrome-devtools:", "view-source:", "data:"];
  for (const scheme of blockedSchemes) {
    if (url.startsWith(scheme)) return false;
  }
  // http(s) is the standard allowed scheme for web navigation.
  if (url.startsWith("http://") || url.startsWith("https://")) {
    return true;
  }
  return false;
}
// ─── BrowserManager ─────────────────────────────────────────────────────────

class BrowserManager {
  /** The webContents ID of the trusted renderer (main window) for security validation. */
  private trustedRendererWebContentsId: number | null = null;

  // Forward and reverse maps for guest webContents lookup
  private readonly webContentsIdByTabId = new Map<string, number>();
  private readonly tabIdByWebContentsId = new Map<number, string>();
  private readonly rendererWebContentsIdByTabId = new Map<string, number>();
  private readonly worktreeIdByTabId = new Map<string, string>();

  // Download tracking
  private readonly downloadsById = new Map<string, ActiveDownload>();
  private readonly pendingDownloadIdsByGuestId = new Map<number, string[]>();

  // Track whether we've set up session policies
  private partitionConfigured = false;

  // Grab & annotation state
  private readonly grabSessionController = new BrowserGrabSessionController();
  private readonly annotationsByPageId = new Map<string, BrowserPageAnnotation[]>();


  // ── Public API ──────────────────────────────────────────────────────────

  getTabIds(): string[] {
    return [...this.webContentsIdByTabId.keys()];
  }

  getGuestWebContentsId(browserTabId: string): number | null {
    return this.webContentsIdByTabId.get(browserTabId) ?? null;
  }

  getWorktreeIdForTab(browserTabId: string): string | null {
    return this.worktreeIdByTabId.get(browserTabId) ?? null;
  }

  getTabState(browserTabId: string): CrabBrowserTabState | null {
    const webContentsId = this.webContentsIdByTabId.get(browserTabId);
    if (webContentsId == null) return null;
    const rendererId = this.rendererWebContentsIdByTabId.get(browserTabId);
    const guest = webContents.fromId(webContentsId);
    if (!guest || guest.isDestroyed()) {
      this.unregisterGuest(browserTabId);
      return null;
    }
    return {
      browserTabId,
      webContentsId,
      rendererWebContentsId: rendererId ?? 0,
      worktreeId: this.worktreeIdByTabId.get(browserTabId) ?? '',
      url: guest.getURL() || CRAB_BROWSER_BLANK_URL,
      title: guest.getTitle() || 'New Tab',
      loading: guest.isLoading(),
      faviconUrl: null,
      canGoBack: guest.canGoBack(),
      canGoForward: guest.canGoForward(),
    };
  }

  getDownloadPrompt(downloadId: string, senderWebContentsId: number): {
    filename: string;
    totalBytes: number | null;
    mimeType: string | null;
  } | null {
    const download = this.downloadsById.get(downloadId);
    if (!download || download.rendererWebContentsId !== senderWebContentsId) return null;
    return { filename: download.filename, totalBytes: download.totalBytes, mimeType: download.mimeType };
  }

  // ── Guest Registration ─────────────────────────────────────────────────

  registerGuest(args: {
    browserTabId: string;
    worktreeId: string;
    webContentsId: number;
    rendererWebContentsId: number;
  }): void {
    const { browserTabId, worktreeId, webContentsId, rendererWebContentsId } = args;

    const previousWcId = this.webContentsIdByTabId.get(browserTabId);
    if (previousWcId != null) this.tabIdByWebContentsId.delete(previousWcId);
    const previousTabId = this.tabIdByWebContentsId.get(webContentsId);
    if (previousTabId != null) {
      this.webContentsIdByTabId.delete(previousTabId);
      this.rendererWebContentsIdByTabId.delete(previousTabId);
      this.worktreeIdByTabId.delete(previousTabId);
    }

    this.webContentsIdByTabId.set(browserTabId, webContentsId);
    this.tabIdByWebContentsId.set(webContentsId, browserTabId);
    this.rendererWebContentsIdByTabId.set(browserTabId, rendererWebContentsId);
    this.worktreeIdByTabId.set(browserTabId, worktreeId);

    this.attachGuestListeners(browserTabId, webContentsId);
    this.flushPendingDownloadRequests(browserTabId, webContentsId);
  }

  unregisterGuest(browserTabId: string): void {
    const wcId = this.webContentsIdByTabId.get(browserTabId);
    if (wcId != null) this.tabIdByWebContentsId.delete(wcId);
    this.webContentsIdByTabId.delete(browserTabId);
    this.rendererWebContentsIdByTabId.delete(browserTabId);
    this.worktreeIdByTabId.delete(browserTabId);
    // Why: unregistering a guest while a grab is active means the guest is
    // being torn down. Cancel the grab so the renderer gets a clean signal
    // and doesn't leave stale grab UI around.
    this.grabSessionController.cancelGrabOp(browserTabId, 'evicted');
  }

  // ── Download Lifecycle ─────────────────────────────────────────────────

  handleGuestWillDownload(args: { guestWebContentsId: number; item: Electron.DownloadItem }): void {
    const { guestWebContentsId, item } = args;
    const downloadId = randomUUID();
    const browserTabId = this.tabIdByWebContentsId.get(guestWebContentsId) ?? null;

    const download: ActiveDownload = {
      downloadId,
      guestWebContentsId,
      browserTabId,
      rendererWebContentsId: browserTabId ? (this.rendererWebContentsIdByTabId.get(browserTabId) ?? null) : null,
      origin: safeOrigin(item.getURL()),
      filename: item.getFilename(),
      totalBytes: item.getTotalBytes(),
      mimeType: item.getMimeType(),
      receivedBytes: 0,
      item,
      state: 'requested',
      savePath: null,
      cleanup: null,
    };

    this.downloadsById.set(downloadId, download);

    if (browserTabId) {
      this.sendDownloadRequested(download);
    } else {
      const pending = this.pendingDownloadIdsByGuestId.get(guestWebContentsId) ?? [];
      pending.push(downloadId);
      this.pendingDownloadIdsByGuestId.set(guestWebContentsId, pending);
    }

    const onUpdated = (_event: Electron.Event, state: string): void => {
      if (download.state === 'canceled' || download.state === 'completed') return;
      if (state === 'progressing') {
        download.state = 'downloading';
        download.receivedBytes = item.getReceivedBytes();
        download.totalBytes = item.getTotalBytes();
        if (download.browserTabId) this.sendDownloadProgress(download);
      } else if (state === 'interrupted') {
        download.state = 'canceled';
        if (download.browserTabId) this.sendDownloadFinished(download, 'interrupted');
        this.downloadsById.delete(downloadId);
      }
    };

    const onDone = (_event: Electron.Event, state: string): void => {
      if (state === 'completed') {
        download.state = 'completed';
        download.savePath = item.getSavePath();
        if (download.browserTabId) this.sendDownloadFinished(download, 'completed');
        this.downloadsById.delete(downloadId);
      }
    };

    item.on('updated', onUpdated);
    item.on('done', onDone);
    download.cleanup = (): void => {
      try { item.removeListener('updated', onUpdated); item.removeListener('done', onDone); } catch {}
    };
  }

  async acceptDownload(args: {
    downloadId: string;
    senderWebContentsId: number;
    savePath: string;
  }): Promise<{ ok: boolean; reason?: string }> {
    const download = this.downloadsById.get(args.downloadId);
    if (!download || download.rendererWebContentsId !== args.senderWebContentsId) {
      return { ok: false, reason: 'not-authorized' };
    }
    try {
      download.item.setSavePath(args.savePath);
      download.item.resume();
      download.savePath = args.savePath;
      return { ok: true };
    } catch (err) {
      return { ok: false, reason: err instanceof Error ? err.message : 'unknown' };
    }
  }

  cancelDownload(args: { downloadId: string; senderWebContentsId: number }): boolean {
    const download = this.downloadsById.get(args.downloadId);
    if (!download || download.rendererWebContentsId !== args.senderWebContentsId) return false;
    download.state = 'canceled';
    if (download.cleanup) { download.cleanup(); download.cleanup = null; }
    try { download.item.cancel(); } catch {}
    if (download.browserTabId) this.sendDownloadFinished(download, 'canceled');
    this.downloadsById.delete(args.downloadId);
    return true;
  }

  flushPendingDownloadRequests(browserTabId: string, guestWebContentsId: number): void {
    const pending = this.pendingDownloadIdsByGuestId.get(guestWebContentsId);
    if (!pending?.length) return;
    this.pendingDownloadIdsByGuestId.delete(guestWebContentsId);
    for (const downloadId of pending) {
      const download = this.downloadsById.get(downloadId);
      if (download) {
        download.browserTabId = browserTabId;
        download.rendererWebContentsId = this.rendererWebContentsIdByTabId.get(browserTabId) ?? null;
        this.sendDownloadRequested(download);
      }
    }
  }

  // ── DevTools ───────────────────────────────────────────────────────────

  openDevTools(browserTabId: string): boolean {
    const wcId = this.webContentsIdByTabId.get(browserTabId);
    if (wcId == null) {
      console.warn("[browser-manager] openDevTools: no wcId for tab", browserTabId);
      return false;
    }
    const guest = webContents.fromId(wcId);
    if (!guest || guest.isDestroyed()) {
      console.warn("[browser-manager] openDevTools: guest destroyed", browserTabId, wcId);
      return false;
    }
    try {
      // Try to close any existing DevTools (ignore failures)
      try { if (guest.isDevToolsOpened()) guest.closeDevTools(); } catch {}
      // Open detached DevTools for this webview
      guest.openDevTools({ mode: 'detach' });
      return true;
    } catch (err) {
      // Fallback: try without mode: 'detach' (open as bottom split)
      try {
        guest.openDevTools({ mode: 'bottom' });
        return true;
      } catch (err2) {
        console.warn("[browser-manager] openDevTools failed", {
          browserTabId, webContentsId: wcId,
          error: err instanceof Error ? err.message : String(err),
        });
        return false;
      }
    }
  }

  // ── Viewport Override (CDP) ────────────────────────────────────────────

  async setViewportOverride(
    browserTabId: string,
    override: { width: number; height: number; deviceScaleFactor: number; mobile: boolean } | null
  ): Promise<boolean> {
    const wcId = this.webContentsIdByTabId.get(browserTabId);
    if (wcId == null) return false;
    const guest = webContents.fromId(wcId);
    if (!guest || guest.isDestroyed()) return false;
    try {
      if (!guest.debugger.isAttached()) {
        guest.debugger.attach('1.3');
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      console.warn("[browser-manager] setViewportOverride: debugger attach failed", { browserTabId, webContentsId: wcId, error: msg });
      return false;
    }
    try {
      if (override) {
        await guest.debugger.sendCommand('Emulation.enable', {});
        await guest.debugger.sendCommand('Emulation.setDeviceMetricsOverride', {
          width: override.width,
          height: override.height,
          deviceScaleFactor: override.deviceScaleFactor,
          mobile: override.mobile,
        });
        if (override.mobile) {
          await guest.debugger.sendCommand('Emulation.setTouchEmulationEnabled', { enabled: true, maxTouchPoints: 5 });
        } else {
          await guest.debugger.sendCommand('Emulation.setTouchEmulationEnabled', { enabled: false, maxTouchPoints: 0 });
        }
      } else {
        await guest.debugger.sendCommand('Emulation.clearDeviceMetricsOverride', {});
        await guest.debugger.sendCommand('Emulation.setTouchEmulationEnabled', { enabled: false, maxTouchPoints: 0 });
      }
      return true;
    } catch { return false; }
  }

  // ── Execute JavaScript in Guest ────────────────────────────────────────

  async executeJavaScript(browserTabId: string, code: string): Promise<unknown> {
    const wcId = this.webContentsIdByTabId.get(browserTabId);
    if (wcId == null) throw new Error('Tab not found');
    const guest = webContents.fromId(wcId);
    if (!guest || guest.isDestroyed()) throw new Error('Guest destroyed');
    return guest.executeJavaScript(code);
  }


  // ---------------------------------------------------------------------------
  // Browser Context Grab — main-owned operations
  // ---------------------------------------------------------------------------

  /**
   * Validates that a caller (identified by sender webContentsId) owns the
   * given browserTabId. Returns the guest WebContents or null.
   */
  getAuthorizedGuest(
    browserTabId: string,
    senderWebContentsId: number
  ): Electron.WebContents | null {
    const registeredRenderer = this.rendererWebContentsIdByTabId.get(browserTabId);
    if (registeredRenderer == null || registeredRenderer !== senderWebContentsId) {
      return null;
    }
    const guestId = this.webContentsIdByTabId.get(browserTabId);
    if (guestId == null) {
      return null;
    }
    const guest = webContents.fromId(guestId);
    if (!guest || guest.isDestroyed()) {
      this.unregisterGuest(browserTabId);
      return null;
    }
    return guest;
  }

  /** Returns true if a grab operation is currently active for this tab. */
  hasActiveGrabOp(browserTabId: string): boolean {
    return this.grabSessionController.hasActiveGrabOp(browserTabId);
  }

  /**
   * Enable or disable grab mode for a browser tab. When enabled, injects the
   * overlay runtime into the guest. When disabled, cancels any active grab op.
   */
  async setGrabMode(
    browserTabId: string,
    enabled: boolean,
    guest: Electron.WebContents
  ): Promise<boolean> {
    if (!enabled) {
      this.cancelGrabOp(browserTabId, 'user');
      return true;
    }
    // Why: injecting the overlay runtime eagerly on arm lets the hover UI
    // appear instantly when the user starts moving the pointer, rather than
    // adding a visible delay between "click Grab" and "overlay appears".
    // The runtime is idempotent — re-injection on the same page is safe.
    try {
      await guest.executeJavaScript(buildGuestOverlayScript('arm'));
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Await a single grab selection on the given tab. Returns a Promise that
   * resolves exactly once when the user clicks, cancels, or an error occurs.
   */
  awaitGrabSelection(
    browserTabId: string,
    opId: string,
    guest: Electron.WebContents
  ): Promise<import('./browser-grab-types.js').BrowserGrabResult> {
    return this.grabSessionController.awaitGrabSelection(browserTabId, opId, guest);
  }

  /**
   * Cancel an active grab operation for the given tab.
   */
  cancelGrabOp(browserTabId: string, reason: BrowserGrabCancelReason): void {
    this.grabSessionController.cancelGrabOp(browserTabId, reason);
  }

  /**
   * Capture a screenshot of the guest surface and optionally crop it to
   * the given CSS-pixel rect.
   */
  async captureSelectionScreenshot(
    _browserTabId: string,
    rect: BrowserGrabRect,
    guest: Electron.WebContents
  ): Promise<BrowserGrabScreenshot | null> {
    return captureGrabSelectionScreenshot(rect, guest);
  }

  /**
   * Extract the payload for the currently hovered element without disrupting
   * the active grab overlay or awaitClick listener. Used by keyboard shortcuts
   * that let the user copy content while hovering, before clicking.
   */
  async extractHoverPayload(
    _browserTabId: string,
    guest: Electron.WebContents
  ): Promise<BrowserGrabPayload | null> {
    try {
      const rawPayload = await guest.executeJavaScript(buildGuestOverlayScript('extractHover'));
      if (!rawPayload || typeof rawPayload !== 'object') {
        return null;
      }
      return clampGrabPayload(rawPayload);
    } catch {
      return null;
    }
  }

  /**
   * Full-page screenshot to clipboard using capturePage() + clipboard.writeImage().
   */
  async screenshotToClipboard(browserTabId: string): Promise<boolean> {
    const wcId = this.webContentsIdByTabId.get(browserTabId);
    if (wcId == null) return false;
    const guest = webContents.fromId(wcId);
    if (!guest || guest.isDestroyed()) return false;
    try {
      const image = await guest.capturePage();
      clipboard.writeImage(image);
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Set the annotation viewport bridge for a browser tab. Injects a script
   * that renders numbered badges on annotated elements that follow page scroll.
   */
  async setAnnotationViewportBridge(
    browserTabId: string,
    options: BrowserAnnotationViewportBridgeOptions
  ): Promise<boolean> {
    const wcId = this.webContentsIdByTabId.get(browserTabId);
    if (wcId == null) return false;
    const guest = webContents.fromId(wcId);
    if (!guest || guest.isDestroyed()) return false;
    const { buildBrowserAnnotationViewportBridgeScript } = await import('./browser-annotation-viewport-bridge.js');
    try {
      await guest.executeJavaScript(buildBrowserAnnotationViewportBridgeScript(options));
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Create a persisted page annotation with a typed comment (fix/change/question/approve),
   * priority, and the grab element context.
   */
  annotateSelection(
    browserPageId: string,
    annotation: {
      id: string;
      comment: string;
      intent: BrowserAnnotationPayload['screenshot'] extends null ? string : string;
      priority: string;
      payload: BrowserAnnotationPayload;
    }
  ): boolean {
    const existing = this.annotationsByPageId.get(browserPageId) ?? [];
    if (existing.length >= 20) {
      return false;
    }
    const entry: BrowserPageAnnotation = {
      id: annotation.id,
      browserPageId,
      comment: annotation.comment,
      intent: annotation.intent as BrowserPageAnnotation['intent'],
      priority: annotation.priority as BrowserPageAnnotation['priority'],
      createdAt: new Date().toISOString(),
      payload: annotation.payload,
    };
    existing.push(entry);
    this.annotationsByPageId.set(browserPageId, existing);
    return true;
  }

  /**
   * Return all persisted annotations for a given page.
   */
  getAnnotations(browserPageId: string): BrowserPageAnnotation[] {
    return this.annotationsByPageId.get(browserPageId) ?? [];
  }

  /**
   * Delete a single annotation by id from a page.
   */
  deleteAnnotation(browserPageId: string, annotationId: string): boolean {
    const existing = this.annotationsByPageId.get(browserPageId);
    if (!existing) return false;
    const idx = existing.findIndex((a) => a.id === annotationId);
    if (idx === -1) return false;
    existing.splice(idx, 1);
    if (existing.length === 0) {
      this.annotationsByPageId.delete(browserPageId);
    }
    return true;
  }


  // ── IPC Setup ──────────────────────────────────────────────────────────

  registerIpcHandlers(ipcMain: Electron.IpcMain): void {
    ipcMain.handle('browser:register-guest', (event, args: {
      browserTabId: string; worktreeId: string; webContentsId: number;
    }) => {
      if (!this.isTrustedBrowserRenderer(event.sender)) return false;
      this.registerGuest({ ...args, rendererWebContentsId: event.sender.id });
      this.flushPendingDownloadRequests(args.browserTabId, args.webContentsId);
      return true;
    });

    ipcMain.handle('browser:unregister-guest', (event, args: { browserTabId: string }) => {
      if (!this.isTrustedBrowserRenderer(event.sender)) return false;
      this.unregisterGuest(args.browserTabId);
      return true;
    });

    ipcMain.handle('browser:accept-download', async (event, args: { downloadId: string }) => {
      const parent = BrowserWindow.fromWebContents(event.sender);
      const download = this.downloadsById.get(args.downloadId);
      const defaultPath = download ? join(app.getPath('downloads'), download.filename) : undefined;
      const result = parent
        ? await dialog.showSaveDialog(parent, { defaultPath })
        : await dialog.showSaveDialog({ defaultPath });
      if (result.canceled || !result.filePath) {
        this.cancelDownload({ downloadId: args.downloadId, senderWebContentsId: event.sender.id });
        return { ok: false, reason: 'canceled' as const };
      }
      return this.acceptDownload({
        downloadId: args.downloadId, senderWebContentsId: event.sender.id, savePath: result.filePath,
      });
    });

    ipcMain.handle('browser:cancel-download', (event, args: { downloadId: string }) => {
      return this.cancelDownload({ downloadId: args.downloadId, senderWebContentsId: event.sender.id });
    });

    ipcMain.handle('browser:open-devtools', (event, args: { browserTabId: string }) => {
      console.log("[browser-manager] open-devtools called", {
        browserTabId: args.browserTabId,
        senderId: event.sender.id,
        trustedId: this.trustedRendererWebContentsId,
        senderType: event.sender.getType(),
      });
      if (!this.isTrustedBrowserRenderer(event.sender)) {
        console.warn("[browser-manager] open-devtools: trust check failed");
        return false;
      }
      const result = this.openDevTools(args.browserTabId);
      console.log("[browser-manager] open-devtools result", result);
      return result;
    });

    ipcMain.handle('browser:set-viewport-override', async (_event, args: {
      browserTabId: string; override: { width: number; height: number; deviceScaleFactor: number; mobile: boolean } | null;
    }) => {
      return this.setViewportOverride(args.browserTabId, args.override);
    });

    ipcMain.handle('browser:execute-javascript', async (event, args: { browserTabId: string; code: string }) => {
      if (!this.isTrustedBrowserRenderer(event.sender)) return { ok: false, error: "not-authorized" } as const;
      try {
        const result = await this.executeJavaScript(args.browserTabId, args.code);
        return { ok: true, result } as const;
      } catch (err) {
        return { ok: false, error: err instanceof Error ? err.message : String(err) } as const;
      }
    });

    ipcMain.handle('browser:get-tab-state', (event, args: { browserTabId: string }) => {
      if (!this.isTrustedBrowserRenderer(event.sender)) return null;
      return this.getTabState(args.browserTabId);
    });

    ipcMain.handle('browser:open-url-external', (_event, args: { url: string }) => {
      const { shell } = require('electron');
      return shell.openExternal(args.url);
    });

    // ── Session Profile IPC ──────────────────────────────────────────

    ipcMain.handle("browser:session:listProfiles", (_event) => {
      return browserSessionRegistry.listProfiles();
    });

    ipcMain.handle("browser:session:createProfile", (_event, args: { scope: string; label: string }) => {
      if (args.scope !== "default" && args.scope !== "worktree" && args.scope !== "global") return null;
      return browserSessionRegistry.createProfile(args.scope, args.label);
    });

    ipcMain.handle("browser:session:deleteProfile", async (_event, args: { profileId: string }) => {
      return browserSessionRegistry.deleteProfile(args.profileId);
    });

    ipcMain.handle("browser:session:resolvePartition", (_event, args: { profileId: string | null }) => {
      return browserSessionRegistry.resolvePartition(args.profileId);
    });

    // ── Cookie Import IPC ─────────────────────────────────────────────

    ipcMain.handle("browser:session:importCookies", async (_event, args: { profileId: string }) => {
      const profile = browserSessionRegistry.getProfile(args.profileId);
      if (!profile) return { ok: false, reason: "Session profile not found." };

      const parent = BrowserWindow.fromWebContents(_event.sender);
      const filePath = await pickCookieFile(parent);
      if (!filePath) return { ok: false, reason: "canceled" };

      const result = await importCookiesFromFile(filePath, profile.partition);
      if (result.ok && 'summary' in result && result.summary) {
        browserSessionRegistry.updateProfileSource(args.profileId, {
          browserFamily: "manual",
          importedAt: Date.now(),
        });
      }
      return result;
    });

    ipcMain.handle("browser:session:clearDefaultCookies", async (_event) => {
      return browserSessionRegistry.clearDefaultSessionCookies();
    });

    ipcMain.handle("browser:session:detectBrowsers", (_event) => {
      return detectInstalledBrowsers().map((b) => ({
        family: b.family,
        label: b.label,
        profiles: b.profiles,
        selectedProfile: b.selectedProfile,
      }));
    });

    ipcMain.handle("browser:session:importFromBrowser", async (_event, args: { profileId: string; browserFamily: string; browserProfile?: string }) => {
      const profile = browserSessionRegistry.getProfile(args.profileId);
      if (!profile) return { ok: false, reason: "Session profile not found." };

      if (args.browserProfile && (/[/\\]/.test(args.browserProfile) || args.browserProfile.includes(".."))) {
        return { ok: false, reason: "Invalid browser profile name." };
      }

      const browsers = detectInstalledBrowsers();
      let browser = browsers.find((b: DetectedBrowser) => b.family === args.browserFamily);
      if (!browser) return { ok: false, reason: "Browser not found on this system." };

      if (args.browserProfile && args.browserProfile !== browser.selectedProfile) {
        const reselected = selectBrowserProfile(browser, args.browserProfile);
        if (!reselected) {
          return { ok: false, reason: "No cookies database found for this profile." };
        }
        browser = reselected;
      }

      const result = await importCookiesFromBrowser(browser, profile.partition);
      if (result.ok && 'summary' in result && result.summary) {
        const profileName = browser.profiles.find((p: { name: string; directory: string }) => p.directory === browser.selectedProfile)?.name ?? browser.selectedProfile;
        browserSessionRegistry.updateProfileSource(args.profileId, {
          browserFamily: browser.family,
          profileName,
          importedAt: Date.now(),
        });
      }
      return result;
    });
    // ── Browser Context Grab IPC ──────────────────────────────────────

    ipcMain.handle(
      'browser:setGrabMode',
      async (event, args: { browserPageId: string; enabled: boolean }): Promise<{ ok: true } | { ok: false; reason: string }> => {
        if (!this.isTrustedBrowserRenderer(event.sender)) {
          return { ok: false, reason: 'not-authorized' };
        }
        const guest = this.getAuthorizedGuest(args.browserPageId, event.sender.id);
        if (!guest) {
          return { ok: false, reason: 'not-ready' };
        }
        const success = await this.setGrabMode(args.browserPageId, args.enabled, guest);
        return success ? { ok: true } : { ok: false, reason: 'not-ready' };
      }
    );

    ipcMain.handle(
      'browser:awaitGrabSelection',
      async (event, args: { browserPageId: string; opId: string }) => {
        if (!this.isTrustedBrowserRenderer(event.sender)) {
          return { opId: args.opId, kind: 'error', reason: 'Not authorized' };
        }
        const guest = this.getAuthorizedGuest(args.browserPageId, event.sender.id);
        if (!guest) {
          return { opId: args.opId, kind: 'error', reason: 'Guest not ready' };
        }
        return this.awaitGrabSelection(args.browserPageId, args.opId, guest);
      }
    );

    ipcMain.handle('browser:cancelGrab', (event, args: { browserPageId: string }): boolean => {
      if (!this.isTrustedBrowserRenderer(event.sender)) {
        return false;
      }
      const guest = this.getAuthorizedGuest(args.browserPageId, event.sender.id);
      if (!guest) {
        return false;
      }
      this.cancelGrabOp(args.browserPageId, 'user');
      return true;
    });

    ipcMain.handle(
      'browser:captureSelectionScreenshot',
      async (event, args: { browserPageId: string; rect: BrowserGrabRect }) => {
        if (!this.isTrustedBrowserRenderer(event.sender)) {
          return { ok: false, reason: 'Not authorized' };
        }
        const guest = this.getAuthorizedGuest(args.browserPageId, event.sender.id);
        if (!guest) {
          return { ok: false, reason: 'Guest not ready' };
        }
        const screenshot = await this.captureSelectionScreenshot(
          args.browserPageId,
          args.rect,
          guest
        );
        if (!screenshot) {
          return { ok: false, reason: 'Screenshot capture failed' };
        }
        return { ok: true, screenshot };
      }
    );

    ipcMain.handle(
      'browser:extractHoverPayload',
      async (event, args: { browserPageId: string }) => {
        if (!this.isTrustedBrowserRenderer(event.sender)) {
          return { ok: false, reason: 'Not authorized' };
        }
        const guest = this.getAuthorizedGuest(args.browserPageId, event.sender.id);
        if (!guest) {
          return { ok: false, reason: 'Guest not ready' };
        }
        const payload = await this.extractHoverPayload(args.browserPageId, guest);
        if (!payload) {
          return { ok: false, reason: 'No element hovered' };
        }
        return { ok: true, payload };
      }
    );

    ipcMain.handle(
      'browser:screenshotToClipboard',
      async (event, args: { browserPageId: string }): Promise<boolean> => {
        if (!this.isTrustedBrowserRenderer(event.sender)) {
          return false;
        }
        const guest = this.getAuthorizedGuest(args.browserPageId, event.sender.id);
        if (!guest) {
          return false;
        }
        return this.screenshotToClipboard(args.browserPageId);
      }
    );

    ipcMain.handle(
      'browser:setAnnotationViewportBridge',
      (event, args: {
        browserPageId: string;
        enabled: boolean;
        emitViewport: boolean;
        markers: import('./browser-annotation-viewport-bridge.js').BrowserAnnotationViewportBridgeMarker[];
        token: string;
      }): Promise<boolean> | boolean => {
        if (!this.isTrustedBrowserRenderer(event.sender)) {
          return false;
        }
        if (
          typeof args?.browserPageId !== 'string' ||
          typeof args.enabled !== 'boolean' ||
          typeof args.emitViewport !== 'boolean' ||
          !Array.isArray(args.markers) ||
          typeof args.token !== 'string'
        ) {
          return false;
        }
        return this.setAnnotationViewportBridge(args.browserPageId, {
          enabled: args.enabled,
          emitViewport: args.emitViewport,
          markers: args.markers,
          token: args.token,
        });
      }
    );

    ipcMain.handle('browser:getAnnotations', (event, args: { browserPageId: string }) => {
      if (!this.isTrustedBrowserRenderer(event.sender)) {
        return [];
      }
      return this.getAnnotations(args.browserPageId);
    });

    ipcMain.handle(
      'browser:annotateSelection',
      (event, args: {
        browserPageId: string;
        id: string;
        comment: string;
        intent: string;
        priority: string;
        payload: BrowserAnnotationPayload;
      }): boolean => {
        if (!this.isTrustedBrowserRenderer(event.sender)) {
          return false;
        }
        return this.annotateSelection(args.browserPageId, args);
      }
    );

    ipcMain.handle(
      'browser:deleteAnnotation',
      (event, args: { browserPageId: string; annotationId: string }): boolean => {
        if (!this.isTrustedBrowserRenderer(event.sender)) {
          return false;
        }
        return this.deleteAnnotation(args.browserPageId, args.annotationId);
      }
    );

  }

  setupSessionPolicies(_app: Electron.App): void {
    if (this.partitionConfigured) return;
    this.partitionConfigured = true;

    const sess = session.fromPartition(CRAB_BROWSER_PARTITION);
    const rawUA = sess.getUserAgent();
    const cleanUA = cleanElectronUserAgent(rawUA);
    sess.setUserAgent(cleanUA);
    setupClientHintsOverride(sess, cleanUA);

    // Permission request handler: auto-grant essential, deny rest
    const autoGranted = new Set(['fullscreen', 'clipboard-read', 'clipboard-sanitized-write']);
    sess.setPermissionRequestHandler((_webContents, permission, callback) => {
      if (autoGranted.has(permission)) {
        callback(true);
      } else {
        this.notifyPermissionDenied(permission, (_webContents as unknown as { id?: number })?.id);
        callback(false);
      }
    });
    sess.setPermissionCheckHandler((_webContents, permission) => autoGranted.has(permission));
    sess.setDisplayMediaRequestHandler((_request, callback) => callback({ video: undefined, audio: undefined }));

    (sess as any).on("certificate-error", (event: any, _webContents: any, _url: any, _error: any, _certificate: any, callback: any) => {
      event.preventDefault();
      callback(false);
      try {
        const win = BrowserWindow.getAllWindows()[0];
        if (win && !win.isDestroyed()) {
          win.webContents.send("browser:certificate-error", {
            browserTabId: "",
            url: _url,
            error: _error,
            certificate: _certificate,
          });
        }
      } catch {}
    });
    // Download handler
    sess.on('will-download', (_event, item, wc) => {
      this.handleGuestWillDownload({ guestWebContentsId: wc.id, item });
    });
  }

  /** Validate that an IPC request comes from the trusted renderer (main window). */
  private isTrustedBrowserRenderer(sender: Electron.WebContents): boolean {
    if (sender.isDestroyed() || sender.getType() !== "window") return false;
    if (this.trustedRendererWebContentsId != null) {
      return sender.id === this.trustedRendererWebContentsId;
    }
    const senderUrl = sender.getURL();
    if (process.env.ELECTRON_RENDERER_URL) {
      try {
        return new URL(senderUrl).origin === new URL(process.env.ELECTRON_RENDERER_URL).origin;
      } catch { return false; }
    }
    return senderUrl.startsWith("file://");
  }

  /** Set the trusted renderer webContents ID for security validation. */
  setTrustedRendererWebContentsId(id: number | null): void {
    this.trustedRendererWebContentsId = id;
  }

  private notifyPermissionDenied(permission: string, webContentsId?: number): void {
    let rendererId: number | null = null;
    if (webContentsId != null) {
      const tabId = this.tabIdByWebContentsId.get(webContentsId);
      if (tabId) rendererId = this.rendererWebContentsIdByTabId.get(tabId) ?? null;
    }
    if (rendererId == null) {
      const win = BrowserWindow.getAllWindows()[0];
      if (win && !win.isDestroyed()) {
        rendererId = win.webContents.id;
      }
    }
    if (rendererId != null) {
      const renderer = webContents.fromId(rendererId);
      if (renderer && !renderer.isDestroyed()) {
        renderer.send('browser:permission-denied', { permission });
      }
    }
  }

  // ── Guest Listeners ────────────────────────────────────────────────────

  private attachGuestListeners(browserTabId: string, webContentsId: number): void {
    const guest = webContents.fromId(webContentsId);
    if (!guest || guest.isDestroyed()) return;

    // Inject anti-detection script via CDP after dom-ready
    let antiDetectionInjected = false;

    const sendEvent = (eventType: string, payload: Partial<CrabBrowserEvent> | Record<string, unknown>): void => {
      const rendererId = this.rendererWebContentsIdByTabId.get(browserTabId);
      if (rendererId == null) return;
      const renderer = webContents.fromId(rendererId);
      if (!renderer || renderer.isDestroyed()) return;
      renderer.send(eventType, { browserTabId, ...payload });
    };

    const injectAntiDetection = (): void => {
      if (antiDetectionInjected) return;
      antiDetectionInjected = true;
      try {
        if (!guest.debugger.isAttached()) {
          guest.debugger.attach('1.3');
        }
        void guest.debugger
          .sendCommand('Page.enable', {})
          .then(() =>
            guest.debugger.sendCommand('Page.addScriptToEvaluateOnNewDocument', {
              source: ANTI_DETECTION_SCRIPT,
            })
          )
          .catch(() => {});
      } catch {}
    };
    // Navigation guard: prevent navigating to dangerous URLs
    const navigationGuard = (event: Electron.Event, url: string): void => {
      if (!isAllowedNavigationUrl(url)) {
        event.preventDefault();
      }
    };
    guest.on("will-navigate", navigationGuard);
    guest.on("will-redirect", navigationGuard);

    // Load failure forwarding to renderer
    const didFailLoadHandler = (
      _event: Electron.Event,
      errorCode: number,
      errorDescription: string,
      validatedURL: string,
      isMainFrame: boolean
    ): void => {
      if (!isMainFrame || errorCode === -3) return;
      sendEvent("browser:load-failure", {
        code: errorCode,
        description: errorDescription || "This site could not be reached.",
        validatedUrl: validatedURL || guest.getURL() || "about:blank",
      });
    };
    guest.on("did-fail-load", didFailLoadHandler);

    // Navigation events
    const handleNavigate = (_e: Electron.Event, url: string): void => {
      sendEvent('browser:navigated', { url });
      antiDetectionInjected = false;
    };
    guest.on('did-navigate', handleNavigate);

    const handleNavigateInPage = (_e: Electron.Event, url: string): void => {
      sendEvent('browser:navigated', { url });
    };
    guest.on('did-navigate-in-page', handleNavigateInPage);

    const handlePageTitle = (_e: Electron.Event, title: string): void => {
      sendEvent('browser:page-title-updated', { title });
    };
    guest.on('page-title-updated', handlePageTitle);

    const handleStartLoading = (): void => {
      sendEvent('browser:did-start-loading', { loading: true });
    };
    guest.on('did-start-loading', handleStartLoading);

    const handleStopLoading = (): void => {
      sendEvent('browser:did-stop-loading', { loading: false });
      try {
        sendEvent('browser:navigation-state', {
          canGoBack: guest.canGoBack(),
          canGoForward: guest.canGoForward(),
        });
      } catch {}
    };
    guest.on('did-stop-loading', handleStopLoading);

    const handleDomReady = (): void => {
      injectAntiDetection();
      try {
        sendEvent('browser:dom-ready', {
          canGoBack: guest.canGoBack(),
          canGoForward: guest.canGoForward(),
        });
      } catch {}
    };
    guest.on('dom-ready', handleDomReady);

    // Favicon update
    const handleFaviconUpdate = (_e: Electron.Event, favicons: string[]): void => {
      const faviconUrl = favicons.length > 0 ? favicons[0] : null;
      sendEvent('browser:page-favicon-updated', { faviconUrl });
    };
    guest.on('page-favicon-updated', handleFaviconUpdate);

    // Context menu
    const handleContextMenu = (_event: Electron.Event, params: Electron.ContextMenuParams): void => {
      sendEvent('browser:context-menu', {
        x: params.x,
        y: params.y,
        pageUrl: guest.getURL() || 'about:blank',
        linkUrl: params.linkURL || null,
        canGoBack: guest.canGoBack ? guest.canGoBack() : false,
        canGoForward: guest.canGoForward ? guest.canGoForward() : false,
      });
    };
    guest.on('context-menu', handleContextMenu);

    // Cleanup on destroy
    const handleDestroyed = (): void => {
      const cleanupListeners = [
        ['did-navigate', handleNavigate],
        ["will-navigate", navigationGuard],
        ["will-redirect", navigationGuard],
        ["did-fail-load", didFailLoadHandler],
        ['did-navigate-in-page', handleNavigateInPage],
        ['page-title-updated', handlePageTitle],
        ['did-start-loading', handleStartLoading],
        ['did-stop-loading', handleStopLoading],
        ['dom-ready', handleDomReady],
        ['page-favicon-updated', handleFaviconUpdate],
        ['context-menu', handleContextMenu],
        ['destroyed', handleDestroyed],
      ] as const;
      for (const [event, handler] of cleanupListeners) {
        try { (guest as any).removeListener(event, handler); } catch {}
      }
      this.unregisterGuest(browserTabId);
    };
    guest.on('destroyed', handleDestroyed);
  }

  // ── Download IPC senders ───────────────────────────────────────────────

  private sendDownloadRequested(download: ActiveDownload): void {
    if (!download.browserTabId) return;
    const rendererId = this.rendererWebContentsIdByTabId.get(download.browserTabId);
    if (!rendererId) return;
    const renderer = webContents.fromId(rendererId);
    if (!renderer || renderer.isDestroyed()) return;
    renderer.send('browser:download-requested', {
      downloadId: download.downloadId,
      browserTabId: download.browserTabId,
      origin: download.origin,
      filename: download.filename,
      totalBytes: download.totalBytes,
      mimeType: download.mimeType,
    } satisfies CrabDownloadRequestedEvent);
  }

  private sendDownloadProgress(download: ActiveDownload): void {
    if (!download.browserTabId || download.rendererWebContentsId == null) return;
    const renderer = webContents.fromId(download.rendererWebContentsId);
    if (!renderer || renderer.isDestroyed()) return;
    renderer.send('browser:download-progress', {
      downloadId: download.downloadId,
      browserTabId: download.browserTabId,
      receivedBytes: download.receivedBytes,
      totalBytes: download.totalBytes ?? 0,
    } satisfies CrabDownloadProgressEvent);
  }

  private sendDownloadFinished(download: ActiveDownload, status: 'completed' | 'canceled' | 'interrupted'): void {
    if (!download.browserTabId || download.rendererWebContentsId == null) return;
    const renderer = webContents.fromId(download.rendererWebContentsId);
    if (!renderer || renderer.isDestroyed()) return;
    renderer.send('browser:download-finished', {
      downloadId: download.downloadId,
      browserTabId: download.browserTabId,
      status,
      savePath: download.savePath,
      error: null,
    } satisfies CrabDownloadFinishedEvent);
  }
}

// ── Singleton Export ────────────────────────────────────────────────────────

export const browserManager = new BrowserManager();
