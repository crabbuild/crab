import { Button } from "@/components/ui/button";
import {
  CommandGroup,
  CommandItem,
  CommandList,
  CommandPopover,
} from "@/components/ui/command";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import clsx from "clsx";
import {
  ArrowLeft,
  ArrowRight,
  Camera,
  ChevronDown,
  Crosshair,
  Globe,
  Loader2,
  Lock,
  Monitor,
  Plus,
  RefreshCw,
  Search,
  Settings,
  Smartphone,
  Star,
  Unlock,
  User,
  X,
  ZoomIn,
  ZoomOut,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  BrowserAddressBarSuggestion,
  buildBrowserAddressBarSuggestions,
} from "../lib/browser-address-bar-suggestions";
import { loadHistory } from "../lib/browser-history";
import {
  CRAB_BROWSER_BLANK_URL,
  DEFAULT_SEARCH_ENGINE,
  normalizeBrowserNavigationUrl,
} from "../lib/browser-url";

// ─── Types ──────────────────────────────────────────────────────────────────

export interface BrowserTab {
  id: string;
  url: string;
  title: string;
  loading: boolean;
  canGoBack: boolean;
  canGoForward: boolean;
  faviconUrl: string | null;
}

interface BrowserToolbarProps {
  tabs: BrowserTab[];
  activeTabId: string | null;
  onSelectTab: (tabId: string) => void;
  onCloseTab: (tabId: string) => void;
  onCreateTab: () => void;
  onNavigate: (tabId: string, url: string) => void;
  onGoBack: (tabId: string) => void;
  onGoForward: (tabId: string) => void;
  onReload: (tabId: string) => void;
  onStop: (tabId: string) => void;
  addressBarValue: string;
  onAddressBarChange: (value: string) => void;
  onAddressBarSubmit: () => void;
  findOpen: boolean;
  onToggleFind: () => void;
  onOpenDevTools: (tabId: string) => void;
  /** Screenshot to clipboard for the active tab */
  onScreenshot?: () => void;
  /** Toggle grab mode (element picker) for the active tab */
  onToggleGrab?: () => void;
  /** Whether grab mode is currently active */
  grabModeActive?: boolean;
  /** Increment to trigger address bar focus from parent */
  focusAddressBarKey?: number;
  /** Whether the current URL is HTTPS */
  isSecure?: boolean;
  /** Whether the current URL is bookmarked */
  isBookmarked?: boolean;
  /** Called to toggle bookmark for current URL */
  onToggleBookmark?: () => void;
  /** Zoom level (1.0 = 100%) */
  zoomLevel?: number;
  /** Called when zoom changes */
  onZoomIn?: () => void;
  onZoomOut?: () => void;
  onZoomReset?: () => void;
  /** Viewport preset ID for mobile emulation */
  viewportPresetId?: string | null;
  /** Viewport presets list */
  viewportPresets?: Array<{ id: string; label: string }>;
  /** Called when viewport preset changes */
  onSetViewportPreset?: (id: string | null) => void;
  /** Session profiles */
  sessionProfiles?: Array<{ id: string; label: string; scope: string }>;
  /** Active profile ID */
  activeProfileId?: string | null;
  /** Called when profile changes */
  onSelectProfile?: (profileId: string | null) => void;
  /** Called to create a profile */
  onCreateProfile?: () => void;
  /** Called to manage profiles */
  onManageProfiles?: () => void;
}

// ─── URL History Persistence ────────────────────────────────────────────────

// ─── Component ──────────────────────────────────────────────────────────────

export function BrowserToolbar({
  tabs,
  activeTabId,
  onSelectTab,
  onCloseTab,
  onCreateTab,
  onNavigate,
  onGoBack,
  onGoForward,
  onReload,
  onStop,
  addressBarValue,
  onAddressBarChange,
  onAddressBarSubmit,
  onOpenDevTools,
  onScreenshot,
  onToggleGrab,
  grabModeActive,
  focusAddressBarKey = 0,
  isSecure = false,
  isBookmarked,
  onToggleBookmark,
  zoomLevel,
  onZoomIn,
  onZoomOut,
  onZoomReset,
  viewportPresetId,
  viewportPresets,
  onSetViewportPreset,
  sessionProfiles,
  activeProfileId,
  onSelectProfile,
  onCreateProfile,
  onManageProfiles,
}: BrowserToolbarProps) {
  const activeTab = tabs.find((t) => t.id === activeTabId) ?? null;
  const addressInputRef = useRef<HTMLInputElement>(null);
  const [suggestions, setSuggestions] = useState<BrowserAddressBarSuggestion[]>([]);
  const [suggestionsOpen, setSuggestionsOpen] = useState(false);
  const [selectedSuggestionIndex, setSelectedSuggestionIndex] = useState(-1);

  // Dropdown menus
  const viewportMenuRef = useRef<HTMLDivElement>(null);
  const profileMenuRef = useRef<HTMLDivElement>(null);
  const [viewportMenuOpen, setViewportMenuOpen] = useState(false);
  const [profileMenuOpen, setProfileMenuOpen] = useState(false);

  // Focus address bar when parent requests it (new tab, switch to browser view)
  useEffect(() => {
    if (focusAddressBarKey > 0) {
      addressInputRef.current?.focus();
      addressInputRef.current?.select();
    }
  }, [focusAddressBarKey]);

  // Build suggestions from history + search engine
  useEffect(() => {
    if (!addressBarValue || document.activeElement !== addressInputRef.current) {
      setSuggestionsOpen(false);
      return;
    }
    const history = loadHistory().map((entry) => ({
      url: entry.url,
      title: entry.title,
      lastVisitedAt: entry.lastVisitedAt,
      visitCount: entry.visitCount,
    }));
    const s = buildBrowserAddressBarSuggestions({
      browserUrlHistory: history,
      searchEngine: DEFAULT_SEARCH_ENGINE,
      value: addressBarValue,
    });
    setSuggestions(s);
    setSuggestionsOpen(s.length > 0);
    setSelectedSuggestionIndex(-1);
  }, [addressBarValue]);

  const handleSubmit = useCallback(() => {
    const url = normalizeBrowserNavigationUrl(addressBarValue, DEFAULT_SEARCH_ENGINE);
    if (!url || url === CRAB_BROWSER_BLANK_URL || !activeTabId) return;
    // History is recorded by BrowserView via recordVisit
    setSuggestionsOpen(false);
    onAddressBarSubmit();
  }, [addressBarValue, activeTabId, onAddressBarSubmit]);

  const handleSuggestionSelect = useCallback(
    (url: string) => {
      setSuggestionsOpen(false);
      onAddressBarChange(url);
      if (activeTabId) {
        onNavigate(activeTabId, url);
      }
    },
    [activeTabId, onAddressBarChange, onNavigate],
  );

  const handleAddressKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        if (
          suggestionsOpen &&
          selectedSuggestionIndex >= 0 &&
          selectedSuggestionIndex < suggestions.length
        ) {
          handleSuggestionSelect(suggestions[selectedSuggestionIndex].url);
        } else {
          handleSubmit();
        }
      } else if (e.key === "Escape") {
        setSuggestionsOpen(false);
        addressInputRef.current?.blur();
      } else if (e.key === "ArrowDown") {
        e.preventDefault();
        setSelectedSuggestionIndex((prev) =>
          prev < suggestions.length - 1 ? prev + 1 : 0,
        );
      } else if (e.key === "ArrowUp") {
        e.preventDefault();
        setSelectedSuggestionIndex((prev) =>
          prev > 0 ? prev - 1 : suggestions.length - 1,
        );
      }
    },
    [suggestionsOpen, selectedSuggestionIndex, suggestions, handleSuggestionSelect, handleSubmit],
  );

  // Focus address bar on Cmd+L
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "l") {
        e.preventDefault();
        addressInputRef.current?.focus();
        addressInputRef.current?.select();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);

  // Sync address input value when switching tabs
  useEffect(() => {
    if (addressInputRef.current) {
      addressInputRef.current.value = addressBarValue;
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeTabId]);

  return (
    <div className="flex flex-col border-b border-border bg-card">
      {/* Tab bar */}
      <div className="flex items-center gap-0 overflow-x-auto px-2 pt-1">
        {tabs.map((tab) => {
          const isActive = tab.id === activeTabId;
          return (
            <div
              key={tab.id}
              className={clsx(
                "group flex shrink-0 items-center gap-1.5 rounded-t-md px-3 py-1.5 text-xs cursor-pointer select-none max-w-[180px] transition",
                isActive
                  ? "bg-background text-foreground border border-border border-b-0"
                  : "text-muted-foreground hover:text-foreground hover:bg-accent",
              )}
              onClick={() => onSelectTab(tab.id)}
              role="tab"
              aria-selected={isActive}
              tabIndex={0}
            >
              {tab.loading ? (
                <Loader2 size={10} className="shrink-0 animate-spin" />
              ) : tab.faviconUrl ? (
                <img src={tab.faviconUrl} alt="" className="h-3.5 w-3.5 shrink-0 rounded" />
              ) : (
                <Globe size={10} className="shrink-0 text-muted-foreground" />
              )}
              <span className="truncate min-w-0 flex-1">
                {tab.title || "New Tab"}
              </span>
              <Button
                variant="ghost"
                size="compact"
                type="button"
                className="shrink-0 rounded p-0.5 opacity-0 group-hover:opacity-100 hover:bg-accent text-muted-foreground hover:text-foreground transition"
                onClick={(e) => {
                  e.stopPropagation();
                  onCloseTab(tab.id);
                }}
                aria-label="Close tab"
              >
                <X size={10} />
              </Button>
            </div>
          );
        })}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="shrink-0 rounded p-1 text-muted-foreground hover:text-foreground hover:bg-accent transition"
          onClick={onCreateTab}
          aria-label="New tab"
          title="New Tab"
        >
          <Plus size={14} />
        </Button>
      </div>

      {/* Navigation bar */}
      <div className="flex items-center gap-1.5 px-2 py-1.5">
        {/* Back */}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={clsx(
            "rounded p-1 transition",
            activeTab?.canGoBack
              ? "text-foreground hover:bg-accent"
              : "text-muted-foreground/40 cursor-not-allowed",
          )}
          onClick={() => activeTabId && onGoBack(activeTabId)}
          disabled={!activeTab?.canGoBack}
          aria-label="Go back"
          title="Back"
        >
          <ArrowLeft size={16} />
        </Button>

        {/* Forward */}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className={clsx(
            "rounded p-1 transition",
            activeTab?.canGoForward
              ? "text-foreground hover:bg-accent"
              : "text-muted-foreground/40 cursor-not-allowed",
          )}
          onClick={() => activeTabId && onGoForward(activeTabId)}
          disabled={!activeTab?.canGoForward}
          aria-label="Go forward"
          title="Forward"
        >
          <ArrowRight size={16} />
        </Button>

        {/* Reload/Stop */}
        {activeTab?.loading ? (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-foreground hover:bg-accent transition"
            onClick={() => activeTabId && onStop(activeTabId)}
            aria-label="Stop loading"
            title="Stop"
          >
            <X size={14} />
          </Button>
        ) : (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded p-1 text-foreground hover:bg-accent transition"
            onClick={() => activeTabId && onReload(activeTabId)}
            aria-label="Reload"
            title="Reload"
          >
            <RefreshCw size={14} />
          </Button>
        )}

        {/* URL Bar */}
        <div className="relative flex min-w-0 flex-1">
          <div className="flex min-w-0 flex-1 items-center gap-1.5 rounded-md border border-border bg-background px-2.5 py-1 text-sm">
            {/* Security indicator */}
            {isSecure ? (
              <Lock size={12} className="shrink-0 text-success" />
            ) : activeTab?.url && !activeTab.url.startsWith("about:") && !activeTab.url.startsWith("data:") ? (
              <Unlock size={12} className="shrink-0 text-muted-foreground" />
            ) : (
              <Globe size={12} className="shrink-0 text-muted-foreground" />
            )}
            <Input
              ref={addressInputRef}
              type="text"
              defaultValue={addressBarValue}
              onChange={(e) => onAddressBarChange(e.target.value)}
              onKeyDown={handleAddressKeyDown}
              onFocus={() => {
                if (suggestions.length > 0) {
                  setSuggestionsOpen(true);
                }
              }}
              placeholder="Search or enter URL..."
              className="min-w-0 flex-1 bg-transparent text-foreground placeholder-muted-foreground/60 outline-none text-[13px]"
              spellCheck={false}
              autoCapitalize="none"
              autoCorrect="off"
              role="combobox"
              aria-expanded={suggestionsOpen}
              aria-autocomplete="list"
            />
          </div>

          {/* Suggestions dropdown */}
          {suggestionsOpen && suggestions.length > 0 && (
            <CommandPopover
              anchorRef={addressInputRef}
              onClose={() => setSuggestionsOpen(false)}
              side="bottom"
              value={String(selectedSuggestionIndex)}
              onValueChange={(value) =>
                setSelectedSuggestionIndex(Number(value))
              }
            >
              <CommandList className="max-h-48">
                <CommandGroup>
                  {suggestions.map((s, i) => (
                    <CommandItem
                      value={String(i)}
                      key={`${s.url}-${i}`}
                      className={clsx(
                    "flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition",
                    i === selectedSuggestionIndex
                      ? "bg-primary/10 text-foreground"
                      : "text-muted-foreground hover:bg-accent hover:text-foreground",
                  )}
                      onSelect={() => {
                        handleSuggestionSelect(s.url);
                      }}
                    >
                      {s.isSearch ? (
                    <Search size={12} className="shrink-0" />
                  ) : (
                    <Globe size={12} className="shrink-0" />
                  )}
                      <div className="min-w-0 flex-1">
                        <div className="truncate">{s.title}</div>
                        {s.subtitle && (
                          <div className="truncate text-[10px] text-muted-foreground/60">
                            {s.subtitle}
                          </div>
                        )}
                      </div>
                    </CommandItem>
                  ))}
                </CommandGroup>
              </CommandList>
            </CommandPopover>
          )}
        </div>

        {/* Viewport preset selector */}
        {viewportPresets && viewportPresetId !== undefined && onSetViewportPreset ? (
          <div className="relative" ref={viewportMenuRef}>
            <DropdownMenu
              open={viewportMenuOpen}
              onOpenChange={setViewportMenuOpen}
            >
              <DropdownMenuTrigger asChild>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="flex items-center gap-1 rounded px-1.5 py-1 text-[11px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
                  onClick={() => setViewportMenuOpen((v) => !v)}
                  title="Viewport preset"
                  aria-label="Viewport preset"
                >
                  <Smartphone size={12} />
                  <ChevronDown size={10} />
                </Button>
              </DropdownMenuTrigger>
              {viewportMenuOpen && (
                <DropdownMenuContent
                  side="bottom"
                  align="end"
                  className=" min-w-[180px] rounded-md border border-border bg-card py-1 "
                >
                  <DropdownMenuGroup>
                    <DropdownMenuItem
                      className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition ${!viewportPresetId ? 'bg-primary/10 text-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}`}
                      onSelect={() => { onSetViewportPreset(null); setViewportMenuOpen(false); }}
                    >
                      <Monitor size={12} />
                      Responsive
                    </DropdownMenuItem>
                    {viewportPresets.map((preset) => (
                      <DropdownMenuItem
                        key={preset.id}
                        className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition ${viewportPresetId === preset.id ? 'bg-primary/10 text-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}`}
                        onSelect={() => { onSetViewportPreset(preset.id); setViewportMenuOpen(false); }}
                      >
                        <Smartphone size={12} />
                        {preset.label}
                      </DropdownMenuItem>
                    ))}
                  </DropdownMenuGroup>
                </DropdownMenuContent>
              )}
            </DropdownMenu>
          </div>
        ) : null}

        {/* Zoom controls */}
        {onZoomIn && onZoomOut ? (
          <div className="flex items-center gap-0.5 rounded px-1">
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="rounded px-1 py-1 text-muted-foreground hover:text-foreground hover:bg-accent transition"
              onClick={onZoomOut}
              aria-label="Zoom Out"
              title="Zoom Out (Cmd+-)"
            >
              <ZoomOut size={12} />
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="rounded px-1 py-1 text-[11px] text-muted-foreground hover:text-foreground hover:bg-accent transition min-w-[32px] text-center"
              onClick={onZoomReset}
              aria-label="Reset Zoom"
              title="Reset Zoom (Cmd+0)"
            >
              {Math.round((zoomLevel ?? 1) * 100)}%
            </Button>
            <Button
              variant="ghost"
              size="compact"
              type="button"
              className="rounded px-1 py-1 text-muted-foreground hover:text-foreground hover:bg-accent transition"
              onClick={onZoomIn}
              aria-label="Zoom In"
              title="Zoom In (Cmd+=)"
            >
              <ZoomIn size={12} />
            </Button>
          </div>
        ) : null}

        {/* Session profile selector */}
        {onSelectProfile ? (
          <div className="relative" ref={profileMenuRef}>
            <DropdownMenu
              open={profileMenuOpen}
              onOpenChange={setProfileMenuOpen}
            >
              <DropdownMenuTrigger asChild>
                <Button
                  variant="ghost"
                  size="compact"
                  type="button"
                  className="flex items-center gap-1 rounded px-1.5 py-1 text-[11px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
                  onClick={() => setProfileMenuOpen((v) => !v)}
                  title="Session profile"
                  aria-label="Session profile"
                >
                  <User size={12} />
                  <ChevronDown size={10} />
                </Button>
              </DropdownMenuTrigger>
              {profileMenuOpen && (
                <DropdownMenuContent
                  side="bottom"
                  align="end"
                  className=" min-w-[200px] rounded-md border border-border bg-card py-1 "
                >
                  <DropdownMenuGroup>
                    {/* Default profile option */}
                    <DropdownMenuItem
                      className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition ${!activeProfileId ? 'bg-primary/10 text-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}`}
                      onSelect={() => { onSelectProfile(null); setProfileMenuOpen(false); }}
                    >
                      <Globe size={12} />
                      Default Session
                    </DropdownMenuItem>
                    {(sessionProfiles || []).map((profile) => (
                      <DropdownMenuItem
                        key={profile.id}
                        className={`flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs transition ${activeProfileId === profile.id ? 'bg-primary/10 text-foreground' : 'text-muted-foreground hover:bg-accent hover:text-foreground'}`}
                        onSelect={() => { onSelectProfile(profile.id); setProfileMenuOpen(false); }}
                      >
                        <User size={12} />
                        <span className="truncate">{profile.label}</span>
                        <span className="ml-auto text-[10px] text-muted-foreground/50">
                          {profile.scope}
                        </span>
                      </DropdownMenuItem>
                    ))}
                    <div className="my-1 border-t border-border" />
                    {onCreateProfile ? (
                      <DropdownMenuItem
                        className="justify-start flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs text-muted-foreground hover:bg-accent hover:text-foreground transition"
                        onSelect={() => { onCreateProfile(); setProfileMenuOpen(false); }}
                      >
                        <Plus size={12} />
                        Create Profile
                      </DropdownMenuItem>
                    ) : null}
                    {onManageProfiles ? (
                      <DropdownMenuItem
                        className="justify-start flex w-full items-center gap-2 px-3 py-1.5 text-left text-xs text-muted-foreground hover:bg-accent hover:text-foreground transition"
                        onSelect={() => { onManageProfiles(); setProfileMenuOpen(false); }}
                      >
                        <Settings size={12} />
                        Manage Profiles
                      </DropdownMenuItem>
                    ) : null}
                  </DropdownMenuGroup>
                </DropdownMenuContent>
              )}
            </DropdownMenu>
          </div>
        ) : null}

        {/* Bookmark */}
        {onToggleBookmark ? (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded px-1.5 py-1 text-[11px] transition"
            style={{ color: isBookmarked ? 'rgb(var(--warning))' : undefined }}
            onClick={onToggleBookmark}
            aria-label={isBookmarked ? 'Remove bookmark' : 'Add bookmark'}
            title={isBookmarked ? 'Remove bookmark' : 'Add bookmark'}
          >
            <Star size={14} fill={isBookmarked ? 'rgb(var(--warning))' : 'none'} />
          </Button>
        ) : null}

        {/* Screenshot to Clipboard */}
        {onScreenshot ? (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className="rounded px-1.5 py-1 text-[11px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
            onClick={onScreenshot}
            aria-label="Screenshot to Clipboard"
            title="Screenshot to Clipboard"
          >
            <Camera size={14} />
          </Button>
        ) : null}

        {/* Grab Mode (Element Picker) */}
        {onToggleGrab ? (
          <Button
            variant="ghost"
            size="compact"
            type="button"
            className={`rounded px-1.5 py-1 text-[11px] transition ${
              grabModeActive
                ? 'text-primary bg-primary/10'
                : 'text-muted-foreground hover:text-foreground hover:bg-accent'
            }`}
            onClick={onToggleGrab}
            aria-label={grabModeActive ? 'Exit Grab Mode' : 'Grab Mode'}
            title={grabModeActive ? 'Exit Grab Mode' : 'Grab Mode (Element Picker)'}
          >
            <Crosshair size={14} />
          </Button>
        ) : null}

        {/* DevTools */}
        <Button
          variant="ghost"
          size="compact"
          type="button"
          className="rounded px-1.5 py-1 text-[11px] text-muted-foreground hover:text-foreground hover:bg-accent transition"
          onClick={() => activeTabId && onOpenDevTools(activeTabId)}
          aria-label="Open DevTools"
          title="Developer Tools"
        >
          DevTools
        </Button>
      </div>
    </div>
  );
}
