import { createPortal } from "react-dom";
import {
  ArrowLeft,
  ArrowRight,
  Bug,
  Camera,
  Copy,
  Crosshair,
  ExternalLink,
  RefreshCw,
} from "lucide-react";

// ─── Types ──────────────────────────────────────────────────────────────────

export interface BrowserContextMenuState {
  browserTabId: string;
  x: number;
  y: number;
  pageUrl: string;
  linkUrl: string | null;
  canGoBack: boolean;
  canGoForward: boolean;
}

interface BrowserContextMenuProps {
  menu: BrowserContextMenuState;
  onClose: () => void;
  onGoBack: (tabId: string) => void;
  onGoForward: (tabId: string) => void;
  onReload: (tabId: string) => void;
  onOpenDevTools: (tabId: string) => void;
  onOpenExternally: (url: string) => void;
  onCopyUrl: (url: string) => void;
  onScreenshotToClipboard?: (tabId: string) => void;
  onGrabModeToggle?: (tabId: string) => void;
}

// ─── Component ──────────────────────────────────────────────────────────────

export function BrowserContextMenu({
  menu,
  onClose,
  onGoBack,
  onGoForward,
  onReload,
  onOpenDevTools,
  onOpenExternally,
  onCopyUrl,
  onScreenshotToClipboard,
  onGrabModeToggle,
}: BrowserContextMenuProps) {
  const tabId = menu.browserTabId;
  const { x, y, pageUrl, linkUrl, canGoBack, canGoForward } = menu;

  // Adjust position to stay within viewport
  const adjustedX = Math.min(x, window.innerWidth - 200);
  const adjustedY = Math.min(y, window.innerHeight - 280);

  return createPortal(
    <>
      {/* Backdrop */}
      <div className="fixed inset-0 z-40" onPointerDown={onClose} />
      {/* Menu */}
      <div
        role="menu"
        style={{ left: adjustedX, top: adjustedY }}
        className="fixed z-50 min-w-[14rem] overflow-hidden rounded-lg border border-border bg-card py-1 shadow-xl"
      >
        {/* Link-specific items */}
        {linkUrl ? (
          <>
            <button
              role="menuitem"
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
              onClick={() => {
                onOpenExternally(linkUrl);
                onClose();
              }}
            >
              <ExternalLink size={13} className="shrink-0 text-muted-foreground" />
              Open Link in System Browser
            </button>
            <button
              role="menuitem"
              className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
              onClick={() => {
                onCopyUrl(linkUrl);
                onClose();
              }}
            >
              <Copy size={13} className="shrink-0 text-muted-foreground" />
              Copy Link Address
            </button>
            <div className="my-1 h-px bg-border" />
          </>
        ) : null}

        {/* Navigation */}
        <button
          role="menuitem"
          className={`flex w-full items-center gap-2 px-3 py-1.5 text-xs transition ${
            canGoBack
              ? "text-foreground hover:bg-accent"
              : "text-muted-foreground/40 cursor-not-allowed"
          }`}
          onClick={() => {
            if (canGoBack) onGoBack(tabId);
            onClose();
          }}
          disabled={!canGoBack}
        >
          <ArrowLeft size={13} className="shrink-0" />
          Back
        </button>
        <button
          role="menuitem"
          className={`flex w-full items-center gap-2 px-3 py-1.5 text-xs transition ${
            canGoForward
              ? "text-foreground hover:bg-accent"
              : "text-muted-foreground/40 cursor-not-allowed"
          }`}
          onClick={() => {
            if (canGoForward) onGoForward(tabId);
            onClose();
          }}
          disabled={!canGoForward}
        >
          <ArrowRight size={13} className="shrink-0" />
          Forward
        </button>
        <button
          role="menuitem"
          className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
          onClick={() => {
            onReload(tabId);
            onClose();
          }}
        >
          <RefreshCw size={13} className="shrink-0 text-muted-foreground" />
          Reload
        </button>

        <div className="my-1 h-px bg-border" />

        {/* Page-level items */}
        <button
          role="menuitem"
          className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
          onClick={() => {
            onOpenExternally(pageUrl);
            onClose();
          }}
        >
          <ExternalLink size={13} className="shrink-0 text-muted-foreground" />
          Open Page in System Browser
        </button>
        <button
          role="menuitem"
          className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
          onClick={() => {
            onCopyUrl(pageUrl);
            onClose();
          }}
        >
          <Copy size={13} className="shrink-0 text-muted-foreground" />
          Copy Page URL
        </button>

        {/* Screenshot to Clipboard */}
        {onScreenshotToClipboard ? (
          <button
            role="menuitem"
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
            onClick={() => {
              onScreenshotToClipboard(tabId);
              onClose();
            }}
          >
            <Camera size={13} className="shrink-0 text-muted-foreground" />
            Screenshot to Clipboard
          </button>
        ) : null}

        {/* Grab Element */}
        {onGrabModeToggle ? (
          <button
            role="menuitem"
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
            onClick={() => {
              onGrabModeToggle(tabId);
              onClose();
            }}
          >
            <Crosshair size={13} className="shrink-0 text-muted-foreground" />
            Grab Element
          </button>
        ) : null}

        <div className="my-1 h-px bg-border" />

        <button
          role="menuitem"
          className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
          onClick={() => {
            onOpenDevTools(tabId);
            onClose();
          }}
        >
          <Bug size={13} className="shrink-0 text-muted-foreground" />
          Developer Tools
        </button>
      </div>
    </>,
    document.body
  );
}

export default BrowserContextMenu;
