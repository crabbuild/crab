import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { DropdownMenuItem, PointMenu } from "@/components/ui/dropdown-menu";
import { Textarea } from "@/components/ui/textarea";
import {
  Copy,
  ExternalLink,
  Globe,
  RefreshCw,
  TriangleAlert,
} from "lucide-react";
import {
useCallback,
useEffect,
useMemo,
useRef,
useState,
} from "react";
import {
BrowserContextMenu,
BrowserContextMenuState,
} from "../components/BrowserContextMenu";
import { BrowserFind } from "../components/BrowserFind";
import {
BrowserMobileDriverOverlay
} from "../components/BrowserMobileDriverOverlay";
import { BrowserToolbar } from "../components/BrowserToolbar";
import {
  isBookmarked,
  loadBookmarks,
  toggleBookmark,
  type BookmarkEntry,
} from "../lib/browser-bookmarks";
import {
loadHistory,removeHistoryEntry,
type HistoryEntry
} from "../lib/browser-history";
import {
  CRAB_BROWSER_BLANK_URL,
  DEFAULT_SEARCH_ENGINE,
  normalizeBrowserNavigationUrl,
} from "../lib/browser-url";
import {

BROWSER_VIEWPORT_PRESETS,
getBrowserViewportPreset,
presetToOverride,
type BrowserViewportPresetId,
} from "../lib/browser-viewport-presets";
import {
  destroyAllWebviews,
  destroyWebview,
  registerWebview,
} from "../lib/browser-webview-registry";

// ─── Constants ──────────────────────────────────────────────────────────────

const CRAB_BROWSER_PARTITION = "persist:crab-browser";
const BROWSER_BLANK_URL = "data:text/html,";
const BROWSER_TABS_KEY = "crab.browserTabs.v1";

// ─── Types ──────────────────────────────────────────────────────────────────

interface BrowserLoadError {
  code: number;
  description: string;
  validatedUrl: string;
}

let tabIdCounter = 0;
function generateTabId(): string {
  return `browser-tab-${Date.now()}-${++tabIdCounter}`;
}

// ─── Browser Tab State ──────────────────────────────────────────────────────

interface BrowserTabState {
  id: string;
  url: string;
  title: string;
  loading: boolean;
  canGoBack: boolean;
  canGoForward: boolean;
  faviconUrl: string | null;
  loadError: BrowserLoadError | null;
  /** Timestamp of last activation, for tab ordering */
  lastActivatedAt: number;
}

// ─── Download Tracking ──────────────────────────────────────────────────────

interface TabDownload {
  downloadId: string;
  browserTabId: string;
  filename: string;
  totalBytes: number;
  receivedBytes: number;
  status: "requested" | "downloading" | "completed" | "canceled" | "interrupted";
  savePath: string | null;
  error: string | null;
  startTime: number;
  speed: number;
}

// ─── Persistence ────────────────────────────────────────────────────────────

interface PersistedTab {
  id: string;
  url: string;
  title: string;
}

function loadPersistedTabs(): PersistedTab[] {
  try {
    const raw = localStorage.getItem(BROWSER_TABS_KEY);
    if (!raw) return [];
    return JSON.parse(raw) as PersistedTab[];
  } catch {
    return [];
  }
}

function savePersistedTabs(tabs: BrowserTabState[]): void {
  try {
    const data: PersistedTab[] = tabs.map((t) => ({ id: t.id, url: t.url, title: t.title }));
    localStorage.setItem(BROWSER_TABS_KEY, JSON.stringify(data));
  } catch {
    // best-effort
  }
}

// ─── Component ──────────────────────────────────────────────────────────────

interface BrowserViewProps {
  repoPath?: string;
}

export function BrowserView({ repoPath: _repoPath }: BrowserViewProps) {
  const [tabs, setTabs] = useState<BrowserTabState[]>(() => {
    const persisted = loadPersistedTabs();
    if (persisted.length > 0) {
      return persisted.map((t) => ({
        ...t,
        title: t.title || "New Tab",
        loading: false,
        canGoBack: false,
        canGoForward: false,
        faviconUrl: null,
        loadError: null,
        lastActivatedAt: 0,
      }));
    }
    // Default: start with a blank tab
    const id = generateTabId();
    return [
      {
        id,
        url: "about:blank",
        title: "New Tab",
        loading: false,
        canGoBack: false,
        canGoForward: false,
        faviconUrl: null,
        loadError: null,
        lastActivatedAt: Date.now(),
      },
    ];
  });

  const [activeTabId, setActiveTabId] = useState<string>(() => tabs[0]?.id ?? "");
  const activeTabIdRef = useRef(activeTabId);
  useEffect(() => {
    activeTabIdRef.current = activeTabId;
  }, [activeTabId]);
  const [addressBarValue, setAddressBarValue] = useState("");
  const [findOpen, setFindOpen] = useState(false);
  const [downloads, setDownloads] = useState<Map<string, TabDownload>>(new Map());
  const [contextMenu, setContextMenu] = useState<BrowserContextMenuState | null>(null);
  const [focusAddressBarKey, setFocusAddressBarKey] = useState(0);
  const [permissionNotice, setPermissionNotice] = useState<string | null>(null);
  // Grab mode state
  const [grabModeActive, setGrabModeActive] = useState(false);
  const grabOpIdRef = useRef<string | null>(null);
  const [grabResult, setGrabResult] = useState<any>(null);
  const [grabActionPosition, setGrabActionPosition] = useState<{ x: number; y: number } | null>(null);
  const [showAnnotationDialog, setShowAnnotationDialog] = useState(false);
  const [annotationComment, setAnnotationComment] = useState('');
  const [annotationIntent, setAnnotationIntent] = useState<string>('fix');
  const [annotationPriority, setAnnotationPriority] = useState<string>('important');
  // Annotations list for the current page
  const [pageAnnotations, setPageAnnotations] = useState<any[]>([]);
  // Zoom, viewport, profile state
  const [zoomLevel, setZoomLevel] = useState(1.0);
  const [viewportPresetId, setViewportPresetId] = useState<BrowserViewportPresetId | null>(null);
  const [sessionProfiles, setSessionProfiles] = useState<Array<{ id: string; label: string; scope: string }>>([]);
  const [activeProfileId, setActiveProfileId] = useState<string | null>(null);
  const [showProfileManager, setShowProfileManager] = useState(false);
  const [showHistoryPanel, setShowHistoryPanel] = useState(false);
  const [historyEntries, setHistoryEntries] = useState<HistoryEntry[]>(() => loadHistory());
  const [bookmarks, setBookmarks] = useState<BookmarkEntry[]>(() => loadBookmarks());

  // Refs keyed by tab ID for webview elements and registered state
  const webviewRefs = useRef<Map<string, Electron.WebviewTag>>(new Map());
  const containerRefs = useRef<Map<string, HTMLDivElement>>(new Map());
  const registeredTabs = useRef<Set<string>>(new Set());

  // ── Tab Management ────────────────────────────────────────────────────

  const activeTab = useMemo(
    () => tabs.find((t) => t.id === activeTabId) ?? null,
    [tabs, activeTabId]
  );

  const updateTab = useCallback(
    (tabId: string, updates: Partial<BrowserTabState>) => {
      setTabs((prev) =>
        prev.map((t) => (t.id === tabId ? { ...t, ...updates } : t))
      );
    },
    []
  );

  const createTab = useCallback((url?: string) => {
    const id = generateTabId();
    const targetUrl = url ?? BROWSER_BLANK_URL;
    const newTab: BrowserTabState = {
      id,
      url: targetUrl,
      title: "New Tab",
      loading: url ? true : false,
      canGoBack: false,
      canGoForward: false,
      faviconUrl: null,
      loadError: null,
      lastActivatedAt: Date.now(),
    };
    setTabs((prev) => [...prev, newTab]);
    setActiveTabId(id);
    setAddressBarValue(url ? targetUrl : "");
    setFocusAddressBarKey((k) => k + 1);
    return id;
  }, []);

  const closeTab = useCallback(

    (tabId: string) => {
      destroyWebview(tabId);
      webviewRefs.current.delete(tabId);
      containerRefs.current.delete(tabId);
      registeredTabs.current.delete(tabId);

      setTabs((prev) => {
        const remaining = prev.filter((t) => t.id !== tabId);
        if (remaining.length === 0) {
          // Create a new blank tab instead of closing the last one
          const id = generateTabId();
          const newTab: BrowserTabState = {
            id,
            url: BROWSER_BLANK_URL,
            title: "New Tab",
            loading: false,
            canGoBack: false,
            canGoForward: false,
            faviconUrl: null,
            loadError: null,
            lastActivatedAt: Date.now(),
          };
          setActiveTabId(id);
          setAddressBarValue("");
          return [newTab];
        }
        if (activeTabId === tabId) {
          // Switch to the nearest tab
          const idx = prev.findIndex((t) => t.id === tabId);
          const nextIdx = Math.min(idx, remaining.length - 1);
          setActiveTabId(remaining[nextIdx].id);
          const switchedUrl = remaining[nextIdx].url;
          setAddressBarValue(
            switchedUrl === BROWSER_BLANK_URL || switchedUrl === "about:blank" ? "" : switchedUrl
          );
        }
        return remaining;
      });
    },
    [activeTabId]
  );

  const switchTab = useCallback(
    (tabId: string) => {
      const tab = tabs.find((t) => t.id === tabId);
      if (!tab) return;
      setActiveTabId(tabId);
      setAddressBarValue(
        tab.url === BROWSER_BLANK_URL || tab.url === "about:blank" ? "" : tab.url
      );
      setFindOpen(false);
      updateTab(tabId, { lastActivatedAt: Date.now() });
    },
    [tabs, updateTab]
  );

  const navigateTab = useCallback(
    (tabId: string, url: string) => {
      const wv = webviewRefs.current.get(tabId);
      if (wv?.isConnected) {
        try {
          wv.loadURL(url);
        } catch {
          // If loadURL fails, try navigating
          wv.src = url;
        }
      }
      updateTab(tabId, { url, loading: true, loadError: null });
      setAddressBarValue(url);
    },
    [updateTab]
  );

  // ── WebView Lifecycle ─────────────────────────────────────────────────

  const createWebview = useCallback(
    (tabId: string, container: HTMLDivElement) => {
      // Check if webview already exists for this tab
      let wv = webviewRefs.current.get(tabId);
      if (wv) {
        // Webview already exists, just move it to the new container
        if (wv.parentNode !== container) {
          container.appendChild(wv);
        }
        return wv;
      }

      // Create new webview
      wv = document.createElement("webview") as Electron.WebviewTag;
      wv.setAttribute("partition", CRAB_BROWSER_PARTITION);
      wv.setAttribute("allowpopups", "");
      wv.style.display = "flex";
      wv.style.flex = "1";
      wv.style.width = "100%";
      wv.style.height = "100%";
      wv.style.border = "none";
      wv.style.background = "#ffffff";

      // Attach dom-ready listener BEFORE appendChild to avoid race
      // where about:blank dom-ready fires synchronously before the listener is registered.
      const handleDomReady = (): void => {
        void (async () => {
          try {
            const wcId = wv.getWebContentsId();
            if (!registeredTabs.current.has(tabId)) {
              registeredTabs.current.add(tabId);
              registerWebview(tabId, wv);
              await window.crab.browser
                .registerGuest({
                  browserTabId: tabId,
                  worktreeId: _repoPath ?? "default",
                  webContentsId: wcId,
                })
                .catch(() => {});
            }

            // Sync navigation state
            try {
              updateTab(tabId, {
                canGoBack: wv.canGoBack(),
                canGoForward: wv.canGoForward(),
              });
            } catch {
              // best-effort
            }
          } catch {
            // best-effort
          }
        })();
      };

      wv.addEventListener("dom-ready", handleDomReady);

      container.appendChild(wv);
      webviewRefs.current.set(tabId, wv);

      const handleDidNavigate = (e: any): void => {
        const url = e.url;
        try {
          updateTab(tabId, {
            url,
            loading: false,
            canGoBack: wv.canGoBack(),
            canGoForward: wv.canGoForward(),
          });
        } catch {
          updateTab(tabId, { url, loading: false });
        }
        if (tabId === activeTabIdRef.current) {
          setAddressBarValue(url);
        }
      };

      const handleDidNavigateInPage = (e: any): void => {
        const url = e.url;
        try {
          updateTab(tabId, {
            url,
            canGoBack: wv.canGoBack(),
            canGoForward: wv.canGoForward(),
          });
        } catch {
          updateTab(tabId, { url });
        }
        if (tabId === activeTabIdRef.current) {
          setAddressBarValue(url);
        }
      };

      const handlePageTitleUpdated = (e: any): void => {
        updateTab(tabId, { title: e.title });
      };

      const handleStartLoading = (): void => {
        updateTab(tabId, { loading: true, loadError: null });
      };

      const handleStopLoading = (): void => {
        try {
          const title = wv.getTitle();
          const url = wv.getURL();
          updateTab(tabId, {
            loading: false,
            canGoBack: wv.canGoBack(),
            canGoForward: wv.canGoForward(),
            title: title || undefined,
            url: url || undefined,
          });
          if (tabId === activeTabIdRef.current && url) {
            setAddressBarValue(url);
          }
        } catch {
          updateTab(tabId, { loading: false });
        }
      };

      const handleDidFailLoad = (e: any): void => {
        if (e.errorCode === -3) {
          return;
        }
        updateTab(tabId, {
          loading: false,
          loadError: {
            code: e.errorCode ?? 0,
            description: e.errorDescription ?? "Failed to load page",
            validatedUrl: e.validatedURL ?? "",
          },
        });
      };

      const handleNewWindow = (e: any): void => {
        const url = e.url;
        if (url) {
          createTab(url);
        }
      };

      const handleFoundInPage = (): void => {};

      const handleFaviconUpdate = (e: any): void => {
        const favicons = e.favicons;
        if (favicons && favicons.length > 0) {
          updateTab(tabId, { faviconUrl: favicons[0] });
        }
      };

      wv.addEventListener("did-navigate", handleDidNavigate);
      wv.addEventListener("did-navigate-in-page", handleDidNavigateInPage);
      wv.addEventListener("page-title-updated", handlePageTitleUpdated);
      wv.addEventListener("did-start-loading", handleStartLoading);
      wv.addEventListener("did-stop-loading", handleStopLoading);
      wv.addEventListener("did-fail-load", handleDidFailLoad);
      wv.addEventListener("new-window", handleNewWindow);
      wv.addEventListener("found-in-page", handleFoundInPage);
      wv.addEventListener("page-favicon-updated", handleFaviconUpdate);

      // Navigate to the tab's URL if it has one
      const tab = tabs.find((t) => t.id === tabId);
      if (tab && tab.url && tab.url !== "about:blank" && tab.url !== BROWSER_BLANK_URL) {
        wv.src = tab.url;
        // Sync address bar to the navigated URL (not the blank placeholder)
        setAddressBarValue(tab.url);
      }

      return wv;
    },
    [_repoPath, createTab, tabs, updateTab]
  );

  // ── WebView Container Refs ────────────────────────────────────────────

  const setContainerRef = useCallback(
    (tabId: string) => (node: HTMLDivElement | null) => {
      if (!node) {
        containerRefs.current.delete(tabId);
        return;
      }
      containerRefs.current.set(tabId, node);
      createWebview(tabId, node);
    },
    [createWebview]
  );

  // ── Navigation Handlers ───────────────────────────────────────────────

  const handleGoBack = useCallback((tabId: string) => {
    const wv = webviewRefs.current.get(tabId);
    if (wv && wv.canGoBack()) {
      wv.goBack();
    }
  }, []);

  const handleGoForward = useCallback((tabId: string) => {
    const wv = webviewRefs.current.get(tabId);
    if (wv && wv.canGoForward()) {
      wv.goForward();
    }
  }, []);

  const handleReload = useCallback((tabId: string) => {
    const wv = webviewRefs.current.get(tabId);
    if (wv) {
      wv.reload();
    }
  }, []);

  const handleStop = useCallback((tabId: string) => {
    const wv = webviewRefs.current.get(tabId);
    if (wv) {
      wv.stop();
    }
  }, []);

  const handleScreenshot = useCallback(() => {
    if (!activeTabId) return;
    void window.crab.browser.screenshotToClipboard({ browserPageId: activeTabId });
  }, [activeTabId]);

  const handleToggleGrab = useCallback(() => {
    if (!activeTabId) return;
    const next = !grabModeActive;
    if (next) {
      // Arm grab mode
      void window.crab.browser.setGrabMode({ browserPageId: activeTabId, enabled: true }).then((r) => {
        if (r.ok) {
          setGrabModeActive(true);
          const opId = `grab-${Date.now()}-${Math.random().toString(36).slice(2, 9)}`;
          grabOpIdRef.current = opId;
          // Start awaiting a selection
          void window.crab.browser.awaitGrabSelection({ browserPageId: activeTabId, opId }).then((result: any) => {
            if (result.kind === 'selected') {
              // Left-click: show the grab action popover with copy/annotate options
              setGrabResult(result.payload);
              setGrabActionPosition({ x: window.innerWidth / 2, y: window.innerHeight / 2 });
            } else if (result.kind === 'context-selected') {
              // Right-click: show annotation dialog immediately
              setGrabResult(result.payload);
              setShowAnnotationDialog(true);
              setGrabActionPosition(null);
            } else {
              // Cancelled or error
              setGrabResult(null);
              setGrabActionPosition(null);
            }
            setGrabModeActive(false);
            grabOpIdRef.current = null;
          }).catch(() => {
            setGrabModeActive(false);
            grabOpIdRef.current = null;
            setGrabResult(null);
            setGrabActionPosition(null);
          });
        }
      });
    } else {
      // Disarm grab mode
      void window.crab.browser.cancelGrab({ browserPageId: activeTabId }).then(() => {
        void window.crab.browser.setGrabMode({ browserPageId: activeTabId, enabled: false });
        setGrabModeActive(false);
        grabOpIdRef.current = null;
        setGrabResult(null);
        setGrabActionPosition(null);
      });
    }
  }, [activeTabId, grabModeActive]);

  // Handle keyboard shortcuts during grab mode
  const handleGrabCopyText = useCallback(() => {
    if (!grabResult?.target?.textSnippet || !activeTabId) return;
    void navigator.clipboard.writeText(grabResult.target.textSnippet).catch(() => {});
    setGrabResult(null);
    setGrabActionPosition(null);
  }, [grabResult, activeTabId]);

  const handleGrabCopyHTML = useCallback(() => {
    if (!grabResult?.target?.htmlSnippet || !activeTabId) return;
    void navigator.clipboard.writeText(grabResult.target.htmlSnippet).catch(() => {});
    setGrabResult(null);
    setGrabActionPosition(null);
  }, [grabResult, activeTabId]);

  const handleGrabScreenshot = useCallback(() => {
    if (!grabResult?.target || !activeTabId) return;
    void window.crab.browser.captureSelectionScreenshot({
      browserPageId: activeTabId,
      rect: grabResult.target.rectViewport,
    });
    setGrabResult(null);
    setGrabActionPosition(null);
  }, [grabResult, activeTabId]);

  const handleCreateAnnotation = useCallback(() => {
    if (!grabResult || !activeTabId) return;
    const id = `annotation-${Date.now()}-${Math.random().toString(36).slice(2, 9)}`;
    void window.crab.browser.annotateSelection({
      browserPageId: activeTabId,
      id,
      comment: annotationComment,
      intent: annotationIntent,
      priority: annotationPriority,
      payload: { ...grabResult, screenshot: null },
    }).then(() => {
      // Refresh annotations and badges
      void window.crab.browser.getAnnotations({ browserPageId: activeTabId }).then((anns: any) => {
        setPageAnnotations(anns);
        // Update annotation viewport bridge
        const markers = (anns || []).map((a: any, i: number) => ({
          id: a.id,
          index: i,
          rectPage: a.payload.target.rectPage,
          rectViewport: a.payload.target.rectViewport,
          isFixed: a.payload.target.isFixed || false,
        }));
        void window.crab.browser.setAnnotationViewportBridge({
          browserPageId: activeTabId,
          enabled: markers.length > 0,
          emitViewport: true,
          markers,
          token: `grab-token-${activeTabId}`,
        });
      });
      setShowAnnotationDialog(false);
      setAnnotationComment('');
      setGrabResult(null);
    });
  }, [grabResult, activeTabId, annotationComment, annotationIntent, annotationPriority]);

  const handleDeleteAnnotation = useCallback((annotationId: string) => {
    if (!activeTabId) return;
    void window.crab.browser.deleteAnnotation({
      browserPageId: activeTabId,
      annotationId,
    }).then(() => {
      void window.crab.browser.getAnnotations({ browserPageId: activeTabId }).then((anns: any) => {
        setPageAnnotations(anns);
        const markers = (anns || []).map((a: any, i: number) => ({
          id: a.id,
          index: i,
          rectPage: a.payload.target.rectPage,
          rectViewport: a.payload.target.rectViewport,
          isFixed: a.payload.target.isFixed || false,
        }));
        void window.crab.browser.setAnnotationViewportBridge({
          browserPageId: activeTabId,
          enabled: markers.length > 0,
          emitViewport: true,
          markers,
          token: `grab-token-${activeTabId}`,
        });
      });
    });
  }, [activeTabId]);

  const handleDismissGrab = useCallback(() => {
    setGrabResult(null);
    setGrabActionPosition(null);
    setShowAnnotationDialog(false);
    setAnnotationComment('');
  }, []);

  const handleOpenDevTools = useCallback((tabId: string) => {
    console.log("[BrowserView] handleOpenDevTools called", { tabId });
    window.crab.browser.openDevtools({ browserTabId: tabId })
      .then((result) => console.log("[BrowserView] openDevtools resolved", result))
      .catch((err) => console.warn("[BrowserView] openDevtools rejected", err));
  }, []);

  const handleOpenExternally = useCallback((url: string) => {
    void window.crab.browser.openUrlExternal({ url });
  }, []);

  const handleCopyUrl = useCallback((url: string) => {
    void navigator.clipboard.writeText(url).catch(() => {});
  }, []);

  const handleToggleBookmark = useCallback((url: string, title: string) => {
    setBookmarks((prev) => toggleBookmark(url, title, prev));
  }, []);

  // ── Address bar submit (handled in the component) ─────────────────────

  const handleAddressBarSubmit = useCallback(() => {
    if (!activeTabId) return;
    const raw = addressBarValue.trim();
    if (!raw) return;
    const url = normalizeBrowserNavigationUrl(raw, DEFAULT_SEARCH_ENGINE);
    if (!url || url === CRAB_BROWSER_BLANK_URL) return;
    navigateTab(activeTabId, url);
    setAddressBarValue(url);
  }, [activeTabId, addressBarValue, navigateTab, setAddressBarValue]);

  // ── Zoom ──────────────────────────────────────────────────────────────

  const handleZoomIn = useCallback(() => {
    setZoomLevel((z) => {
      const next = Math.min(z + 0.1, 3.0);
      const wv = activeTabId ? webviewRefs.current.get(activeTabId) : null;
      if (wv) wv.setZoomLevel(Math.log2(next));
      return next;
    });
  }, [activeTabId]);

  const handleZoomOut = useCallback(() => {
    setZoomLevel((z) => {
      const next = Math.max(z - 0.1, 0.25);
      const wv = activeTabId ? webviewRefs.current.get(activeTabId) : null;
      if (wv) wv.setZoomLevel(Math.log2(next));
      return next;
    });
  }, [activeTabId]);

  const handleZoomReset = useCallback(() => {
    setZoomLevel(1.0);
    const wv = activeTabId ? webviewRefs.current.get(activeTabId) : null;
    if (wv) wv.setZoomLevel(0);
  }, [activeTabId]);

  // ── Viewport Presets ──────────────────────────────────────────────────

  const handleSetViewportPreset = useCallback((id: BrowserViewportPresetId | null) => {
    setViewportPresetId(id);
    if (!activeTabId) return;
    const preset = id ? getBrowserViewportPreset(id) : null;
    const override = preset ? presetToOverride(preset) : null;
    // Resize the webview element to match preset dimensions so the user
    // sees the actual viewport size, not just CDP internal emulation.
    const wv = webviewRefs.current.get(activeTabId);
    if (wv) {
      if (preset && preset.width > 0 && preset.height > 0) {
        wv.style.width = preset.width + "px";
        wv.style.height = preset.height + "px";
        wv.style.margin = "0 auto";
        wv.style.flex = "none";
      } else {
        wv.style.width = "100%";
        wv.style.height = "100%";
        wv.style.margin = "";
        wv.style.flex = "1";
      }
    }
    window.crab.browser.setViewportOverride({ browserTabId: activeTabId, override }).catch(() => {});
  }, [activeTabId]);

  // ── Session Profiles ──────────────────────────────────────────────────

  const loadSessionProfiles = useCallback(async () => {
    try {
      const profiles = await window.crab.browser.session.listProfiles();
      setSessionProfiles(profiles.map((p: any) => ({ id: p.id, label: p.label, scope: p.scope })));
    } catch {
      // best-effort
    }
  }, []);

  const handleSelectProfile = useCallback((profileId: string | null) => {
    setActiveProfileId(profileId);
  }, []);

  const handleCreateProfile = useCallback(async () => {
    try {
      const label = prompt("Enter a name for the new profile:");
      if (!label) return;
      const profile = await window.crab.browser.session.createProfile({ scope: "default", label });
      if (profile) loadSessionProfiles();
    } catch {
      // best-effort
    }
  }, [loadSessionProfiles]);

  const handleDeleteProfile = useCallback(async (profileId: string) => {
    try {
      await window.crab.browser.session.deleteProfile({ profileId });
      if (activeProfileId === profileId) setActiveProfileId(null);
      loadSessionProfiles();
    } catch {
      // best-effort
    }
  }, [activeProfileId, loadSessionProfiles]);

  const handleImportCookies = useCallback(async (profileId: string) => {
    try {
      const result = await window.crab.browser.session.importCookies({ profileId });
      if (typeof result === "object" && result && "ok" in result && result.ok && "summary" in result && result.summary) {
        const s = result.summary as any;
        setPermissionNotice("Imported " + s.imported + " cookies (" + s.skipped + " skipped)");
        setTimeout(() => setPermissionNotice(null), 4000);
      } else if (typeof result === "object" && result && "reason" in result && result.reason) {
        setPermissionNotice("Cookie import: " + (result as any).reason);
        setTimeout(() => setPermissionNotice(null), 4000);
      }
    } catch {
      setPermissionNotice("Cookie import failed.");
      setTimeout(() => setPermissionNotice(null), 4000);
    }
  }, []);

  const handleManageProfiles = useCallback(() => {
    setShowProfileManager((v) => !v);
    loadSessionProfiles();
  }, [loadSessionProfiles]);

  // ── IPC Event Handlers ────────────────────────────────────────────────

  useEffect(() => {
    // Download requested — prompt save dialog
    const unsubDownloadReq = window.crab.onBrowserDownloadRequested?.(
      async (payload) => {
        const result = await window.crab.browser.acceptDownload({ downloadId: payload.downloadId });
        if (result.ok) {
          setDownloads((prev) => {
            const next = new Map(prev);
            next.set(payload.downloadId, {
              downloadId: payload.downloadId,
              browserTabId: payload.browserTabId,
              filename: payload.filename,
              totalBytes: payload.totalBytes ?? 0,
              receivedBytes: 0,
              status: "downloading",
              savePath: null,
              error: null,
              startTime: Date.now(),
              speed: 0,
            });
            return next;
          });
        }
      }
    );

    // Download progress
    const unsubDownloadProgress = window.crab.onBrowserDownloadProgress?.(
      (payload) => {
        setDownloads((prev) => {
          const d = prev.get(payload.downloadId);
          if (!d) return prev;
          const elapsed = (Date.now() - d.startTime) / 1000;
          const bytesDelta = payload.receivedBytes - d.receivedBytes;
          const speed = elapsed > 0.5 ? Math.round(bytesDelta / elapsed) : d.speed || 0;
          const next = new Map(prev);
          next.set(payload.downloadId, {
            ...d,
            receivedBytes: payload.receivedBytes,
            totalBytes: payload.totalBytes,
            speed,
            status: "downloading" as const,
          });
          return next;
        });
      }
    );

    // Download finished
    const unsubDownloadFinished = window.crab.onBrowserDownloadFinished?.(
      (payload) => {
        setDownloads((prev) => {
          const d = prev.get(payload.downloadId);
          if (!d) return prev;
          const next = new Map(prev);
          next.set(payload.downloadId, {
            ...d,
            status: payload.status as "completed" | "canceled" | "interrupted",
            savePath: payload.savePath,
            error: payload.error,
          });
          return next;
        });
        setTimeout(() => {
          setDownloads((prev) => {
            const next = new Map(prev);
            next.delete(payload.downloadId);
            return next;
          });
        }, 6000);
      },
    );

    // Context menu
    const unsubContextMenu = window.crab.onBrowserContextMenu?.(
      (payload) => {
        setContextMenu(payload);
      }
    );

    // Certificate error — surface as notification
    const unsubCertificateError = window.crab.onBrowserCertificateError?.(
      (payload) => {
        setPermissionNotice(
          `Certificate error for ${payload.url}: ${payload.error}`
        );
        setTimeout(() => setPermissionNotice(null), 8000);
      }
    );

    // Permission denied
    const unsubPermissionDenied = window.crab.onBrowserPermissionDenied?.(
      (payload) => {
        setPermissionNotice(
          `Permission denied: ${humanizePermission(payload.permission)}`
        );
        setTimeout(() => setPermissionNotice(null), 4000);
      }
    );

    // Load session profiles
    loadSessionProfiles();

    // Tab persistence
    const saveInterval = setInterval(() => {
      setTabs((current) => {
        savePersistedTabs(current);
        return current;
      });
    }, 3000);

    return () => {
      unsubDownloadReq?.();
      unsubDownloadProgress?.();
      unsubDownloadFinished?.();
      unsubContextMenu?.();
      unsubPermissionDenied?.();
      unsubCertificateError?.();
      clearInterval(saveInterval);
    };
  }, []);

  // Save tabs on unmount
  useEffect(() => {
    return () => {
      savePersistedTabs(tabs);
      destroyAllWebviews();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Set up grab action shortcut listener (must be after callbacks are defined)
  useEffect(() => {
    const unsub = window.crab.onGrabActionShortcut?.(
      (payload: { browserPageId: string; key: string }) => {
        if (payload.key === 'c' || payload.key === 'C') {
          // Access latest values via refs to avoid stale closures
          if (grabResult?.target?.textSnippet && activeTabId) {
            void navigator.clipboard.writeText(grabResult.target.textSnippet).catch(() => {});
            setGrabResult(null);
            setGrabActionPosition(null);
          }
        } else if (payload.key === 's' || payload.key === 'S') {
          if (grabResult?.target && activeTabId) {
            void window.crab.browser.captureSelectionScreenshot({
              browserPageId: activeTabId,
              rect: grabResult.target.rectViewport,
            });
            setGrabResult(null);
            setGrabActionPosition(null);
          }
        }
      }
    );
    return () => { if (unsub) unsub(); };
  }, [grabResult, activeTabId]);

  // ── Window Custom Events ──────────────────────────────────────────────

  useEffect(() => {
    const handleNewTab = () => createTab();
    const handleSetActiveView = (e: Event) => {
      const detail = (e as CustomEvent).detail;
      if (detail?.view === "browser") {
        setFocusAddressBarKey((k) => k + 1);
      }
    };

    window.addEventListener("crab:browser-new-tab", handleNewTab);
    window.addEventListener("crab:set-active-view", handleSetActiveView);

    return () => {
      window.removeEventListener("crab:browser-new-tab", handleNewTab);
      window.removeEventListener("crab:set-active-view", handleSetActiveView);
    };
  }, [createTab]);

  // ── Keyboard Shortcuts ────────────────────────────────────────────────

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      // Cmd+T / Ctrl+T: New tab
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && e.key === "t") {
        e.preventDefault();
        createTab();
      }
      // Cmd+W / Ctrl+W: Close tab
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && e.key === "w") {
        e.preventDefault();
        if (activeTabId) closeTab(activeTabId);
      }
      // Cmd+F / Ctrl+F: Find
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && e.key === "f") {
        e.preventDefault();
        setFindOpen((v) => !v);
      }
      // Cmd+R / Ctrl+R: Reload
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && e.key === "r") {
        e.preventDefault();
        if (activeTabId) handleReload(activeTabId);
      }
      // Cmd+Shift+R / Ctrl+Shift+R: Hard reload
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && e.key === "r") {
        e.preventDefault();
        const wv = activeTabId ? webviewRefs.current.get(activeTabId) : null;
        if (wv) {
          registeredTabs.current.delete(activeTabId!);
          wv.reloadIgnoringCache();
        }
      }
      // Alt+Cmd+Left/Right: Tab switch
      if ((e.metaKey || e.ctrlKey) && e.altKey) {
        if (e.key === "ArrowRight") {
          e.preventDefault();
          const idx = tabs.findIndex((t) => t.id === activeTabId);
          if (idx < tabs.length - 1) switchTab(tabs[idx + 1].id);
        } else if (e.key === "ArrowLeft") {
          e.preventDefault();
          const idx = tabs.findIndex((t) => t.id === activeTabId);
          if (idx > 0) switchTab(tabs[idx - 1].id);
        }
      // Cmd+= / Ctrl+=: Zoom in
      if ((e.metaKey || e.ctrlKey) && (e.key === "=" || e.key === "+")) {
        e.preventDefault();
        handleZoomIn();
      }
      // Cmd+- / Ctrl+-: Zoom out
      if ((e.metaKey || e.ctrlKey) && e.key === "-") {
        e.preventDefault();
        handleZoomOut();
      }
      // Cmd+0 / Ctrl+0: Reset zoom
      if ((e.metaKey || e.ctrlKey) && e.key === "0") {
        e.preventDefault();
        handleZoomReset();
      }
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [activeTabId, tabs, createTab, closeTab, switchTab, handleReload, handleZoomIn, handleZoomOut, handleZoomReset]);

  // ── Download Notice Auto-Dismiss ──────────────────────────────────────

  // ── Derived State ─────────────────────────────────────────────────────

  const isBlankTab =
    activeTab?.url === "about:blank" || activeTab?.url === BROWSER_BLANK_URL;
  const showFailureOverlay =
    Boolean(activeTab?.loadError) && !isBlankTab;

  const loadErrorMeta = useMemo(() => {
    const err = activeTab?.loadError;
    if (!err) return { host: null, isLocalhostLike: false };
    try {
      const parsed = new URL(err.validatedUrl);
      const hostname = parsed.hostname;
      return {
        host: parsed.host || null,
        isLocalhostLike:
          hostname === "localhost" ||
          hostname === "127.0.0.1" ||
          hostname === "0.0.0.0" ||
          hostname === "::1",
      };
    } catch {
      return { host: null, isLocalhostLike: false };
    }
  }, [activeTab?.loadError]);

  const formatLoadFailureDescription = useCallback(() => {
    const err = activeTab?.loadError;
    if (!err) return "The page did not respond.";
    if (loadErrorMeta.isLocalhostLike) return "We couldn't connect to your local server.";
    if (err.code === 0) return err.description;
    return "We couldn't connect to this page.";
  }, [activeTab?.loadError, loadErrorMeta.isLocalhostLike]);

  const loadErrorHint = loadErrorMeta.isLocalhostLike
    ? "If this should be a local app, make sure the server is running and listening on the expected port."
    : null;

  const urlProtocol = useMemo(() => {
    try {
      return new URL(activeTab?.url ?? "").protocol;
    } catch {
      return "";
    }
  }, [activeTab?.url]);

  const isSecure = urlProtocol === "https:";

  // ── Render ────────────────────────────────────────────────────────────

  return (
    <div className="flex h-full w-full flex-col bg-background">
      <BrowserToolbar
        tabs={tabs.map((t) => ({
          id: t.id,
          url: t.url,
          title: t.title,
          loading: t.loading,
          canGoBack: t.canGoBack,
          canGoForward: t.canGoForward,
          faviconUrl: t.faviconUrl,
        }))}
        activeTabId={activeTabId}
        onSelectTab={switchTab}
        onCloseTab={closeTab}
        onCreateTab={() => createTab()}
        onNavigate={navigateTab}
        onGoBack={handleGoBack}
        onGoForward={handleGoForward}
        onReload={handleReload}
        onStop={handleStop}
        addressBarValue={addressBarValue}
        onAddressBarChange={setAddressBarValue}
        onAddressBarSubmit={handleAddressBarSubmit}
        findOpen={findOpen}
        onToggleFind={() => setFindOpen((v) => !v)}
        onOpenDevTools={handleOpenDevTools}
        onScreenshot={handleScreenshot}
        onToggleGrab={handleToggleGrab}
        grabModeActive={grabModeActive}
        focusAddressBarKey={focusAddressBarKey}
        isSecure={isSecure}
        isBookmarked={activeTab ? isBookmarked(activeTab.url, bookmarks) : false}
        onToggleBookmark={activeTab ? () => handleToggleBookmark(activeTab.url, activeTab.title) : undefined}
        zoomLevel={zoomLevel}
        onZoomIn={handleZoomIn}
        onZoomOut={handleZoomOut}
        onZoomReset={handleZoomReset}
        viewportPresetId={viewportPresetId}
        viewportPresets={BROWSER_VIEWPORT_PRESETS.map(p => ({ id: p.id, label: p.label }))}
        onSetViewportPreset={(id: string | null) => handleSetViewportPreset(id as BrowserViewportPresetId | null)}
        sessionProfiles={sessionProfiles}
        activeProfileId={activeProfileId}
        onSelectProfile={handleSelectProfile}
        onCreateProfile={handleCreateProfile}
        onManageProfiles={handleManageProfiles}
      />

      {/* WebView area */}
      <div className="relative flex min-h-0 flex-1 overflow-hidden">
        {/* WebView area */}
        {activeTab?.loading && !showFailureOverlay ? (
          <div className="pointer-events-none absolute top-0 left-0 right-0 z-30 h-0.5">
            <div className="h-full w-full animate-browser-loading bg-primary opacity-80" />
          </div>
        ) : null}

        {/* WebView area */}
        {tabs.map((tab) => (
          <div
            key={tab.id}
            ref={setContainerRef(tab.id)}
            className={
              tab.id === activeTabId ? "absolute inset-0 flex" : "hidden"
            }
          />
        ))}

        {/* WebView area */}
        {showHistoryPanel ? (
          <div className="absolute inset-0 z-30 flex bg-background">
            <div className="flex h-full w-full flex-col">
              <div className="flex items-center justify-between border-b border-border px-4 py-2">
                <h3 className="text-sm font-semibold text-foreground">
                  History
                </h3>
                <div className="flex items-center gap-2">
                  <Button
                    variant="ghost"
                    size="compact"
                    type="button"
                    className="rounded px-2 py-1 text-[11px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
                    onClick={() => { setHistoryEntries([]); setShowHistoryPanel(false); }}
                  >
                    Clear
                  </Button>
                  <Button
                    variant="ghost"
                    size="compact"
                    type="button"
                    className="rounded px-2 py-1 text-[11px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
                    onClick={() => setShowHistoryPanel(false)}
                  >
                    Close
                  </Button>
                </div>
              </div>
              <div className="flex-1 overflow-y-auto">
                {historyEntries.length === 0 ? (
                  <div className="flex h-full items-center justify-center text-xs text-muted-foreground">
                    No history yet
                  </div>
                ) : (
                  historyEntries.map((entry) => (
                    <Button
                      variant="ghost"
                      size="compact"
                      key={entry.url}
                      type="button"
                      className="justify-start flex w-full items-center gap-3 px-4 py-2 text-left text-xs hover:bg-accent transition border-b border-border/30"
                      onClick={() => {
                        if (activeTabId) navigateTab(activeTabId, entry.url);
                        setShowHistoryPanel(false);
                      }}
                    >
                      <div className="flex min-w-0 flex-1 flex-col">
                        <span className="truncate font-medium text-foreground">
                          {entry.title || entry.url}
                        </span>
                        <span className="truncate text-[10px] text-muted-foreground">
                          {entry.url} · {entry.visitCount} visits
                        </span>
                      </div>
                      <Button
                        variant="ghost"
                        size="compact"
                        type="button"
                        className="shrink-0 rounded p-0.5 text-muted-foreground/40 hover:text-destructive transition"
                        onClick={(e) => { e.stopPropagation(); setHistoryEntries(removeHistoryEntry(entry.url)); }}
                        title="Remove from history"
                      >
                        ×
                      </Button>
                    </Button>
                  ))
                )}
              </div>
            </div>
          </div>
        ) : null}

        {/* Profile manager panel */}
        {showProfileManager ? (
          <div className="absolute inset-0 z-30 flex bg-background">
            <div className="flex h-full w-full flex-col">
              <div className="flex items-center justify-between border-b border-border px-4 py-2">
                <h3 className="text-sm font-semibold text-foreground">
                  Session Profiles
                </h3>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="rounded px-2 py-1 text-[11px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
                  onClick={() => setShowProfileManager(false)}
                >
                  Close
                </Button>
              </div>
              <div className="flex-1 overflow-y-auto p-3">
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="justify-start mb-3 w-full rounded-md border border-border bg-card px-3 py-1.5 text-left text-xs text-foreground hover:bg-accent transition"
                  onClick={handleCreateProfile}
                >
                  + Create New Profile
                </Button>
                {sessionProfiles.length === 0 ? (
                  <div className="text-center text-xs text-muted-foreground mt-4">
                    No profiles yet. Create one to start managing separate
                    browsing sessions.
                  </div>
                ) : (
                  <div className="flex flex-col gap-2">
                    {sessionProfiles.map((profile) => (
                      <div
                        key={profile.id}
                        className={`rounded-md border px-3 py-2 text-left transition ${activeProfileId === profile.id ? 'border-primary bg-primary/10' : 'border-border bg-card'}`}
                      >
                        <div className="flex items-center justify-between">
                          <div>
                            <div className="text-xs font-medium text-foreground">
                              {profile.label}
                            </div>
                            <div className="text-[10px] text-muted-foreground">
                              {profile.scope}
                            </div>
                          </div>
                          <div className="flex items-center gap-1">
                            <Button
                              variant="ghost"
                              size="compact"
                              type="button"
                              className="rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
                              onClick={() => {
                                handleSelectProfile(profile.id);
                                setShowProfileManager(false);
                              }}
                            >
                              Use
                            </Button>
                            <Button
                              variant="ghost"
                              size="compact"
                              type="button"
                              className="rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
                              onClick={() => handleImportCookies(profile.id)}
                            >
                              Import Cookies
                            </Button>
                            <Button
                              variant="ghost"
                              size="compact"
                              type="button"
                              className="rounded px-1.5 py-0.5 text-[10px] text-muted-foreground hover:text-destructive transition"
                              onClick={() => handleDeleteProfile(profile.id)}
                            >
                              Delete
                            </Button>
                          </div>
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </div>
            </div>
          </div>
        ) : null}

        {/* Viewport preset overlay */}
        <BrowserMobileDriverOverlay presetId={viewportPresetId} />
        {/* Viewport preset overlay */}
        {showFailureOverlay ? (
          <div className="absolute inset-0 z-10 flex items-center justify-center bg-background px-6">
            <div className="flex max-w-sm flex-col items-center px-8 py-8 text-center opacity-80">
              <div className="mb-4 rounded-full border border-border/70 bg-background/30 p-3">
                <TriangleAlert size={20} className="text-warning" />
              </div>
              <h2 className="text-base font-semibold text-foreground/85">
                {loadErrorMeta.host
                  ? `Can't reach ${loadErrorMeta.host}`
                  : "Can't load this page"}
              </h2>
              <p className="mt-2 text-sm text-muted-foreground">
                {formatLoadFailureDescription()}
              </p>
              {loadErrorHint ? (
                <p className="mt-2 text-xs text-muted-foreground/80">
                  {loadErrorHint}
                </p>
              ) : null}
              <div className="mt-5 flex items-center gap-2">
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="inline-flex items-center gap-2 rounded-md border border-border bg-card px-3 py-1.5 text-xs font-medium text-foreground hover:bg-accent transition"
                  onClick={() => {
                    const wv = activeTabId ? webviewRefs.current.get(activeTabId) : null;
                    if (!wv) return;
                    updateTab(activeTabId!, { loading: true, loadError: null });
                    wv.reload();
                  }}
                >
                  <RefreshCw size={14} />
                  Refresh
                </Button>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="inline-flex items-center gap-2 rounded-md border border-transparent px-3 py-1.5 text-xs font-medium text-foreground hover:bg-accent transition"
                  onClick={() => {
                    handleCopyUrl(activeTab?.url ?? "");
                    setPermissionNotice("Copied the current page URL.");
                    setTimeout(() => setPermissionNotice(null), 2000);
                  }}
                >
                  <Copy size={14} />
                  Copy Address
                </Button>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="inline-flex items-center gap-2 rounded-md border border-transparent px-3 py-1.5 text-xs font-medium text-foreground hover:bg-accent transition"
                  onClick={() => {
                    if (activeTab?.url) handleOpenExternally(activeTab.url);
                  }}
                >
                  <ExternalLink size={14} />
                  Open Externally
                </Button>
              </div>
            </div>
          </div>
        ) : null}

        {/* Empty state for blank tabs */}
        {activeTab && isBlankTab && !showFailureOverlay ? (
          <div className="pointer-events-none absolute inset-0 z-10 flex items-center justify-center">
            <div className="flex flex-col items-center px-8 py-8 text-center opacity-60">
              <div className="mb-4 rounded-full border border-border/70 bg-background/30 p-3">
                <Globe size={20} className="text-muted-foreground" />
              </div>
              <div className="text-center">
                <p className="text-base font-semibold text-foreground/85">
                  New Tab
                </p>
                <p className="mt-2 text-sm text-muted-foreground">
                  Enter a URL or search term above.
                </p>
              </div>
            </div>
          </div>
        ) : null}

        {/* Find in page */}
        {activeTab && (
          <BrowserFind
            isOpen={findOpen}
            onClose={() => setFindOpen(false)}
            webviewRef={{
              current: webviewRefs.current.get(activeTab.id) ?? null,
            }}
          />
        )}

        {/* Grab Action Popover — shown after element selection */}
        {grabResult && grabActionPosition && !showAnnotationDialog ? (
          <PointMenu
            position={grabActionPosition}
            onClose={() => handleDismissGrab()}
          >
            {/* Element info header */}
            <div className="border-b border-border px-3 py-2">
              <div className="text-xs font-medium text-foreground">
                &lt;{grabResult.target?.tagName || 'element'}&gt;
              </div>
              <div className="mt-0.5 text-[11px] text-muted-foreground truncate max-w-[240px]">
                {grabResult.target?.textSnippet || grabResult.target?.selector || ''}
              </div>
            </div>
            {/* Actions */}
            <DropdownMenuItem
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
              onSelect={handleGrabCopyText}
            >
              Copy Text
            </DropdownMenuItem>
            <DropdownMenuItem
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
              onSelect={handleGrabCopyHTML}
            >
              Copy HTML
            </DropdownMenuItem>
            <DropdownMenuItem
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
              onSelect={handleGrabScreenshot}
            >
              Crop Screenshot
            </DropdownMenuItem>
            <div className="my-1 border-t border-border" />
            <DropdownMenuItem
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-primary hover:bg-primary/10 transition"
              onSelect={() => { setGrabActionPosition(null); setShowAnnotationDialog(true); }}
            >
              Annotate...
            </DropdownMenuItem>
            <div className="my-1 border-t border-border" />
            <DropdownMenuItem
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-muted-foreground hover:bg-accent transition"
              onSelect={handleDismissGrab}
            >
              Dismiss
            </DropdownMenuItem>
          </PointMenu>
        ) : null}

        {/* Annotation Dialog */}
        {showAnnotationDialog && grabResult ? (
          <>
            <div className="fixed inset-0 z-40" onClick={handleDismissGrab} />
            <Dialog
              open
              onOpenChange={(open) => {
                if (!open) handleDismissGrab();
              }}
            >
              <DialogContent
                showCloseButton={false}
                aria-describedby={undefined}
                className="block gap-0 p-0 w-[22rem] rounded-lg border border-border bg-card shadow-xl"
              >
                <DialogTitle className="sr-only">New annotation</DialogTitle>
                <div className="border-b border-border px-4 py-3">
                  <div className="text-sm font-medium text-foreground">
                    New Annotation
                  </div>
                  <div className="mt-1 text-[11px] text-muted-foreground">
                    &lt;{grabResult.target?.tagName || 'element'}&gt; —{" "}
                    {grabResult.target?.textSnippet?.slice(0, 60) || ''}
                  </div>
                </div>
                <div className="px-4 py-3 space-y-3">
                  <div>
                    <label className="block text-[11px] font-medium text-muted-foreground mb-1">
                      Intent
                    </label>
                    <div className="flex gap-1">
                      {['fix','change','question','approve'].map(
                        (intent) => (
                          <Button
                            variant="default"
                            size="compact"
                            key={intent}
                            className={`rounded px-2.5 py-1 text-[11px] transition ${annotationIntent === intent ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}`}
                            onClick={() => setAnnotationIntent(intent)}
                          >
                            {intent}
                          </Button>
                        ),
                      )}
                    </div>
                  </div>
                  <div>
                    <label className="block text-[11px] font-medium text-muted-foreground mb-1">
                      Priority
                    </label>
                    <div className="flex gap-1">
                      {['blocking','important','suggestion'].map((p) => (
                        <Button
                          variant="default"
                          size="compact"
                          key={p}
                          className={`rounded px-2.5 py-1 text-[11px] transition ${annotationPriority === p ? 'bg-primary text-primary-foreground' : 'text-muted-foreground hover:bg-accent'}`}
                          onClick={() => setAnnotationPriority(p)}
                        >
                          {p}
                        </Button>
                      ))}
                    </div>
                  </div>
                  <div>
                    <label className="block text-[11px] font-medium text-muted-foreground mb-1">
                      Comment
                    </label>
                    <Textarea
                      className="w-full rounded border border-border bg-accent px-2 py-1.5 text-xs text-foreground placeholder:text-muted-foreground/50 resize-none"
                      rows={3}
                      placeholder="Describe what needs attention..."
                      value={annotationComment}
                      onChange={(e) => setAnnotationComment(e.target.value)}
                      autoFocus
                    />
                  </div>
                </div>
                <div className="border-t border-border px-4 py-2 flex justify-end gap-2">
                  <Button
                    variant="ghost"
                    size="compact"
                    className="rounded px-3 py-1 text-[11px] text-muted-foreground hover:bg-accent transition"
                    onClick={handleDismissGrab}
                  >
                    Cancel
                  </Button>
                  <Button
                    variant="default"
                    size="compact"
                    className="rounded px-3 py-1 text-[11px] bg-primary text-primary-foreground hover:bg-primary/90 transition disabled:opacity-40"
                    onClick={handleCreateAnnotation}
                    disabled={!annotationComment.trim()}
                  >
                    Annotate
                  </Button>
                </div>
              </DialogContent>
            </Dialog>
          </>
        ) : null}

        {/* Annotation Sidebar — shown when page has annotations */}
        {pageAnnotations.length > 0 ? (
          <div className="absolute right-2 top-2 z-30 w-[17rem] max-h-[60vh] overflow-y-auto rounded-lg border border-border bg-card shadow-lg">
            <div className="sticky top-0 bg-card border-b border-border px-3 py-2 flex items-center justify-between">
              <div className="text-xs font-medium text-foreground">
                Annotations ({pageAnnotations.length})
              </div>
              <Button
                variant="ghost"
                size="compact"
                className="text-[11px] text-muted-foreground hover:text-foreground transition"
                onClick={() => {
                  void window.crab.browser.setAnnotationViewportBridge({
                    browserPageId: activeTabId!,
                    enabled: false,
                    emitViewport: false,
                    markers: [],
                    token: '',
                  });
                  setPageAnnotations([]);
                }}
              >
                Clear all
              </Button>
            </div>
            {pageAnnotations.map((a: any) => (
              <div key={a.id} className="border-b border-border/50 px-3 py-2 last:border-b-0">
                <div className="flex items-start gap-2">
                  <span className={`mt-0.5 shrink-0 rounded-full w-5 h-5 flex items-center justify-center text-[10px] font-bold ${
                    a.intent === 'fix' ? 'bg-destructive' :
                    a.intent === 'change' ? 'bg-warning' :
                    a.intent === 'question' ? 'bg-info text-info-foreground' : 'bg-success text-success-foreground'
                  }`}>
                    {pageAnnotations.indexOf(a) + 1}
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-1">
                      <span className="text-[10px] font-medium text-muted-foreground">
                        {a.intent}
                      </span>
                      <span className="text-[10px] text-muted-foreground/60">
                        {a.priority}
                      </span>
                    </div>
                    <div className="mt-0.5 text-[11px] text-foreground leading-tight">
                      {a.comment}
                    </div>
                    <div className="mt-0.5 text-[10px] text-muted-foreground/60 truncate">
                      &lt;{a.payload.target?.tagName || 'element'}&gt;{" "}
                      {a.payload.target?.textSnippet?.slice(0, 50) || ''}
                    </div>
                  </div>
                  <Button
                    variant="ghost"
                    size="compact"
                    className="shrink-0 text-[10px] text-muted-foreground/50 hover:text-destructive transition"
                    onClick={() => handleDeleteAnnotation(a.id)}
                  >
                    ×
                  </Button>
                </div>
              </div>
            ))}
          </div>
        ) : null}

        {/* Context menu */}

        {contextMenu ? (
          <BrowserContextMenu
            menu={contextMenu}
            onClose={() => setContextMenu(null)}
            onGoBack={handleGoBack}
            onGoForward={handleGoForward}
            onReload={handleReload}
            onOpenDevTools={handleOpenDevTools}
            onOpenExternally={handleOpenExternally}
            onCopyUrl={handleCopyUrl}
            onScreenshotToClipboard={(tabId) => { void window.crab.browser.screenshotToClipboard({ browserPageId: tabId }); }}
            onGrabModeToggle={() => handleToggleGrab()}
          />
        ) : null}

        {/* Permission notice toast */}
        {permissionNotice ? (
          <div className="absolute bottom-3 left-3 z-40 rounded-md border border-warning/50 bg-card px-3 py-2 shadow-lg text-xs text-foreground">
            {permissionNotice}
          </div>
        ) : null}

        {/* Download progress */}
        {downloads.size > 0 ? (
          <div className="absolute bottom-3 right-3 z-40 flex flex-col gap-1 max-w-[300px]">
            {[...downloads.values()].slice(0, 2).map((d) => {
              const progress = d.totalBytes > 0 ? Math.round((d.receivedBytes / d.totalBytes) * 100) : 0;
              return (
                <div key={d.downloadId} className="rounded-md border border-border bg-card px-2.5 py-1.5 shadow-lg text-[11px] text-foreground leading-tight">
                  <div className="flex items-center justify-between gap-2">
                    <span className="truncate">{d.filename}</span>
                    {d.status === "downloading" ? (
                      <span className="shrink-0 text-muted-foreground">
                        {progress}%
                      </span>
                    ) : d.status === "completed" ? (
                      <span className="shrink-0 text-success">Done</span>
                    ) : d.status === "canceled" ? (
                      <span className="shrink-0 text-muted-foreground">
                        Canceled
                      </span>
                    ) : (
                      <span className="shrink-0 text-destructive">Failed</span>
                    )}
                  </div>
                  {d.status === "downloading" && d.totalBytes > 0 ? (
                    <div className="mt-1 h-1 w-full rounded-full bg-border overflow-hidden">
                      <div className="h-full rounded-full bg-primary transition-all" style={{ width: progress + "%" }} />
                    </div>
                  ) : null}
                  {d.status === "downloading" && d.speed > 0 ? (
                    <div className="mt-0.5 text-[10px] text-muted-foreground/70">
                      {formatBytes(d.speed)}/s
                      {d.totalBytes > d.receivedBytes ? (
                        <span className="ml-1">
                          ETA{" "}
                          {formatDuration((d.totalBytes - d.receivedBytes) / d.speed)}
                        </span>
                      ) : null}
                    </div>
                  ) : null}
                  {d.status === "completed" && d.savePath ? (
                    <div className="mt-1 truncate text-[10px] text-muted-foreground">
                      {d.savePath}
                    </div>
                  ) : null}
                </div>
              );
            })}
          </div>
        ) : null}
      </div>
    </div>
  );
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

function formatDuration(seconds: number): string {
  if (seconds < 60) return Math.round(seconds) + "s";
  if (seconds < 3600) return Math.round(seconds / 60) + "m " + Math.round(seconds % 60) + "s";
  const h = Math.floor(seconds / 3600);
  const m = Math.round((seconds % 3600) / 60);
  return h + "h " + m + "m";
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  const k = 1024;
  const sizes = ["B", "KB", "MB", "GB"];
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  return `${parseFloat((bytes / Math.pow(k, i)).toFixed(1))} ${sizes[i]}`;
}

function humanizePermission(permission: string): string {
  switch (permission) {
    case "notifications":
      return "notifications";
    case "geolocation":
      return "location access";
    case "camera":
      return "camera access";
    case "microphone":
      return "microphone access";
    case "midi":
      return "MIDI access";
    case "idle-detection":
      return "idle detection";
    case "storage-access":
      return "storage access";
    case "pointerLock":
      return "pointer lock";
    case "media":
      return "camera or microphone access";
    default:
      return permission;
  }
}

export default BrowserView;
