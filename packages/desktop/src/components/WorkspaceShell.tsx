import { useCallback, useEffect, useRef, useState } from "react";
import { ImperativePanelHandle, Panel, PanelGroup, PanelResizeHandle } from "react-resizable-panels";
import clsx from "clsx";
import { ActivityBar, SidebarView, VIEWS } from "./ActivityBar";
import { useSetting } from "../settings";

interface Props {
  /** View content keyed by id. `undefined` means "not yet built" — the
   *  sidebar will render a placeholder for that view. */
  views: Partial<Record<SidebarView, React.ReactNode>>;
  main: React.ReactNode;
  /** Notifies the parent whenever the active view changes (e.g. via
   *  the activity bar or the Cmd+, accelerator). Parents use this to
   *  switch the main pane between editor / Settings / etc. */
  onViewChange?: (view: SidebarView) => void;
  /** When set, overrides the current view. Used by the parent to
   *  programmatically switch views (e.g. to "dashboard" after opening
   *  a deferred repo). */
  requestedView?: SidebarView | null;
  /** Views that should hide the middle sidebar entirely — letting
   *  the main content extend edge-to-edge. Used today by the Settings
   *  page, which has its own internal left-nav and doesn't benefit
   *  from a second, near-empty column to the left of it. */
  fullPaneViews?: readonly SidebarView[];
  /** Views to hide from the activity bar entirely (e.g. pull-requests
   *  when no forge provider is detected for the repo). */
  hiddenViews?: SidebarView[];
  /** Badge counts to display on activity bar icons. */
  badges?: Partial<Record<SidebarView, number>>;
}

const LS_WIDTH_KEY = "crab.sidebarPct";
const LS_VIEW_KEY = "crab.sidebarView";
const LS_OPEN_KEY = "crab.sidebarOpen";

/**
 * VS Code-style workspace shell:
 * - Activity bar (48 px) on the far left.
 * - Resizable sidebar (persisted width + collapsed state).
 * - Main content to the right.
 *
 * Keyboard: Cmd-B / Ctrl-B toggles sidebar visibility, matching VS
 * Code. The sidebar "collapses" by dropping to 0 % — we keep the
 * sibling panel alive so state (open files, scroll offsets) survives
 * across toggles.
 */
export function WorkspaceShell({ views, main, onViewChange, requestedView, fullPaneViews = [], hiddenViews = [], badges = {} }: Props) {
  const [activeView, setActiveView] = useState<SidebarView>(() => {
    const saved = localStorage.getItem(LS_VIEW_KEY);
    return (saved as SidebarView | null) ?? "dashboard";
  });
  const [open, setOpen] = useState<boolean>(() => {
    const saved = localStorage.getItem(LS_OPEN_KEY);
    return saved == null ? true : saved === "1";
  });

  // Allow the parent to programmatically switch views (e.g. to
  // "dashboard" after opening a deferred repo).
  useEffect(() => {
    if (requestedView) {
      setActiveView(requestedView);
    }
  }, [requestedView]);

  // Emit the initial view too so the parent can render the correct
  // main pane on first paint.
  useEffect(() => {
    onViewChange?.(activeView);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeView]);

  // Remember the last non-zero sidebar width so collapse/expand
  // restores it. Defaults to 22 % which is close to VS Code's own
  // sensible default.
  const lastWidthRef = useRef<number>(
    Number(localStorage.getItem(LS_WIDTH_KEY)) || 22,
  );

  const sidebarPanelRef = useRef<ImperativePanelHandle | null>(null);

  const showActivityBar = useSetting("showActivityBar");
  const isFullPane = fullPaneViews.includes(activeView);
  // Hide the middle column entirely for full-pane views so the main
  // content extends past the activity bar with no empty sidebar
  // between them. `open` stays untouched so the user's explicit
  // collapse/restore choice is preserved when they navigate back to
  // a regular view.
  const sidebarVisible = open && !isFullPane;

  // Track whether the panel collapse was triggered programmatically
  // (by a full-pane view) vs. by the user dragging it shut. We skip
  // updating `open` for programmatic collapses so the activity bar
  // indicator stays lit.
  const programmaticCollapseRef = useRef(false);

  // Persist whenever the user drags the divider. Only writes the
  // localStorage entry when the width is reasonably large — a tiny
  // sliver is almost certainly about to become "collapsed".
  const onResize = useCallback((size: number) => {
    if (size >= 8) {
      lastWidthRef.current = size;
      localStorage.setItem(LS_WIDTH_KEY, String(size));
    }
  }, []);

  useEffect(() => {
    localStorage.setItem(LS_VIEW_KEY, activeView);
  }, [activeView]);

  useEffect(() => {
    localStorage.setItem(LS_OPEN_KEY, open ? "1" : "0");
    const panel = sidebarPanelRef.current;
    if (!panel) return;
    if (sidebarVisible) {
      panel.resize(lastWidthRef.current);
    } else {
      // Mark this collapse as programmatic so the onCollapse callback
      // doesn't flip `open` to false (which would kill the indicator).
      programmaticCollapseRef.current = true;
      panel.collapse();
    }
  }, [open, sidebarVisible]);

  // Cmd-B / Ctrl-B toggle, matching VS Code's accelerator. Scoped
  // to window so it works regardless of focus. Cmd+, jumps to
  // Settings, also VS Code-native.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === "b") {
        e.preventDefault();
        setOpen((o) => !o);
      } else if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === ",") {
        e.preventDefault();
        setActiveView("settings");
        setOpen(true);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const onActivitySelect = (view: SidebarView) => {
    if (view === activeView) {
      setOpen((o) => !o);
    } else {
      setActiveView(view);
      setOpen(true);
    }
  };

  const activeDef = VIEWS.find((v) => v.id === activeView);
  const activeContent = views[activeView];

  return (
    <div className="flex h-full min-h-0 w-full">
      {showActivityBar && (
        <ActivityBar active={activeView} sidebarOpen={open} onSelect={onActivitySelect} hiddenViews={hiddenViews} badges={badges} />
      )}
      <PanelGroup direction="horizontal" className="flex min-h-0 flex-1">
        <Panel
          ref={sidebarPanelRef}
          defaultSize={sidebarVisible ? lastWidthRef.current : 0}
          minSize={sidebarVisible ? 12 : 0}
          maxSize={60}
          collapsible
          collapsedSize={0}
          onResize={onResize}
          onCollapse={() => {
            // Only update `open` if this was a user-initiated collapse
            // (dragging the handle). Programmatic collapses (full-pane
            // views hiding the sidebar) should not change `open` so the
            // activity bar indicator stays visible.
            if (programmaticCollapseRef.current) {
              programmaticCollapseRef.current = false;
            } else {
              setOpen(false);
            }
          }}
          onExpand={() => setOpen(true)}
          className={clsx(
            "flex min-h-0 flex-col bg-gx-surface",
            !sidebarVisible && "hidden",
          )}
        >
          <div className="flex shrink-0 items-center justify-between border-b border-gx-border px-3 py-2 text-[11px] uppercase tracking-wider text-gx-muted">
            <span>{activeDef?.label ?? "Sidebar"}</span>
          </div>
          <div className="flex min-h-0 flex-1 flex-col">
            {activeContent ?? (
              <div className="p-3 text-xs text-gx-muted">Coming soon.</div>
            )}
          </div>
        </Panel>
        <PanelResizeHandle
          className={clsx(
            "w-[3px] cursor-col-resize bg-transparent transition-colors hover:bg-gx-accent/50",
            !sidebarVisible && "pointer-events-none w-0",
          )}
        />
        <Panel minSize={20} className="min-h-0 min-w-0">
          {main}
        </Panel>
      </PanelGroup>
    </div>
  );
}
