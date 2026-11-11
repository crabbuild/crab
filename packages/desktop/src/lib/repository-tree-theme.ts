import type { CSSProperties } from "react";
import { themeToTreeStyles } from "@pierre/trees";

export function repositoryTreeTheme(mode: "dark" | "light"): CSSProperties {
  // @pierre/trees emits these values as CSS variables. Keeping references
  // lets shadow-root trees follow live and high-contrast theme changes.
  const colors = {
    "sideBar.background": "rgb(var(--background))",
    "sideBar.foreground": "rgb(var(--foreground))",
    "sideBarSectionHeader.foreground": "rgb(var(--muted-foreground))",
    "sideBar.border": "rgb(var(--border))",
    "list.hoverBackground": "rgb(var(--accent))",
    "list.activeSelectionBackground": "rgb(var(--accent))",
    "list.activeSelectionForeground": "rgb(var(--accent-foreground))",
    focusBorder: "rgb(var(--ring))",
    "input.background": "rgb(var(--background))",
    "input.border": "rgb(var(--input))",
    "scrollbarSlider.background": "rgb(var(--border))",
    "gitDecoration.addedResourceForeground": "rgb(var(--success))",
    "gitDecoration.modifiedResourceForeground": "rgb(var(--warning))",
    "gitDecoration.deletedResourceForeground": "rgb(var(--destructive))",
  };
  return themeToTreeStyles({ type: mode, colors }) as CSSProperties;
}
