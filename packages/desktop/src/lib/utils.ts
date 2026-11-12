import { clsx, type ClassValue } from "clsx";
import type { KeyboardEventHandler } from "react";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

// Portalled controls sit outside native-input shortcut guards. Their keys
// belong to the control, so typeahead/navigation must not act on the workspace.
export function scopeControlKeys<T extends HTMLElement>(
  handler?: KeyboardEventHandler<T>,
): KeyboardEventHandler<T> {
  return (event) => {
    handler?.(event);
    event.stopPropagation();
  };
}

const pointMenuOwners = new WeakMap<HTMLElement, HTMLElement | null>();
export function rememberPointMenuOwner(
  trigger: HTMLElement,
  owner: HTMLElement | null,
) {
  pointMenuOwners.set(trigger, owner);
}

// A menu item disappears when it opens a dialog. Follow Radix's menu/trigger
// label relationship so closing that dialog returns to the surviving control.
export function controlFocusOwner(): HTMLElement | null {
  let owner = document.activeElement as HTMLElement | null;
  while (owner) {
    if (pointMenuOwners.has(owner)) return pointMenuOwners.get(owner) ?? null;
    const menu = owner.closest('[role="menu"]');
    const triggerId = menu?.getAttribute("aria-labelledby");
    const trigger = triggerId ? document.getElementById(triggerId) : null;
    if (!trigger || trigger === owner) return owner;
    owner = trigger;
  }
  return null;
}
