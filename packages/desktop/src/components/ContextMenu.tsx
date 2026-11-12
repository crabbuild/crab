import {
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuShortcut,
  PointMenu,
} from "@/components/ui/dropdown-menu";
import type { LucideIcon } from "lucide-react";
import { Fragment } from "react";

export interface MenuItemDef {
  id: string;
  label: string;
  icon?: LucideIcon;
  shortcut?: string;
  disabled?: boolean;
  separator?: boolean;
  danger?: boolean;
}

export interface ContextMenuProps {
  items: MenuItemDef[];
  position: { x: number; y: number };
  onSelect: (itemId: string) => void;
  onClose: () => void;
}

export function ContextMenu({ items, position, onSelect, onClose }: ContextMenuProps) {
  return (
    <PointMenu position={position} onClose={onClose}
    >
      {items.map(({ id, label, icon: Icon, shortcut, disabled, separator, danger }) => (
          <Fragment key={id}>
            {separator && <DropdownMenuSeparator />}
            <DropdownMenuItem
              disabled={disabled}
              className={
                danger ? "text-destructive focus:text-destructive" : undefined}
              onSelect={() => onSelect(id)}
    >
      {Icon && <Icon className="size-3.5" />}
              <span>{label}</span>
              {shortcut && (
        <DropdownMenuShortcut>{shortcut}</DropdownMenuShortcut>
              )}
            </DropdownMenuItem>
          </Fragment>
        ),
      )}
    </PointMenu>
  );
}
