import { useEffect } from "react";
import { useWorkspaceTheme } from "../hooks/useWorkspaceTheme";

/**
 * Side-effect-only component that overrides the `<html>` theme class
 * when a per-workspace theme is active. Mounts inside `Shell` where
 * both `StoreProvider` and `SettingsProvider` are available.
 *
 * Precedence: workspace theme > user theme > "dark" default.
 * The user-level theme class is applied by `SettingsProvider`; this
 * component re-applies when a workspace override exists.
 */
export function WorkspaceThemeEffect() {
  const { workspaceTheme } = useWorkspaceTheme();

  useEffect(() => {
    if (!workspaceTheme) return;

    const root = document.documentElement;
    const apply = () => {
      root.classList.remove("theme-dark", "theme-light", "theme-hc-dark", "theme-hc-light", "dark");
      if (workspaceTheme === "system") {
        const mq = window.matchMedia("(prefers-color-scheme: light)");
        const isDark = !mq.matches;
        root.classList.add(isDark ? "theme-dark" : "theme-light");
        if (isDark) root.classList.add("dark");
      } else {
        root.classList.add(`theme-${workspaceTheme}`);
        const isDark = workspaceTheme === "dark" || workspaceTheme === "hc-dark";
        if (isDark) root.classList.add("dark");
      }
    };

    apply();

    if (workspaceTheme === "system") {
      const mq = window.matchMedia("(prefers-color-scheme: light)");
      const handler = () => apply();
      mq.addEventListener("change", handler);
      return () => mq.removeEventListener("change", handler);
    }
    return undefined;
  }, [workspaceTheme]);

  return null;
}
