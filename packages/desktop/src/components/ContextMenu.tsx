import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import type { LucideIcon } from "lucide-react";
import clsx from "clsx";

export interface MenuItemDef {
  id: string;
  label: string;
  icon?: LucideIcon;
  shortcut?: string;
  disabled?: boolean;
  /** When true, renders a visual separator line before this item. */
  separator?: boolean;
  /** Danger styling (red text) for destructive actions. */
  danger?: boolean;
}

export interface ContextMenuProps {
  /** Menu items to render. */
  items: MenuItemDef[];
  /** Screen position where the menu should appear. */
  position: { x: number; y: number };
  /** Called when a menu item is selected. */
  onSelect: (itemId: string) => void;
  /** Called when the menu should close (click outside, Escape, item selected). */
  onClose: () => void;
}

/**
 * Generic right-click context menu component. Renders a positioned popup
 * at the given (x, y) with viewport edge detection, keyboard navigation,
 * and full ARIA support.
 */
export function ContextMenu({ items, position, onSelect, onClose }: ContextMenuProps) {
  const menuRef = useRef<HTMLDivElement>(null);
  const [highlightIndex, setHighlightIndex] = useState(-1);
  const [adjustedPos, setAdjustedPos] = useState(position);
  const triggerRef = useRef<Element | null>(null);

  // Capture the element that was focused before the menu opened so we
  // can restore focus on close.
  useEffect(() => {
    triggerRef.current = document.activeElement;
  }, []);

  // Viewport edge detection: flip the menu up/left if it would overflow.
  useLayoutEffect(() => {
    const el = menuRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const vw = window.innerWidth;
    const vh = window.innerHeight;

    let x = position.x;
    let y = position.y;

    if (x + rect.width > vw) {
      x = Math.max(0, x - rect.width);
    }
    if (y + rect.height > vh) {
      y = Math.max(0, y - rect.height);
    }

    setAdjustedPos({ x, y });
  }, [position]);

  // Focus the menu container on mount for keyboard navigation.
  useEffect(() => {
    menuRef.current?.focus();
  }, []);

  // Close on window blur.
  useEffect(() => {
    const handleBlur = () => onClose();
    window.addEventListener("blur", handleBlur);
    return () => window.removeEventListener("blur", handleBlur);
  }, [onClose]);

  // Close on click outside.
  useEffect(() => {
    const handleMouseDown = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) {
        onClose();
      }
    };
    document.addEventListener("mousedown", handleMouseDown);
    return () => document.removeEventListener("mousedown", handleMouseDown);
  }, [onClose]);

  // Restore focus to the trigger element on unmount.
  useEffect(() => {
    return () => {
      if (triggerRef.current && triggerRef.current instanceof HTMLElement) {
        triggerRef.current.focus();
      }
    };
  }, []);

  // Get indices of selectable (non-disabled) items.
  const selectableIndices = items.reduce<number[]>((acc, item, i) => {
    if (!item.disabled) acc.push(i);
    return acc;
  }, []);

  const handleSelect = useCallback(
    (itemId: string) => {
      onSelect(itemId);
      onClose();
    },
    [onSelect, onClose],
  );

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      switch (e.key) {
        case "ArrowDown": {
          e.preventDefault();
          e.stopPropagation();
          const currentPos = selectableIndices.indexOf(highlightIndex);
          const nextPos = currentPos < selectableIndices.length - 1 ? currentPos + 1 : 0;
          setHighlightIndex(selectableIndices[nextPos] ?? -1);
          break;
        }
        case "ArrowUp": {
          e.preventDefault();
          e.stopPropagation();
          const currentPos = selectableIndices.indexOf(highlightIndex);
          const prevPos = currentPos > 0 ? currentPos - 1 : selectableIndices.length - 1;
          setHighlightIndex(selectableIndices[prevPos] ?? -1);
          break;
        }
        case "Enter": {
          e.preventDefault();
          e.stopPropagation();
          if (highlightIndex >= 0 && items[highlightIndex] && !items[highlightIndex].disabled) {
            handleSelect(items[highlightIndex].id);
          }
          break;
        }
        case "Escape": {
          e.preventDefault();
          e.stopPropagation();
          onClose();
          break;
        }
        case "Tab": {
          // Trap focus within the menu.
          e.preventDefault();
          break;
        }
      }
    },
    [highlightIndex, selectableIndices, items, handleSelect, onClose],
  );

  return (
    <div
      ref={menuRef}
      role="menu"
      tabIndex={-1}
      className="fixed z-[9999] min-w-[180px] overflow-hidden rounded-md border border-border bg-card p-1 shadow-2xl outline-none"
      style={{ left: adjustedPos.x, top: adjustedPos.y }}
      onKeyDown={handleKeyDown}
    >
      {items.map((item, index) => {
        if (item.separator) {
          return (
            <div key={`${item.id}-group`}>
              <hr className="my-1 border-t border-border/60" />
              <MenuItemRow
                item={item}
                highlighted={index === highlightIndex}
                onSelect={handleSelect}
                onHover={() => !item.disabled && setHighlightIndex(index)}
              />
            </div>
          );
        }
        return (
          <MenuItemRow
            key={item.id}
            item={item}
            highlighted={index === highlightIndex}
            onSelect={handleSelect}
            onHover={() => !item.disabled && setHighlightIndex(index)}
          />
        );
      })}
    </div>
  );
}

function MenuItemRow({
  item,
  highlighted,
  onSelect,
  onHover,
}: {
  item: MenuItemDef;
  highlighted: boolean;
  onSelect: (id: string) => void;
  onHover: () => void;
}) {
  const Icon = item.icon;

  return (
    <button
      type="button"
      role="menuitem"
      aria-disabled={item.disabled || undefined}
      className={clsx(
        "flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-[12.5px] transition-colors",
        item.disabled && "opacity-50 pointer-events-none",
        item.danger && !item.disabled && "text-destructive",
        !item.danger && !item.disabled && "text-foreground",
        highlighted && !item.disabled && "bg-primary/15",
        !highlighted && !item.disabled && "hover:bg-accent",
      )}
      onClick={() => {
        if (!item.disabled) onSelect(item.id);
      }}
      onMouseEnter={onHover}
      tabIndex={-1}
    >
      {Icon && (
        <span className="shrink-0 text-muted-foreground">
          <Icon size={14} />
        </span>
      )}
      <span className="flex-1 truncate">{item.label}</span>
      {item.shortcut && (
        <span className="ml-4 shrink-0 text-[11px] text-muted-foreground">{item.shortcut}</span>
      )}
    </button>
  );
}
