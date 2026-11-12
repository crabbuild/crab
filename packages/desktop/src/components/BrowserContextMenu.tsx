import {
  DropdownMenuItem,
  DropdownMenuSeparator,
  PointMenu,
} from "@/components/ui/dropdown-menu";
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

  return (
    <PointMenu position={{ x, y }} onClose={onClose}>
        {/* Link-specific items */}
        {linkUrl ? (
          <>
            <DropdownMenuItem
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
            onSelect={() => {
                onOpenExternally(linkUrl);
            }}
            >
              <ExternalLink size={13} className="shrink-0 text-muted-foreground" />
            Open Link in System Browser
          </DropdownMenuItem>
          <DropdownMenuItem
            className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
            onSelect={() => {
                onCopyUrl(linkUrl);
            }}
            >
              <Copy size={13} className="shrink-0 text-muted-foreground" />
            Copy Link Address
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          </>
        ) : null}

        {/* Navigation */}
        <DropdownMenuItem
        className={`flex w-full items-center gap-2 px-3 py-1.5 text-xs transition ${
            canGoBack
              ? "text-foreground hover:bg-accent"
              : "text-muted-foreground/40 cursor-not-allowed"
          }`}
        onSelect={() => {
            if (canGoBack) onGoBack(tabId);
          }}
          disabled={!canGoBack}
        >
          <ArrowLeft size={13} className="shrink-0" />
        Back
      </DropdownMenuItem>
      <DropdownMenuItem
        className={`flex w-full items-center gap-2 px-3 py-1.5 text-xs transition ${
            canGoForward
              ? "text-foreground hover:bg-accent"
              : "text-muted-foreground/40 cursor-not-allowed"
          }`}
        onSelect={() => {
            if (canGoForward) onGoForward(tabId);
          }}
          disabled={!canGoForward}
        >
          <ArrowRight size={13} className="shrink-0" />
        Forward
      </DropdownMenuItem>
      <DropdownMenuItem
        className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
        onSelect={() => {
            onReload(tabId);
        }}
        >
          <RefreshCw size={13} className="shrink-0 text-muted-foreground" />
        Reload
      </DropdownMenuItem>

      <DropdownMenuSeparator />

      {/* Page-level items */}
      <DropdownMenuItem
        className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
        onSelect={() => {
            onOpenExternally(pageUrl);
        }}
        >
          <ExternalLink size={13} className="shrink-0 text-muted-foreground" />
        Open Page in System Browser
      </DropdownMenuItem>
      <DropdownMenuItem
        className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
        onSelect={() => {
            onCopyUrl(pageUrl);
        }}
        >
          <Copy size={13} className="shrink-0 text-muted-foreground" />
        Copy Page URL
      </DropdownMenuItem>

        {/* Screenshot to Clipboard */}
        {onScreenshotToClipboard ? (
          <DropdownMenuItem
          className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
          onSelect={() => {
              onScreenshotToClipboard(tabId);
          }}
          >
            <Camera size={13} className="shrink-0 text-muted-foreground" />
          Screenshot to Clipboard
        </DropdownMenuItem>
        ) : null}

        {/* Grab Element */}
        {onGrabModeToggle ? (
          <DropdownMenuItem
          className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
          onSelect={() => {
              onGrabModeToggle(tabId);
          }}
          >
            <Crosshair size={13} className="shrink-0 text-muted-foreground" />
          Grab Element
        </DropdownMenuItem>
        ) : null}

        <DropdownMenuSeparator />

      <DropdownMenuItem
        className="flex w-full items-center gap-2 px-3 py-1.5 text-xs text-foreground hover:bg-accent transition"
        onSelect={() => {
            onOpenDevTools(tabId);
        }}
        >
          <Bug size={13} className="shrink-0 text-muted-foreground" />
        Developer Tools
      </DropdownMenuItem>
    </PointMenu>
  );
}
