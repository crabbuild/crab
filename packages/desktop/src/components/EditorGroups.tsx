import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import clsx from "clsx";
import { GitCompareArrows, Pin, X } from "lucide-react";
import { PanelGroup, Panel, PanelResizeHandle } from "react-resizable-panels";
import { FileIcon } from "./FileIcon";
import { publishToast } from "../lib/toasts";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface EditorTab {
  id: string;
  title: string;
  kind: "file" | "diff" | "preview" | "view";
  path?: string;
  closable?: boolean;
  /** Optional icon override — defaults to FileIcon for "file" kind. */
  icon?: React.ReactNode;
  /** Whether this tab is pinned (pinned tabs sort to the left and resist close-all). */
  pinned?: boolean;
}

export interface EditorGroupState {
  tabs: EditorTab[];
  activeTabId: string | null;
}

interface EditorGroupProps {
  /** The group state (tabs + active). */
  group: EditorGroupState;
  /** Called when the user clicks a tab. */
  onActivate: (tabId: string) => void;
  /** Called when the user closes a tab (X button or middle-click). */
  onClose: (tabId: string) => void;
  /** Called when a tab is reordered within this group. */
  onReorder?: (tabId: string, newIndex: number) => void;
  /** Renders the content for the currently active tab. */
  children: React.ReactNode;
  /** Called when a tab is dropped on the split zone. */
  onSplitDrop?: (sourceGroupIndex: number, tabId: string) => void;
  /** Index of this group (used for drag data). */
  groupIndex?: number;
  /** Whether to show the split drop zone (hidden when max groups reached). */
  showSplitZone?: boolean;
  /** Called to pin/unpin a tab. */
  onPin?: (tabId: string, pinned: boolean) => void;
  /** Called to close all tabs in this group. */
  onCloseAll?: () => void;
  /** Called to close all tabs except the given one. */
  onCloseOthers?: (tabId: string) => void;
  /** Called to close all tabs to the right of the given one. */
  onCloseToRight?: (tabId: string) => void;
  /** Called to split a specific tab into a new group. */
  onSplitTab?: (tabId: string) => void;
}

interface EditorGroupsProps {
  /** Array of groups — one per split pane. */
  groups: EditorGroupState[];
  /** Called when the user clicks a tab in any group. */
  onActivate: (groupIndex: number, tabId: string) => void;
  /** Called when the user closes a tab in any group. */
  onClose: (groupIndex: number, tabId: string) => void;
  /** Called when a tab is reordered within a group. */
  onReorder?: (groupIndex: number, tabId: string, newIndex: number) => void;
  /** Render function for the content of a given group's active tab. */
  renderContent: (groupIndex: number, activeTabId: string | null) => React.ReactNode;
  /** Called when a tab is dragged to the split zone to create a new group. */
  onSplitDrop?: (sourceGroupIndex: number, tabId: string) => void;
  /** Maximum number of groups allowed (default: 2). */
  maxGroups?: number;
  /** Called to pin/unpin a tab. */
  onPin?: (groupIndex: number, tabId: string, pinned: boolean) => void;
  /** Called to close all tabs in a group. */
  onCloseAll?: (groupIndex: number) => void;
  /** Called to close all tabs except the given one. */
  onCloseOthers?: (groupIndex: number, tabId: string) => void;
  /** Called to close all tabs to the right of the given one. */
  onCloseToRight?: (groupIndex: number, tabId: string) => void;
  /** Called to split a specific tab into a new group (via context menu). */
  onSplitTab?: (groupIndex: number, tabId: string) => void;
}

// ---------------------------------------------------------------------------
// Drag-and-drop constants
// ---------------------------------------------------------------------------

const DRAG_TYPE_TAB = "application/x-gx-tab";

/** Encode tab drag data into the dataTransfer. */
function setTabDragData(
  dataTransfer: DataTransfer,
  tabId: string,
  groupIndex: number,
) {
  dataTransfer.setData(DRAG_TYPE_TAB, JSON.stringify({ tabId, groupIndex }));
  dataTransfer.effectAllowed = "move";
}

/** Decode tab drag data from the dataTransfer. Returns null if not a tab drag. */
function getTabDragData(
  dataTransfer: DataTransfer,
): { tabId: string; groupIndex: number } | null {
  const raw = dataTransfer.getData(DRAG_TYPE_TAB);
  if (!raw) return null;
  try {
    return JSON.parse(raw);
  } catch {
    return null;
  }
}

/** Check if the current drag contains tab data (for dragOver). */
function hasTabDragData(dataTransfer: DataTransfer): boolean {
  return dataTransfer.types.includes(DRAG_TYPE_TAB);
}

// ---------------------------------------------------------------------------
// EditorGroups — top-level container supporting split panes via
// react-resizable-panels. Drag-to-split creates sibling panels.
// ---------------------------------------------------------------------------

export function EditorGroups({
  groups,
  onActivate,
  onClose,
  onReorder,
  renderContent,
  onSplitDrop,
  maxGroups = 2,
  onPin,
  onCloseAll,
  onCloseOthers,
  onCloseToRight,
  onSplitTab,
}: EditorGroupsProps) {
  if (groups.length === 0) {
    return <div className="h-full bg-background" />;
  }

  const canSplit = groups.length < maxGroups;

  // Single group — no split, no resize handles needed.
  if (groups.length === 1) {
    return (
      <EditorGroup
        group={groups[0]}
        groupIndex={0}
        onActivate={(tabId) => onActivate(0, tabId)}
        onClose={(tabId) => onClose(0, tabId)}
        onReorder={onReorder ? (tabId, newIndex) => onReorder(0, tabId, newIndex) : undefined}
        onSplitDrop={onSplitDrop}
        showSplitZone={canSplit}
        onPin={onPin ? (tabId, pinned) => onPin(0, tabId, pinned) : undefined}
        onCloseAll={onCloseAll ? () => onCloseAll(0) : undefined}
        onCloseOthers={onCloseOthers ? (tabId) => onCloseOthers(0, tabId) : undefined}
        onCloseToRight={onCloseToRight ? (tabId) => onCloseToRight(0, tabId) : undefined}
        onSplitTab={onSplitTab ? (tabId) => onSplitTab(0, tabId) : undefined}
      >
        {renderContent(0, groups[0].activeTabId)}
      </EditorGroup>
    );
  }

  // Multiple groups — render inside a PanelGroup for resizable splits.
  return (
    <PanelGroup direction="horizontal" className="h-full">
      {groups.map((group, idx) => (
        <GroupPanel
          key={idx}
          group={group}
          groupIndex={idx}
          isLast={idx === groups.length - 1}
          onActivate={onActivate}
          onClose={onClose}
          onReorder={onReorder}
          renderContent={renderContent}
          onSplitDrop={onSplitDrop}
          showSplitZone={canSplit}
          onPin={onPin}
          onCloseAll={onCloseAll}
          onCloseOthers={onCloseOthers}
          onCloseToRight={onCloseToRight}
          onSplitTab={onSplitTab}
        />
      ))}
    </PanelGroup>
  );
}

/** Helper to render a single panel + optional resize handle. */
function GroupPanel({
  group,
  groupIndex,
  isLast,
  onActivate,
  onClose,
  onReorder,
  renderContent,
  onSplitDrop,
  showSplitZone,
  onPin,
  onCloseAll,
  onCloseOthers,
  onCloseToRight,
  onSplitTab,
}: {
  group: EditorGroupState;
  groupIndex: number;
  isLast: boolean;
  onActivate: (groupIndex: number, tabId: string) => void;
  onClose: (groupIndex: number, tabId: string) => void;
  onReorder?: (groupIndex: number, tabId: string, newIndex: number) => void;
  renderContent: (groupIndex: number, activeTabId: string | null) => React.ReactNode;
  onSplitDrop?: (sourceGroupIndex: number, tabId: string) => void;
  showSplitZone: boolean;
  onPin?: (groupIndex: number, tabId: string, pinned: boolean) => void;
  onCloseAll?: (groupIndex: number) => void;
  onCloseOthers?: (groupIndex: number, tabId: string) => void;
  onCloseToRight?: (groupIndex: number, tabId: string) => void;
  onSplitTab?: (groupIndex: number, tabId: string) => void;
}) {
  return (
    <>
      <Panel minSize={20}>
        <EditorGroup
          group={group}
          groupIndex={groupIndex}
          onActivate={(tabId) => onActivate(groupIndex, tabId)}
          onClose={(tabId) => onClose(groupIndex, tabId)}
          onReorder={onReorder ? (tabId, newIndex) => onReorder(groupIndex, tabId, newIndex) : undefined}
          onSplitDrop={onSplitDrop}
          showSplitZone={showSplitZone}
          onPin={onPin ? (tabId, pinned) => onPin(groupIndex, tabId, pinned) : undefined}
          onCloseAll={onCloseAll ? () => onCloseAll(groupIndex) : undefined}
          onCloseOthers={onCloseOthers ? (tabId) => onCloseOthers(groupIndex, tabId) : undefined}
          onCloseToRight={onCloseToRight ? (tabId) => onCloseToRight(groupIndex, tabId) : undefined}
          onSplitTab={onSplitTab ? (tabId) => onSplitTab(groupIndex, tabId) : undefined}
        >
          {renderContent(groupIndex, group.activeTabId)}
        </EditorGroup>
      </Panel>
      {!isLast && (
        <PanelResizeHandle className="w-1 bg-border hover:bg-primary transition-colors" />
      )}
    </>
  );
}

// ---------------------------------------------------------------------------
// EditorGroup — a single tab group with its tab bar + content area.
// Includes a split drop zone on the right edge.
// ---------------------------------------------------------------------------

function EditorGroup({
  group,
  onActivate,
  onClose,
  onReorder,
  children,
  onSplitDrop,
  groupIndex = 0,
  showSplitZone = false,
  onPin,
  onCloseAll,
  onCloseOthers,
  onCloseToRight,
  onSplitTab,
}: EditorGroupProps) {
  const { tabs, activeTabId } = group;

  if (tabs.length === 0) {
    return (
      <div className="flex h-full items-center justify-center bg-background">
        <div className="text-sm text-muted-foreground">No tabs open</div>
      </div>
    );
  }

  return (
    <div className="relative flex h-full min-h-0 flex-col bg-background">
      {/* Tab bar */}
      <TabBar
        tabs={tabs}
        activeTabId={activeTabId}
        onActivate={onActivate}
        onClose={onClose}
        onReorder={onReorder}
        groupIndex={groupIndex}
        onPin={onPin}
        onCloseAll={onCloseAll}
        onCloseOthers={onCloseOthers}
        onCloseToRight={onCloseToRight}
        onSplitTab={onSplitTab}
      />
      {/* Content area */}
      <div className="min-h-0 flex-1 overflow-hidden">
        {children}
      </div>
      {/* Split drop zone on the right edge */}
      {showSplitZone && onSplitDrop && (
        <SplitDropZone onSplitDrop={onSplitDrop} />
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// SplitDropZone — appears on the right edge when a tab is being dragged.
// Dropping a tab here triggers a split.
// ---------------------------------------------------------------------------

function SplitDropZone({
  onSplitDrop,
}: {
  onSplitDrop: (sourceGroupIndex: number, tabId: string) => void;
}) {
  const [isDragOver, setIsDragOver] = useState(false);
  const [isTabDragging, setIsTabDragging] = useState(false);

  const handleDragEnter = useCallback((e: React.DragEvent) => {
    if (hasTabDragData(e.dataTransfer)) {
      e.preventDefault();
      setIsDragOver(true);
    }
  }, []);

  const handleDragOver = useCallback((e: React.DragEvent) => {
    if (hasTabDragData(e.dataTransfer)) {
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";
      setIsDragOver(true);
      setIsTabDragging(true);
    }
  }, []);

  const handleDragLeave = useCallback((e: React.DragEvent) => {
    // Only reset if leaving the drop zone entirely (not entering a child).
    if (!e.currentTarget.contains(e.relatedTarget as Node)) {
      setIsDragOver(false);
    }
  }, []);

  const handleDrop = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      setIsDragOver(false);
      setIsTabDragging(false);
      const data = getTabDragData(e.dataTransfer);
      if (data) {
        onSplitDrop(data.groupIndex, data.tabId);
      }
    },
    [onSplitDrop],
  );

  // Listen for global dragstart/dragend to show/hide the zone.
  // We use the dragOver state as a proxy — the zone is always mounted
  // but only visible when a tab drag is in progress.
  const handleGlobalDragOver = useCallback((e: React.DragEvent) => {
    if (hasTabDragData(e.dataTransfer)) {
      setIsTabDragging(true);
    }
  }, []);

  return (
    <div
      className={clsx(
        "absolute right-0 top-0 bottom-0 z-10 flex items-center justify-center transition-all duration-150",
        isTabDragging || isDragOver ? "w-10 opacity-100" : "w-4 opacity-0",
        isDragOver
          ? "bg-primary/20 border-l-2 border-primary"
          : "bg-transparent border-l border-transparent",
      )}
      onDragEnter={handleDragEnter}
      onDragOver={(e) => {
        handleDragOver(e);
        handleGlobalDragOver(e);
      }}
      onDragLeave={handleDragLeave}
      onDrop={handleDrop}
      aria-label="Drop here to split editor"
    >
      {isDragOver && (
        <div className="h-8 w-[2px] rounded bg-primary" />
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// TabBar — horizontal scrollable bar of tabs with drag-to-reorder support.
// ---------------------------------------------------------------------------

function TabBar({
  tabs,
  activeTabId,
  onActivate,
  onClose,
  onReorder,
  groupIndex,
  onPin,
  onCloseAll,
  onCloseOthers,
  onCloseToRight,
  onSplitTab,
}: {
  tabs: EditorTab[];
  activeTabId: string | null;
  onActivate: (tabId: string) => void;
  onClose: (tabId: string) => void;
  onReorder?: (tabId: string, newIndex: number) => void;
  groupIndex: number;
  onPin?: (tabId: string, pinned: boolean) => void;
  onCloseAll?: () => void;
  onCloseOthers?: (tabId: string) => void;
  onCloseToRight?: (tabId: string) => void;
  onSplitTab?: (tabId: string) => void;
}) {
  // Track which tab index the dragged tab would be inserted before.
  // null means no indicator shown.
  const [dropIndicatorIndex, setDropIndicatorIndex] = useState<number | null>(null);

  const handleTabDragOver = useCallback(
    (e: React.DragEvent, targetIndex: number) => {
      if (!hasTabDragData(e.dataTransfer)) return;
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";

      // Determine if the cursor is on the left or right half of the target tab.
      const rect = e.currentTarget.getBoundingClientRect();
      const midX = rect.left + rect.width / 2;
      const insertIndex = e.clientX < midX ? targetIndex : targetIndex + 1;
      setDropIndicatorIndex(insertIndex);
    },
    [],
  );

  const handleTabDrop = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault();
      const data = getTabDragData(e.dataTransfer);
      if (!data || !onReorder) {
        setDropIndicatorIndex(null);
        return;
      }

      // Only reorder within the same group.
      if (data.groupIndex === groupIndex && dropIndicatorIndex != null) {
        const currentIndex = tabs.findIndex((t) => t.id === data.tabId);
        if (currentIndex !== -1) {
          // Adjust target index: if dragging forward, account for removal.
          let targetIndex = dropIndicatorIndex;
          if (currentIndex < targetIndex) {
            targetIndex -= 1;
          }
          if (targetIndex !== currentIndex) {
            onReorder(data.tabId, targetIndex);
          }
        }
      }
      setDropIndicatorIndex(null);
    },
    [onReorder, groupIndex, tabs, dropIndicatorIndex],
  );

  const handleTabDragLeave = useCallback((e: React.DragEvent) => {
    // Only clear if leaving the tab bar entirely.
    if (!e.currentTarget.contains(e.relatedTarget as Node)) {
      setDropIndicatorIndex(null);
    }
  }, []);

  const handleDragEnd = useCallback(() => {
    setDropIndicatorIndex(null);
  }, []);

  return (
    <div
      className="flex shrink-0 items-center overflow-x-auto scrollbar-none border-b border-border bg-card"
      role="tablist"
      onDragLeave={handleTabDragLeave}
      onDrop={handleTabDrop}
      onDragEnd={handleDragEnd}
    >
      {tabs.map((tab, index) => (
        <Tab
          key={tab.id}
          tab={tab}
          index={index}
          active={tab.id === activeTabId}
          onActivate={() => onActivate(tab.id)}
          onClose={() => onClose(tab.id)}
          groupIndex={groupIndex}
          onDragOver={handleTabDragOver}
          dropIndicatorIndex={dropIndicatorIndex}
          onPin={onPin ? (pinned) => onPin(tab.id, pinned) : undefined}
          onCloseAll={onCloseAll}
          onCloseOthers={onCloseOthers ? () => onCloseOthers(tab.id) : undefined}
          onCloseToRight={onCloseToRight ? () => onCloseToRight(tab.id) : undefined}
          onSplitTab={onSplitTab ? () => onSplitTab(tab.id) : undefined}
          totalTabs={tabs.length}
          isLastTab={index === tabs.length - 1}
        />
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Tab — individual tab pill with icon, title, close button.
// Draggable for split and reorder interactions. Right-click context menu.
// ---------------------------------------------------------------------------

function Tab({
  tab,
  index,
  active,
  onActivate,
  onClose,
  groupIndex,
  onDragOver,
  dropIndicatorIndex,
  onPin,
  onCloseAll,
  onCloseOthers,
  onCloseToRight,
  onSplitTab,
  totalTabs,
  isLastTab,
}: {
  tab: EditorTab;
  index: number;
  active: boolean;
  onActivate: () => void;
  onClose: () => void;
  groupIndex: number;
  onDragOver: (e: React.DragEvent, targetIndex: number) => void;
  dropIndicatorIndex: number | null;
  onPin?: (pinned: boolean) => void;
  onCloseAll?: () => void;
  onCloseOthers?: () => void;
  onCloseToRight?: () => void;
  onSplitTab?: () => void;
  totalTabs: number;
  isLastTab: boolean;
}) {
  const closable = tab.closable !== false;
  const [contextMenu, setContextMenu] = useState<{ x: number; y: number } | null>(null);

  const icon = useMemo(() => {
    if (tab.icon) return tab.icon;
    if (tab.kind === "diff") {
      return <GitCompareArrows size={13} className="text-primary" />;
    }
    if (tab.kind === "file" && tab.path) {
      return <FileIcon path={tab.path} kind="file" />;
    }
    return null;
  }, [tab.icon, tab.kind, tab.path]);

  const handleMouseDown = useCallback(
    (e: React.MouseEvent) => {
      // Middle-click closes the tab.
      if (e.button === 1 && closable) {
        e.preventDefault();
        onClose();
      }
    },
    [closable, onClose],
  );

  const handleContextMenu = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setContextMenu({ x: e.clientX, y: e.clientY });
  }, []);

  const handleDragStart = useCallback(
    (e: React.DragEvent) => {
      setTabDragData(e.dataTransfer, tab.id, groupIndex);
    },
    [tab.id, groupIndex],
  );

  const handleDragOver = useCallback(
    (e: React.DragEvent) => {
      onDragOver(e, index);
    },
    [onDragOver, index],
  );

  // Show a drop indicator on the left edge of this tab when the
  // dropIndicatorIndex matches this tab's index.
  const showLeftIndicator = dropIndicatorIndex === index;

  return (
    <>
      <div
        className={clsx(
          "group relative flex h-9 shrink-0 items-center gap-2 border-r border-border px-3 text-[12px] transition select-none cursor-grab active:cursor-grabbing",
          active
            ? "bg-background text-foreground"
            : "text-muted-foreground hover:bg-accent",
          tab.pinned && "italic",
        )}
        onMouseDown={handleMouseDown}
        onContextMenu={handleContextMenu}
        draggable
        onDragStart={handleDragStart}
        onDragOver={handleDragOver}
        role="tab"
        aria-selected={active}
        tabIndex={active ? 0 : -1}
      >
        {/* Drop indicator — vertical accent line on the left edge */}
        {showLeftIndicator && (
          <div className="absolute left-0 top-1 bottom-1 w-[2px] rounded bg-primary z-10" />
        )}

        {/* Active tab accent border */}
        {active && (
          <div className="absolute inset-x-0 bottom-0 h-[2px] bg-primary" />
        )}

        {/* Pin indicator */}
        {tab.pinned && (
          <Pin size={10} className="shrink-0 text-primary rotate-45" />
        )}

        <button
          type="button"
          onClick={onActivate}
          className="flex items-center gap-2 truncate"
          aria-label={`Switch to ${tab.title}`}
        >
          {icon}
          <span className="truncate max-w-[140px]">{tab.title}</span>
        </button>

        {closable && !tab.pinned && (
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onClose();
            }}
            className="ml-1 opacity-0 transition group-hover:opacity-100 hover:text-foreground"
            aria-label={`Close ${tab.title}`}
            title="Close"
          >
            <X size={12} />
          </button>
        )}
      </div>

      {/* Context menu */}
      {contextMenu && (
        <TabContextMenu
          x={contextMenu.x}
          y={contextMenu.y}
          tab={tab}
          closable={closable}
          totalTabs={totalTabs}
          isLastTab={isLastTab}
          onClose={() => {
            setContextMenu(null);
            onClose();
          }}
          onCloseAll={onCloseAll ? () => { setContextMenu(null); onCloseAll(); } : undefined}
          onCloseOthers={onCloseOthers ? () => { setContextMenu(null); onCloseOthers(); } : undefined}
          onCloseToRight={onCloseToRight ? () => { setContextMenu(null); onCloseToRight(); } : undefined}
          onPin={onPin ? (pinned) => { setContextMenu(null); onPin(pinned); } : undefined}
          onSplitTab={onSplitTab ? () => { setContextMenu(null); onSplitTab(); } : undefined}
          onDismiss={() => setContextMenu(null)}
        />
      )}
    </>
  );
}

// ---------------------------------------------------------------------------
// TabContextMenu — VS Code-style right-click context menu for tabs.
// ---------------------------------------------------------------------------

function TabContextMenu({
  x,
  y,
  tab,
  closable,
  totalTabs,
  isLastTab,
  onClose,
  onCloseAll,
  onCloseOthers,
  onCloseToRight,
  onPin,
  onSplitTab,
  onDismiss,
}: {
  x: number;
  y: number;
  tab: EditorTab;
  closable: boolean;
  totalTabs: number;
  isLastTab: boolean;
  onClose: () => void;
  onCloseAll?: () => void;
  onCloseOthers?: () => void;
  onCloseToRight?: () => void;
  onPin?: (pinned: boolean) => void;
  onSplitTab?: () => void;
  onDismiss: () => void;
}) {
  const menuRef = useRef<HTMLDivElement>(null);

  // Close on click outside or Escape.
  useEffect(() => {
    const handleClickOutside = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        onDismiss();
      }
    };
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") onDismiss();
    };
    document.addEventListener("mousedown", handleClickOutside);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("mousedown", handleClickOutside);
      document.removeEventListener("keydown", handleKeyDown);
    };
  }, [onDismiss]);

  // Adjust position so the menu doesn't overflow the viewport.
  const [pos, setPos] = useState({ x, y });
  useEffect(() => {
    if (!menuRef.current) return;
    const rect = menuRef.current.getBoundingClientRect();
    let adjustedX = x;
    let adjustedY = y;
    if (x + rect.width > window.innerWidth) {
      adjustedX = window.innerWidth - rect.width - 4;
    }
    if (y + rect.height > window.innerHeight) {
      adjustedY = window.innerHeight - rect.height - 4;
    }
    setPos({ x: adjustedX, y: adjustedY });
  }, [x, y]);

  const handleCopyPath = useCallback(() => {
    if (tab.path) {
      void navigator.clipboard.writeText(tab.path);
    }
    onDismiss();
  }, [tab.path, onDismiss]);

  const handleCopyRelativePath = useCallback(() => {
    if (tab.path) {
      void navigator.clipboard.writeText(tab.path);
    }
    onDismiss();
  }, [tab.path, onDismiss]);

  return (
    <div
      ref={menuRef}
      className="fixed z-50 min-w-[220px] rounded-md border border-border bg-card py-1 shadow-lg text-[12px]"
      style={{ left: pos.x, top: pos.y }}
      role="menu"
    >
      {/* --- Close group --- */}
      {closable && (
        <ContextMenuItem label="Close" shortcut="⌘W" onClick={onClose} />
      )}
      {onCloseOthers && totalTabs > 1 && (
        <ContextMenuItem label="Close Others" shortcut="⌥⌘T" onClick={onCloseOthers} />
      )}
      {onCloseToRight && !isLastTab && (
        <ContextMenuItem label="Close to the Right" onClick={onCloseToRight} />
      )}
      {onCloseAll && (
        <ContextMenuItem label="Close All" shortcut="⌘K W" onClick={onCloseAll} />
      )}

      {/* --- Separator --- */}
      {tab.path && (
        <div className="my-1 border-t border-border" role="separator" />
      )}

      {/* --- Copy path group --- */}
      {tab.path && (
        <ContextMenuItem label="Copy Path" shortcut="⇧⌘C" onClick={handleCopyPath} />
      )}
      {tab.path && (
        <ContextMenuItem label="Copy Relative Path" shortcut="⌥⇧⌘C" onClick={handleCopyRelativePath} />
      )}

      {/* --- Separator --- */}
      {(onPin || onSplitTab) && (
        <div className="my-1 border-t border-border" role="separator" />
      )}

      {/* Pin / Unpin */}
      {onPin && (
        <ContextMenuItem
          label={tab.pinned ? "Unpin" : "Pin"}
          shortcut="⌘K ⇧Enter"
          onClick={() => onPin(!tab.pinned)}
        />
      )}

      {/* --- Separator --- */}
      {onSplitTab && (
        <div className="my-1 border-t border-border" role="separator" />
      )}

      {/* Split Right */}
      {onSplitTab && (
        <ContextMenuItem label="Split Right" shortcut="⌘\\" onClick={onSplitTab} />
      )}
    </div>
  );
}

function ContextMenuItem({
  label,
  shortcut,
  onClick,
  disabled,
}: {
  label: string;
  shortcut?: string;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="menuitem"
      disabled={disabled}
      onClick={onClick}
      className={clsx(
        "flex w-full items-center justify-between px-3 py-1.5 text-left transition",
        disabled
          ? "text-muted-foreground cursor-not-allowed"
          : "text-foreground hover:bg-accent",
      )}
    >
      <span>{label}</span>
      {shortcut && <span className="ml-4 text-[10px] text-muted-foreground">{shortcut}</span>}
    </button>
  );
}

// ---------------------------------------------------------------------------
// Hook: useEditorGroups — manages multi-group tab state.
// Consumers can use this hook or manage state externally.
// ---------------------------------------------------------------------------

export function useEditorGroups(initialGroups?: EditorGroupState[], maxGroups = 2) {
  const [groups, setGroups] = useState<EditorGroupState[]>(
    initialGroups ?? [{ tabs: [], activeTabId: null }],
  );

  // Keep maxGroups in a ref so splitGroup's useCallback doesn't need it as a dep.
  const maxGroupsRef = useRef(maxGroups);
  maxGroupsRef.current = maxGroups;

  const activateTab = useCallback((groupIndex: number, tabId: string) => {
    setGroups((prev) =>
      prev.map((g, i) => (i === groupIndex ? { ...g, activeTabId: tabId } : g)),
    );
  }, []);

  const closeTab = useCallback((groupIndex: number, tabId: string) => {
    setGroups((prev) => {
      const updated = prev.map((g, i) => {
        if (i !== groupIndex) return g;
        const remaining = g.tabs.filter((t) => t.id !== tabId);
        let nextActive = g.activeTabId;
        if (g.activeTabId === tabId) {
          nextActive = remaining[remaining.length - 1]?.id ?? null;
        }
        return { tabs: remaining, activeTabId: nextActive };
      });
      // Remove empty groups (except keep at least one).
      const nonEmpty = updated.filter((g) => g.tabs.length > 0);
      return nonEmpty.length > 0 ? nonEmpty : [{ tabs: [], activeTabId: null }];
    });
  }, []);

  const openTab = useCallback(
    (groupIndex: number, tab: EditorTab, activate = true) => {
      setGroups((prev) =>
        prev.map((g, i) => {
          if (i !== groupIndex) return g;
          const exists = g.tabs.some((t) => t.id === tab.id);
          const tabs = exists ? g.tabs : [...g.tabs, tab];
          const activeTabId = activate ? tab.id : g.activeTabId;
          return { tabs, activeTabId };
        }),
      );
    },
    [],
  );

  /**
   * Moves a tab from the source group into a new sibling group,
   * creating a horizontal split. If the source group becomes empty
   * after the move, it is removed.
   *
   * Guarded by maxGroups — if the limit is already reached, a toast
   * is shown and the state is unchanged.
   */
  const splitGroup = useCallback(
    (sourceGroupIndex: number, tabId: string) => {
      setGroups((prev) => {
        if (prev.length >= maxGroupsRef.current) {
          publishToast({
            kind: "info",
            title: "Split limit reached",
            body: `Maximum ${maxGroupsRef.current} editor groups side by side.`,
          });
          return prev;
        }

        const source = prev[sourceGroupIndex];
        if (!source) return prev;

        const tab = source.tabs.find((t) => t.id === tabId);
        if (!tab) return prev;

        // Remove the tab from the source group.
        const remainingTabs = source.tabs.filter((t) => t.id !== tabId);
        let sourceActive = source.activeTabId;
        if (sourceActive === tabId) {
          sourceActive = remainingTabs[remainingTabs.length - 1]?.id ?? null;
        }

        const updatedSource: EditorGroupState = {
          tabs: remainingTabs,
          activeTabId: sourceActive,
        };

        // Create the new group with the dragged tab.
        const newGroup: EditorGroupState = {
          tabs: [tab],
          activeTabId: tab.id,
        };

        // Build the new groups array.
        const result: EditorGroupState[] = [];
        for (let i = 0; i < prev.length; i++) {
          if (i === sourceGroupIndex) {
            // Keep the source only if it still has tabs.
            if (updatedSource.tabs.length > 0) {
              result.push(updatedSource);
            }
          } else {
            result.push(prev[i]);
          }
        }
        // Append the new group at the end (right side).
        result.push(newGroup);

        return result;
      });
    },
    [],
  );

  /** Reorders a tab within a group to a new position. */
  const reorderTab = useCallback(
    (groupIndex: number, tabId: string, newIndex: number) => {
      setGroups((prev) =>
        prev.map((g, i) => {
          if (i !== groupIndex) return g;
          const currentIndex = g.tabs.findIndex((t) => t.id === tabId);
          if (currentIndex === -1 || currentIndex === newIndex) return g;
          const tabs = [...g.tabs];
          const [moved] = tabs.splice(currentIndex, 1);
          tabs.splice(newIndex, 0, moved);
          return { ...g, tabs };
        }),
      );
    },
    [],
  );

  /** Pins or unpins a tab. Pinned tabs sort to the left of unpinned tabs. */
  const pinTab = useCallback(
    (groupIndex: number, tabId: string, pinned: boolean) => {
      setGroups((prev) =>
        prev.map((g, i) => {
          if (i !== groupIndex) return g;
          const tabs = g.tabs.map((t) =>
            t.id === tabId ? { ...t, pinned } : t,
          );
          // Sort: pinned tabs first, preserving relative order within each group.
          const pinnedTabs = tabs.filter((t) => t.pinned);
          const unpinnedTabs = tabs.filter((t) => !t.pinned);
          return { ...g, tabs: [...pinnedTabs, ...unpinnedTabs] };
        }),
      );
    },
    [],
  );

  /** Closes all tabs in a group (respects pinned — pinned tabs stay). */
  const closeAllTabs = useCallback(
    (groupIndex: number) => {
      setGroups((prev) => {
        const updated = prev.map((g, i) => {
          if (i !== groupIndex) return g;
          const remaining = g.tabs.filter((t) => t.pinned);
          const nextActive = remaining[remaining.length - 1]?.id ?? null;
          return { tabs: remaining, activeTabId: nextActive };
        });
        const nonEmpty = updated.filter((g) => g.tabs.length > 0);
        return nonEmpty.length > 0 ? nonEmpty : [{ tabs: [], activeTabId: null }];
      });
    },
    [],
  );

  /** Closes all tabs except the given one (respects pinned — pinned tabs stay). */
  const closeOtherTabs = useCallback(
    (groupIndex: number, tabId: string) => {
      setGroups((prev) => {
        const updated = prev.map((g, i) => {
          if (i !== groupIndex) return g;
          const remaining = g.tabs.filter((t) => t.id === tabId || t.pinned);
          return { tabs: remaining, activeTabId: tabId };
        });
        const nonEmpty = updated.filter((g) => g.tabs.length > 0);
        return nonEmpty.length > 0 ? nonEmpty : [{ tabs: [], activeTabId: null }];
      });
    },
    [],
  );

  /** Closes all tabs to the right of the given one (respects pinned). */
  const closeTabsToRight = useCallback(
    (groupIndex: number, tabId: string) => {
      setGroups((prev) => {
        const updated = prev.map((g, i) => {
          if (i !== groupIndex) return g;
          const idx = g.tabs.findIndex((t) => t.id === tabId);
          if (idx === -1) return g;
          const remaining = g.tabs.filter(
            (t, tIdx) => tIdx <= idx || t.pinned,
          );
          let nextActive = g.activeTabId;
          if (nextActive && !remaining.some((t) => t.id === nextActive)) {
            nextActive = tabId;
          }
          return { tabs: remaining, activeTabId: nextActive };
        });
        const nonEmpty = updated.filter((g) => g.tabs.length > 0);
        return nonEmpty.length > 0 ? nonEmpty : [{ tabs: [], activeTabId: null }];
      });
    },
    [],
  );

  return {
    groups,
    setGroups,
    activateTab,
    closeTab,
    openTab,
    splitGroup,
    reorderTab,
    pinTab,
    closeAllTabs,
    closeOtherTabs,
    closeTabsToRight,
  };
}
